use std::{pin::Pin, sync::Arc};

use futures_core::Stream;
use next_proto::bottles::{
    common::v1::{AuthState, Storefront},
    library::v1::{
        GameEvent, InstallGameEvent, InstallGameRequest, InstallProgress,
        ListGamesRequest as LibraryListGamesRequest, ListGamesResponse as LibraryListGamesResponse,
        WatchGamesRequest as LibraryWatchGamesRequest, install_game_event, library_server::Library,
    },
    plugin::v1::{
        GetInstallManifestRequest, ListGamesRequest as StoreListGamesRequest,
        WatchGamesRequest as StoreWatchGamesRequest, plugin_client::PluginClient,
    },
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, async_trait, transport::Channel};

use bottles_core::{
    BottleManager,
    library::{InstallChunk, InstallEvent, InstallFile, InstallManager, install_dir},
    profile::ProfileManager,
};
use uuid::Uuid;

/// Aggregates game libraries across storefronts by resolving each
/// storefront's owning plugin through the Registry, then calling
/// Store.ListGames on that plugin directly. No in-process plugin
/// objects are held here — everything is a fresh RPC per call.
///
/// Installing a game splits cleanly across the two crates: this service
/// resolves the owning plugin and fetches its manifest (gRPC, so it
/// can't live in `next-core`), then hands the manifest's files to
/// `bottles_core::library::InstallManager`, which owns the storefront-
/// agnostic parts — downloading, reassembling, and persisting the
/// install. Picking a launch executable from the downloaded files is a
/// storefront-specific heuristic (GOG in particular), so it stays here
/// too, alongside registering the resulting `Program` on the bottle.
pub struct LibraryService {
    registry: Mutex<RegistryClient<Channel>>,
    install_manager: Arc<InstallManager>,
    bottles: BottleManager,
    profile: ProfileManager,
}

impl LibraryService {
    pub fn new(
        registry: RegistryClient<Channel>,
        install_manager: Arc<InstallManager>,
        profile: ProfileManager,
        bottles: BottleManager,
    ) -> Self {
        Self {
            registry: Mutex::new(registry),
            install_manager,
            profile,
            bottles,
        }
    }

    /// Resolves `storefront` to a live endpoint via the Registry, then
    /// dials that endpoint's Store service. Returns Ok(None) if no
    /// plugin currently owns the storefront (not an error — just
    /// something to skip in an aggregate query), and Err if the
    /// Registry call itself, or the dial, failed.
    async fn store_client_for(
        &self,
        storefront: Storefront,
    ) -> Result<Option<PluginClient<Channel>>, Status> {
        let resolved = {
            let mut registry = self.registry.lock().await;
            registry
                .resolve_plugin(ResolvePluginRequest {
                    storefront: storefront as i32,
                })
                .await?
                .into_inner()
        };

        let Some(endpoint) = resolved.endpoint else {
            return Ok(None);
        };

        let client = PluginClient::connect(endpoint.clone())
            .await
            .map_err(|err| {
                Status::unavailable(format!(
                    "failed to dial {storefront:?} plugin at {endpoint}: {err}"
                ))
            })?;

        Ok(Some(client))
    }
}

#[async_trait]
impl Library for LibraryService {
    async fn list_games(
        &self,
        request: Request<LibraryListGamesRequest>,
    ) -> Result<Response<LibraryListGamesResponse>, Status> {
        let LibraryListGamesRequest { profile_id } = request.into_inner();

        let storefronts = self
            .profile
            .get(&profile_id)
            .await?
            .accounts
            .into_iter()
            .filter(|a| a.auth_state == AuthState::Active as i32)
            .map(|a| a.storefront())
            .collect::<Vec<_>>();

        let mut games = Vec::new();

        for storefront in storefronts {
            // A single storefront being unreachable (unregistered,
            // crashed, dial failure) shouldn't fail the whole
            // aggregate query — log and continue with the rest.
            let mut client = match self.store_client_for(storefront).await {
                Ok(Some(client)) => client,
                Ok(None) => {
                    tracing::debug!("no plugin registered for {storefront:?}, skipping");
                    continue;
                }
                Err(err) => {
                    tracing::warn!("failed to reach {storefront:?} plugin: {err}");
                    continue;
                }
            };

            match client
                .list_games(StoreListGamesRequest {
                    profile_id: profile_id.clone(),
                })
                .await
            {
                Ok(response) => games.extend(response.into_inner().games),
                Err(err) => {
                    tracing::warn!("{storefront:?} plugin ListGames failed: {err}");
                }
            }
        }

        for game in &mut games {
            let Ok(storefront) = Storefront::try_from(game.storefront) else {
                continue;
            };
            if let Some(record) = self
                .install_manager
                .get(&profile_id, storefront, &game.id)
                .await
            {
                game.install_state = Some(record.install_state());
            }
        }

        Ok(Response::new(LibraryListGamesResponse { games }))
    }

    /// Server streaming response type for the WatchGames method.
    type WatchGamesStream = Pin<Box<dyn Stream<Item = Result<GameEvent, Status>> + Send + 'static>>;

    /// Resolves each requested storefront's plugin the same way
    /// ListGames does, then spawns one forwarding task per storefront
    /// that pipes its Store.WatchGames stream into a single shared
    /// channel. One storefront's plugin being unreachable, or its stream
    /// ending/erroring, doesn't affect the others.
    async fn watch_games(
        &self,
        request: Request<LibraryWatchGamesRequest>,
    ) -> Result<Response<Self::WatchGamesStream>, Status> {
        let LibraryWatchGamesRequest { profile_id } = request.into_inner();

        let storefronts = self
            .profile
            .get(&profile_id)
            .await?
            .accounts
            .into_iter()
            .filter(|a| a.auth_state == AuthState::Active as i32)
            .map(|a| a.storefront())
            .collect::<Vec<_>>();

        let (tx, rx) = mpsc::channel(32);

        for storefront in storefronts {
            let mut client = match self.store_client_for(storefront).await {
                Ok(Some(client)) => client,
                Ok(None) => {
                    tracing::debug!("no plugin registered for {storefront:?}, skipping watch");
                    continue;
                }
                Err(err) => {
                    tracing::warn!("failed to reach {storefront:?} plugin: {err}");
                    continue;
                }
            };

            let profile_id = profile_id.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut stream = match client
                    .watch_games(StoreWatchGamesRequest { profile_id })
                    .await
                {
                    Ok(response) => response.into_inner(),
                    Err(err) => {
                        tracing::warn!("{storefront:?} WatchGames failed: {err}");
                        return;
                    }
                };

                while let Some(item) = stream.next().await {
                    if tx.send(item).await.is_err() {
                        return;
                    }
                }
            });
        }

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type InstallGameStream =
        Pin<Box<dyn Stream<Item = Result<InstallGameEvent, Status>> + Send + 'static>>;

    /// Resolves the owning plugin's install manifest, then delegates the
    /// storefront-agnostic download/reassembly/persistence work to
    /// `InstallManager`. Once every file has landed, picks a launch
    /// executable (see `find_primary_executable`) and registers it as a
    /// `Program` on the bottle before recording the install. Progress
    /// from every file's download is forwarded as it happens; the
    /// stream ends with one `done` event.
    async fn install_game(
        &self,
        request: Request<InstallGameRequest>,
    ) -> Result<Response<Self::InstallGameStream>, Status> {
        let InstallGameRequest {
            profile_id,
            storefront,
            game_id,
            bottle_id,
        } = request.into_inner();
        let storefront_enum = Storefront::try_from(storefront)
            .map_err(|_| Status::invalid_argument("invalid storefront"))?;
        let bottle_uuid = Uuid::parse_str(&bottle_id)
            .map_err(|_| Status::invalid_argument("invalid bottle_id"))?;
        // Fails fast on an unknown bottle rather than downloading first.
        let bottle = self.bottles.open(bottle_uuid).await?;
        let c_drive = bottle.c_drive_path();

        let mut client = self
            .store_client_for(storefront_enum)
            .await?
            .ok_or_else(|| {
                Status::unavailable(format!("no plugin registered for {storefront_enum:?}"))
            })?;

        let manifest = client
            .get_install_manifest(GetInstallManifestRequest {
                profile_id: profile_id.clone(),
                game_id: game_id.clone(),
            })
            .await?
            .into_inner();

        // Depot file paths are flat (no game-named prefix of their
        // own) — install under the storefront's own canonical folder
        // name, matching where its official client would put them,
        // rather than dumping files straight at the drive root.
        let install_root_name = format!("Program Files/{}", manifest.install_directory);
        let destination_root = c_drive.join(&install_root_name);

        // Chunks land here temporarily before being decompressed and
        // concatenated straight into the bottle's C: drive — this is
        // just scratch space, never the final install location.
        let staging_dir = install_dir(&profile_id, storefront_enum, &game_id)?;

        let files = manifest
            .files
            .iter()
            .map(|file| InstallFile {
                relative_path: file.relative_path.clone(),
                chunks: file
                    .chunks
                    .iter()
                    .map(|chunk| InstallChunk {
                        download_url: chunk.download_url.clone(),
                        compressed: chunk.compressed,
                    })
                    .collect(),
            })
            .collect();

        let key = (profile_id.clone(), storefront, game_id.clone());
        let mut downloads = self.install_manager.download(
            key,
            destination_root,
            install_root_name,
            staging_dir,
            files,
        );
        let install_manager = self.install_manager.clone();

        let (tx, rx) = mpsc::channel(32);

        tokio::spawn(async move {
            while let Some(item) = downloads.next().await {
                match item {
                    Ok(InstallEvent::Progress(progress)) => {
                        let event = InstallGameEvent {
                            event: Some(install_game_event::Event::Progress(InstallProgress {
                                current_file: progress.current_file,
                                bytes_downloaded: progress.bytes_downloaded,
                                total_bytes: progress.total_bytes,
                                bytes_per_second: progress.bytes_per_second,
                            })),
                        };
                        if tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                    Ok(InstallEvent::Done {
                        relative_paths,
                        install_size_bytes,
                    }) => {
                        // Epic sets this directly (launch_exe). GOG
                        // never does — in principle it's knowable
                        // post-download from a goggame-<id>.info file
                        // shipped as a depot file (see plugin.proto's
                        // doc comment), but that file isn't always
                        // present, so that lookup is kept as a first
                        // try, not relied on, and backed by a heuristic
                        // scan of the install root that doesn't depend
                        // on GOG providing anything extra.
                        let primary_executable = manifest
                            .primary_executable
                            .clone()
                            .or_else(|| find_goggame_primary_executable(&c_drive, &relative_paths))
                            .or_else(|| {
                                find_primary_executable_heuristic(
                                    &relative_paths,
                                    &manifest.install_directory,
                                )
                            });

                        let program_id = match &primary_executable {
                            Some(executable_relative) => {
                                let windows_path =
                                    format!("C:\\{}", executable_relative.replace('/', "\\"));
                                let name = std::path::Path::new(executable_relative)
                                    .file_stem()
                                    .and_then(|stem| stem.to_str())
                                    .unwrap_or(&game_id)
                                    .to_string();
                                let program = bottles_core::Program::new(name, windows_path);
                                let program_id = program.id.to_string();
                                let mut edit = bottle.edit();
                                edit.add_program(program);
                                match edit.commit().await {
                                    Ok(()) => Some(program_id),
                                    Err(err) => {
                                        tracing::warn!(
                                            "failed to register launch program for {game_id}: {err}"
                                        );
                                        None
                                    }
                                }
                            }
                            None => {
                                tracing::warn!(
                                    "couldn't determine a launch executable for {game_id}; installed without a Program"
                                );
                                None
                            }
                        };

                        let record = bottles_core::library::InstallRecord {
                            profile_id: profile_id.clone(),
                            storefront,
                            game_id: game_id.clone(),
                            version: manifest.version.clone(),
                            install_size_bytes: manifest
                                .install_size_bytes
                                .or(Some(install_size_bytes)),
                            bottle_id: bottle_id.clone(),
                            relative_paths,
                            program_id,
                        };
                        let install_state = record.install_state();
                        match install_manager.record(record).await {
                            Ok(()) => {
                                let event = InstallGameEvent {
                                    event: Some(install_game_event::Event::Done(install_state)),
                                };
                                let _ = tx.send(Ok(event)).await;
                            }
                            Err(err) => {
                                let _ = tx.send(Err(err.into())).await;
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(Status::internal(err.to_string()))).await;
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn cancel_install(
        &self,
        request: Request<InstallGameRequest>,
    ) -> Result<Response<()>, Status> {
        let InstallGameRequest {
            profile_id,
            storefront,
            game_id,
            ..
        } = request.into_inner();
        self.install_manager
            .cancel(&(profile_id, storefront, game_id))
            .await;
        Ok(Response::new(()))
    }

    /// Removes exactly the files this install wrote and the registered
    /// launch `Program`, if any — see `InstallManager::uninstall`.
    async fn uninstall_game(
        &self,
        request: Request<InstallGameRequest>,
    ) -> Result<Response<()>, Status> {
        let InstallGameRequest {
            profile_id,
            storefront,
            game_id,
            ..
        } = request.into_inner();
        let storefront_enum = Storefront::try_from(storefront)
            .map_err(|_| Status::invalid_argument("invalid storefront"))?;

        let bottle = match self
            .install_manager
            .get(&profile_id, storefront_enum, &game_id)
            .await
        {
            Some(record) => match Uuid::parse_str(&record.bottle_id) {
                Ok(bottle_uuid) => self.bottles.open(bottle_uuid).await.ok(),
                Err(_) => None,
            },
            None => None,
        };

        self.install_manager
            .uninstall(&profile_id, storefront_enum, &game_id, bottle)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(()))
    }
}

/// GOG's launch executable isn't in the install manifest — it's inside
/// a `goggame-<id>.info` file that ships as one of the depot's own
/// files, sitting at the install root alongside a `playTasks` array;
/// the entry with `isPrimary: true` names the executable, relative to
/// that same root. Best-effort: `None` on any format surprise (missing
/// file, unexpected JSON shape) rather than failing the whole install
/// over launch metadata.
fn find_goggame_primary_executable(
    c_drive: &std::path::Path,
    relative_paths: &[String],
) -> Option<String> {
    let info_path = relative_paths.iter().find(|path| {
        std::path::Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                let name = name.to_ascii_lowercase();
                name.starts_with("goggame-") && name.ends_with(".info")
            })
    })?;

    let contents = std::fs::read(c_drive.join(info_path)).ok()?;
    let info: serde_json::Value = serde_json::from_slice(&contents).ok()?;
    let primary_task =
        info.get("playTasks")?.as_array()?.iter().find(|task| {
            task.get("isPrimary").and_then(serde_json::Value::as_bool) == Some(true)
        })?;
    let executable = primary_task.get("path")?.as_str()?;

    let root = std::path::Path::new(info_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    Some(
        root.join(executable.replace('\\', "/"))
            .to_string_lossy()
            .replace('\\', "/"),
    )
}

/// Falls back to guessing the launch executable from the installed file
/// list itself when nothing in the manifest or a goggame-*.info file
/// says so outright. Only looks directly inside the install root (not
/// subdirectories — redist installers, launchers, and crash handlers
/// routinely ship their own nested `.exe`s that aren't the game).
///
/// Prefers an exact case-insensitive match between a candidate's file
/// stem and the install directory name (GOG's own convention in
/// practice: "Hollow Knight" installs "Hollow Knight.exe" at its
/// root); falls back to the single remaining candidate once obvious
/// non-game helpers (crash handlers, uninstallers, redist installers)
/// are filtered out, but refuses to guess when more than one
/// candidate remains — a wrong launch target is worse than none.
fn find_primary_executable_heuristic(
    relative_paths: &[String],
    install_directory: &str,
) -> Option<String> {
    let root_prefix = format!("Program Files/{install_directory}/");

    let candidates: Vec<&str> = relative_paths
        .iter()
        .filter_map(|path| {
            let rest = path.strip_prefix(&root_prefix)?;
            let is_top_level_exe =
                !rest.contains('/') && rest.to_ascii_lowercase().ends_with(".exe");
            is_top_level_exe.then_some(path.as_str())
        })
        .collect();

    if let Some(exact) = candidates.iter().find(|path| {
        std::path::Path::new(path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case(install_directory))
    }) {
        return Some((*exact).to_string());
    }

    fn looks_like_helper(name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        [
            "crashhandler",
            "crashpad",
            "unins",
            "redist",
            "vcredist",
            "setup",
            "helper",
        ]
        .iter()
        .any(|pattern| name.contains(pattern))
    }

    let non_helpers: Vec<&&str> = candidates
        .iter()
        .filter(|path| {
            std::path::Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !looks_like_helper(name))
        })
        .collect();

    match non_helpers.as_slice() {
        [only] => Some((**only).to_string()),
        _ => None,
    }
}
