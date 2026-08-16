use std::{collections::HashMap, pin::Pin, sync::Arc};

use download_manager::{download::Download, manager::DownloadManager};
use futures_core::Stream;
use next_proto::bottles::{
    common::v1::Storefront,
    library::v1::{
        GameEvent, InstallGameEvent, InstallGameRequest, InstallProgress,
        ListGamesRequest as LibraryListGamesRequest, ListGamesResponse as LibraryListGamesResponse,
        WatchGamesRequest as LibraryWatchGamesRequest, install_game_event, library_server::Library,
    },
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
    store::v1::{
        GetInstallManifestRequest, ListGamesRequest as StoreListGamesRequest,
        WatchGamesRequest as StoreWatchGamesRequest, store_client::StoreClient,
    },
};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, async_trait, transport::Channel};

use bottles_core::{
    BottleManager,
    library::{InstallRecord, InstallsStore, install_dir},
};
use uuid::Uuid;

/// Identifies one in-flight install for `CancelInstall` to find.
type InstallKey = (String, i32, String);

/// Aggregates game libraries across storefronts by resolving each
/// storefront's owning plugin through the Registry, then calling
/// Store.ListGames on that plugin directly. No in-process plugin
/// objects are held here — everything is a fresh RPC per call.
pub struct LibraryService {
    registry: Mutex<RegistryClient<Channel>>,
    downloads: Arc<DownloadManager>,
    installs: Arc<InstallsStore>,
    bottles: BottleManager,
    /// Downloads belonging to an in-progress InstallGame, so CancelInstall
    /// can find and cancel them. Entries are removed once the install
    /// finishes, fails, or is cancelled. `Arc`-wrapped so the spawned
    /// completion task in `install_game` can remove its own entry
    /// without needing `self` to outlive it.
    active_installs: Arc<Mutex<HashMap<InstallKey, Vec<Download>>>>,
}

impl LibraryService {
    pub fn new(
        registry: RegistryClient<Channel>,
        downloads: Arc<DownloadManager>,
        installs: Arc<InstallsStore>,
        bottles: BottleManager,
    ) -> Self {
        Self {
            registry: Mutex::new(registry),
            downloads,
            installs,
            bottles,
            active_installs: Arc::new(Mutex::new(HashMap::new())),
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
    ) -> Result<Option<StoreClient<Channel>>, Status> {
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

        let client = StoreClient::connect(endpoint.clone())
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
        let LibraryListGamesRequest {
            profile_id,
            storefronts,
        } = request.into_inner();

        let storefronts = storefronts
            .into_iter()
            .filter_map(|s| Storefront::try_from(s).ok())
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
            if let Some(record) = self.installs.get(&profile_id, storefront, &game.id).await {
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
        let LibraryWatchGamesRequest {
            profile_id,
            storefronts,
        } = request.into_inner();

        let storefronts = storefronts
            .into_iter()
            .filter_map(|s| Storefront::try_from(s).ok())
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

    /// Resolves the owning plugin's install manifest and downloads every
    /// file directly into `bottle_id`'s `C:` drive, then registers the
    /// discovered launch executable (see `find_primary_executable`) as a
    /// `Program` on that bottle before persisting an `InstallRecord`.
    /// Progress from every file's download is forwarded as it happens;
    /// the stream ends with one `done` event.
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
        let bottle_uuid =
            Uuid::parse_str(&bottle_id).map_err(|_| Status::invalid_argument("invalid bottle_id"))?;
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
        let install_root = format!("Program Files/{}", manifest.install_directory);

        // Chunks land here temporarily before being decompressed and
        // concatenated straight into the bottle's C: drive — this is
        // just scratch space, never the final install location.
        let staging_dir = install_dir(&profile_id, storefront_enum, &game_id)?;
        let downloads = self.downloads.clone();
        let installs = self.installs.clone();
        let key: InstallKey = (profile_id.clone(), storefront, game_id.clone());

        let (tx, rx) = mpsc::channel(32);
        let mut all_handles = Vec::new();
        // Per file: (relative_path, one Download per chunk, that chunk's
        // temp path, whether it needs zlib decompression). Chunks
        // download independently, then get concatenated in order once
        // every chunk for that file has landed — see `reassemble_file`.
        let mut file_downloads = Vec::with_capacity(manifest.files.len());

        for file in &manifest.files {
            let mut chunk_downloads = Vec::with_capacity(file.chunks.len());
            let mut chunk_temp_paths = Vec::with_capacity(file.chunks.len());
            let mut chunk_compressed = Vec::with_capacity(file.chunks.len());
            let mut chunk_urls = Vec::with_capacity(file.chunks.len());

            for (index, chunk) in file.chunks.iter().enumerate() {
                let url = match url::Url::parse(&chunk.download_url) {
                    Ok(url) => url,
                    Err(err) => {
                        let _ = tx
                            .send(Err(Status::internal(format!(
                                "bad chunk URL for {}: {err}",
                                file.relative_path
                            ))))
                            .await;
                        return Ok(Response::new(Box::pin(ReceiverStream::new(rx))));
                    }
                };
                let temp_path = staging_dir.join(".chunks").join(format!(
                    "{}.{index}",
                    file.relative_path.replace(['/', '\\'], "_")
                ));
                if let Some(parent) = temp_path.parent()
                    && let Err(err) = tokio::fs::create_dir_all(parent).await
                {
                    let _ = tx.send(Err(Status::internal(err.to_string()))).await;
                    return Ok(Response::new(Box::pin(ReceiverStream::new(rx))));
                }

                let download = match downloads.download(url, temp_path.clone()) {
                    Ok(download) => download,
                    Err(err) => {
                        let _ = tx
                            .send(Err(Status::internal(format!(
                                "failed to enqueue a chunk of {}: {err}",
                                file.relative_path
                            ))))
                            .await;
                        return Ok(Response::new(Box::pin(ReceiverStream::new(rx))));
                    }
                };
                all_handles.push(download.clone());

                let relative_path = file.relative_path.clone();
                let progress_tx = tx.clone();
                let progress_download = download.clone();
                tokio::spawn(async move {
                    let stream = progress_download.progress();
                    tokio::pin!(stream);
                    while let Some(progress) = stream.next().await {
                        let event = InstallGameEvent {
                            event: Some(install_game_event::Event::Progress(InstallProgress {
                                current_file: relative_path.clone(),
                                bytes_downloaded: progress.bytes_downloaded(),
                                total_bytes: progress.total_bytes(),
                                bytes_per_second: progress.bytes_per_second(),
                            })),
                        };
                        if progress_tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                });

                chunk_downloads.push(download);
                chunk_temp_paths.push(temp_path);
                chunk_compressed.push(chunk.compressed);
                chunk_urls.push(chunk.download_url.clone());
            }

            file_downloads.push((
                file.relative_path.clone(),
                chunk_downloads,
                chunk_temp_paths,
                chunk_compressed,
                chunk_urls,
            ));
        }

        self.active_installs
            .lock()
            .await
            .insert(key.clone(), all_handles);
        let active_installs = self.active_installs.clone();

        tokio::spawn(async move {
            let mut relative_paths = Vec::with_capacity(file_downloads.len());
            let mut install_size_bytes = Some(0u64);
            let mut failed = false;

            'files: for (
                relative_path,
                chunk_downloads,
                chunk_temp_paths,
                chunk_compressed,
                chunk_urls,
            ) in file_downloads
            {
                for (download, url) in chunk_downloads.iter().zip(&chunk_urls) {
                    if let Err(err) = download.clone().await {
                        tracing::warn!(
                            "chunk download failed for {relative_path} ({url}): {err}"
                        );
                        let _ = tx
                            .send(Err(Status::internal(format!(
                                "{relative_path} ({url}): {err}"
                            ))))
                            .await;
                        failed = true;
                        break 'files;
                    }
                }

                let destination = c_drive.join(&install_root).join(&relative_path);
                match reassemble_file(&destination, &chunk_temp_paths, &chunk_compressed).await {
                    Ok(size) => {
                        // Stored (and used for goggame-*.info discovery
                        // below) with `install_root` baked in, so it's
                        // already a full path relative to `c_drive` —
                        // callers never need to know `install_root`
                        // separately.
                        relative_paths.push(format!("{install_root}/{relative_path}"));
                        install_size_bytes =
                            install_size_bytes.zip(Some(size)).map(|(a, b)| a + b);
                    }
                    Err(err) => {
                        let _ = tx.send(Err(Status::internal(err.to_string()))).await;
                        failed = true;
                        break 'files;
                    }
                }
            }

            // The install is no longer cancellable once every file has
            // settled (succeeded, failed, or was already cancelled by
            // CancelInstall, which removes this entry itself).
            active_installs.lock().await.remove(&key);

            if !failed {
                // Epic sets this directly (launch_exe). GOG never does —
                // in principle it's knowable post-download from a
                // goggame-<id>.info file shipped as a depot file (see
                // store.proto's doc comment), but that file isn't always
                // present, so that lookup is kept as a first try, not
                // relied on, and backed by a heuristic scan of the
                // install root that doesn't depend on GOG providing
                // anything extra.
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

                let record = InstallRecord {
                    profile_id,
                    storefront,
                    game_id,
                    version: manifest.version,
                    install_size_bytes: manifest.install_size_bytes.or(install_size_bytes),
                    bottle_id,
                    relative_paths,
                    program_id,
                };
                let install_state = record.install_state();
                match installs.upsert(record).await {
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
        let key: InstallKey = (profile_id, storefront, game_id);

        if let Some(downloads) = self.active_installs.lock().await.remove(&key) {
            for download in downloads {
                let _ = download.cancel().await;
            }
        }

        Ok(Response::new(()))
    }

    /// Removes exactly the files this install wrote (from the
    /// `InstallRecord`, not a directory sweep — the bottle's `C:` drive
    /// is shared with every other game installed there) and the
    /// registered launch `Program`, if any.
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

        let Some(record) = self
            .installs
            .remove(&profile_id, storefront_enum, &game_id)
            .await?
        else {
            return Ok(Response::new(()));
        };

        if let Ok(bottle_uuid) = Uuid::parse_str(&record.bottle_id)
            && let Ok(bottle) = self.bottles.open(bottle_uuid).await
        {
            let c_drive = bottle.c_drive_path();
            for relative_path in &record.relative_paths {
                let _ = tokio::fs::remove_file(c_drive.join(relative_path)).await;
            }
            if let Some(program_id) = record.program_id.as_deref()
                && let Ok(program_uuid) = Uuid::parse_str(program_id)
            {
                let mut edit = bottle.edit();
                edit.remove_program(program_uuid);
                if let Err(err) = edit.commit().await {
                    tracing::warn!("failed to remove launch program for {game_id}: {err}");
                }
            }
        }

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
    let primary_task = info
        .get("playTasks")?
        .as_array()?
        .iter()
        .find(|task| task.get("isPrimary").and_then(serde_json::Value::as_bool) == Some(true))?;
    let executable = primary_task.get("path")?.as_str()?;

    let root = std::path::Path::new(info_path).parent().unwrap_or_else(|| std::path::Path::new(""));
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
        ["crashhandler", "crashpad", "unins", "redist", "vcredist", "setup", "helper"]
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

/// Concatenates a file's already-downloaded chunks, in order, into
/// `destination`, decompressing each independently first when its
/// manifest entry said it needed it (matches how GOG's — and Epic's —
/// chunk formats work: each chunk is compressed on its own, not the
/// concatenated whole). Returns the reassembled file's size. Leaves
/// temp chunk files in place on error so a retry doesn't have to
/// re-download them; removes them on success.
async fn reassemble_file(
    destination: &std::path::Path,
    chunk_temp_paths: &[std::path::PathBuf],
    chunk_compressed: &[bool],
) -> std::io::Result<u64> {
    use tokio::io::AsyncWriteExt;

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut out = tokio::fs::File::create(destination).await?;
    let mut total = 0u64;

    for (temp_path, &compressed) in chunk_temp_paths.iter().zip(chunk_compressed) {
        let bytes = tokio::fs::read(temp_path).await?;
        let bytes = if compressed {
            tokio::task::spawn_blocking(move || {
                use std::io::Read;
                let mut decoder = flate2::read::ZlibDecoder::new(&bytes[..]);
                let mut decompressed = Vec::new();
                decoder.read_to_end(&mut decompressed)?;
                std::io::Result::Ok(decompressed)
            })
            .await
            .map_err(std::io::Error::other)??
        } else {
            bytes
        };

        total += bytes.len() as u64;
        out.write_all(&bytes).await?;
    }

    for temp_path in chunk_temp_paths {
        let _ = tokio::fs::remove_file(temp_path).await;
    }

    Ok(total)
}
