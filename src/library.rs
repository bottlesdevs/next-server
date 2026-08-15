use std::pin::Pin;

use futures_core::Stream;
use next_proto::bottles::{
    common::v1::Storefront,
    library::v1::{
        GameEvent, ListGamesRequest as LibraryListGamesRequest,
        ListGamesResponse as LibraryListGamesResponse, WatchGamesRequest as LibraryWatchGamesRequest,
        library_server::Library,
    },
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
    store::v1::{
        ListGamesRequest as StoreListGamesRequest, WatchGamesRequest as StoreWatchGamesRequest,
        store_client::StoreClient,
    },
};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, async_trait, transport::Channel};

/// Aggregates game libraries across storefronts by resolving each
/// storefront's owning plugin through the Registry, then calling
/// Store.ListGames on that plugin directly. No in-process plugin
/// objects are held here — everything is a fresh RPC per call.
pub struct LibraryService {
    registry: Mutex<RegistryClient<Channel>>,
}

impl LibraryService {
    pub fn new(registry: RegistryClient<Channel>) -> Self {
        Self {
            registry: Mutex::new(registry),
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
}
