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

use bottles_core::library::{InstallRecord, InstallsStore, install_dir};

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
    ) -> Self {
        Self {
            registry: Mutex::new(registry),
            downloads,
            installs,
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

    /// Resolves the owning plugin's install manifest, downloads every
    /// file into `installs::install_dir`, and persists an `InstallRecord`
    /// on success. Progress from every file's download is forwarded as
    /// it happens; the stream ends with one `done` event.
    ///
    /// Doesn't yet install into a Bottle (see bottles.bottle.v1, not
    /// implemented) — files land in a Library-managed directory instead,
    /// and the resulting `InstallState.bottle_id` stays unset.
    async fn install_game(
        &self,
        request: Request<InstallGameRequest>,
    ) -> Result<Response<Self::InstallGameStream>, Status> {
        let InstallGameRequest {
            profile_id,
            storefront,
            game_id,
        } = request.into_inner();
        let storefront_enum = Storefront::try_from(storefront)
            .map_err(|_| Status::invalid_argument("invalid storefront"))?;

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

        let dir = install_dir(&profile_id, storefront_enum, &game_id)?;
        let downloads = self.downloads.clone();
        let installs = self.installs.clone();
        let key: InstallKey = (profile_id.clone(), storefront, game_id.clone());

        let (tx, rx) = mpsc::channel(32);
        let mut handles = Vec::with_capacity(manifest.files.len());
        let mut in_flight = Vec::with_capacity(manifest.files.len());

        for file in &manifest.files {
            let url = match url::Url::parse(&file.download_url) {
                Ok(url) => url,
                Err(err) => {
                    let _ = tx
                        .send(Err(Status::internal(format!(
                            "bad download URL for {}: {err}",
                            file.relative_path
                        ))))
                        .await;
                    return Ok(Response::new(Box::pin(ReceiverStream::new(rx))));
                }
            };
            let destination = dir.join(&file.relative_path);
            if let Some(parent) = destination.parent()
                && let Err(err) = tokio::fs::create_dir_all(parent).await
            {
                let _ = tx.send(Err(Status::internal(err.to_string()))).await;
                return Ok(Response::new(Box::pin(ReceiverStream::new(rx))));
            }

            let download = match downloads.download(url, destination) {
                Ok(download) => download,
                Err(err) => {
                    let _ = tx
                        .send(Err(Status::internal(format!(
                            "failed to enqueue {}: {err}",
                            file.relative_path
                        ))))
                        .await;
                    return Ok(Response::new(Box::pin(ReceiverStream::new(rx))));
                }
            };
            handles.push(download.clone());

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

            in_flight.push((file.relative_path.clone(), download));
        }

        self.active_installs
            .lock()
            .await
            .insert(key.clone(), handles);
        let active_installs = self.active_installs.clone();

        tokio::spawn(async move {
            let mut relative_paths = Vec::with_capacity(in_flight.len());
            let mut install_size_bytes = Some(0u64);
            let mut failed = false;

            for (relative_path, download) in in_flight {
                match download.await {
                    Ok(result) => {
                        relative_paths.push(relative_path);
                        install_size_bytes = install_size_bytes
                            .zip(Some(result.bytes_downloaded))
                            .map(|(a, b)| a + b);
                    }
                    Err(err) => {
                        let _ = tx.send(Err(Status::internal(err.to_string()))).await;
                        failed = true;
                        break;
                    }
                }
            }

            // The install is no longer cancellable once every file has
            // settled (succeeded, failed, or was already cancelled by
            // CancelInstall, which removes this entry itself).
            active_installs.lock().await.remove(&key);

            if !failed {
                let record = InstallRecord {
                    profile_id,
                    storefront,
                    game_id,
                    version: manifest.version,
                    install_size_bytes: manifest.install_size_bytes.or(install_size_bytes),
                    relative_paths,
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
        } = request.into_inner();
        let key: InstallKey = (profile_id, storefront, game_id);

        if let Some(downloads) = self.active_installs.lock().await.remove(&key) {
            for download in downloads {
                let _ = download.cancel().await;
            }
        }

        Ok(Response::new(()))
    }

    async fn uninstall_game(
        &self,
        request: Request<InstallGameRequest>,
    ) -> Result<Response<()>, Status> {
        let InstallGameRequest {
            profile_id,
            storefront,
            game_id,
        } = request.into_inner();
        let storefront_enum = Storefront::try_from(storefront)
            .map_err(|_| Status::invalid_argument("invalid storefront"))?;

        let removed = self
            .installs
            .remove(&profile_id, storefront_enum, &game_id)
            .await?;
        if removed.is_some() {
            let dir = install_dir(&profile_id, storefront_enum, &game_id)?;
            let _ = tokio::fs::remove_dir_all(dir).await;
        }

        Ok(Response::new(()))
    }
}
