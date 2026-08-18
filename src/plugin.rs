use next_proto::bottles::{
    common::v1::{LinkedAccount, Storefront},
    library::v1::GameEvent,
    plugin::v1::{
        BeginLoginRequest, CompleteLoginRequest, GetInstallManifestRequest, InstallManifest,
        ListGamesRequest, ListGamesResponse, LoginChallenge, RefreshSessionRequest,
        RevokeSessionRequest, WatchGamesRequest, plugin_client::PluginClient,
        plugin_server::Plugin,
    },
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
};
use std::pin::Pin;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status, async_trait, transport::Channel};

/// Forwards each Plugin call to the plugin that owns the request's
/// storefront, resolved fresh through the Registry on every call — the
/// same pattern LibraryService uses to fan out ListGames.
pub struct PluginService {
    registry: Mutex<RegistryClient<Channel>>,
}

impl PluginService {
    pub fn new(registry: RegistryClient<Channel>) -> Self {
        Self {
            registry: Mutex::new(registry),
        }
    }

    async fn client_for(&self, storefront: Storefront) -> Result<PluginClient<Channel>, Status> {
        let resolved = {
            let mut registry = self.registry.lock().await;
            registry
                .resolve_plugin(ResolvePluginRequest {
                    storefront: storefront as i32,
                })
                .await?
                .into_inner()
        };

        let endpoint = resolved.endpoint.ok_or_else(|| {
            Status::unavailable(format!("no plugin registered for {storefront:?}"))
        })?;

        PluginClient::connect(endpoint.clone())
            .await
            .map_err(|err| {
                Status::unavailable(format!(
                    "failed to dial {storefront:?} plugin at {endpoint}: {err}"
                ))
            })
    }

    fn parse_storefront(raw: i32) -> Result<Storefront, Status> {
        Storefront::try_from(raw).map_err(|_| Status::invalid_argument("invalid storefront"))
    }
}

#[async_trait]
impl Plugin for PluginService {
    async fn begin_login(
        &self,
        request: Request<BeginLoginRequest>,
    ) -> Result<Response<LoginChallenge>, Status> {
        let req = request.into_inner();
        let storefront = Self::parse_storefront(req.storefront)?;
        self.client_for(storefront).await?.begin_login(req).await
    }

    async fn complete_login(
        &self,
        request: Request<CompleteLoginRequest>,
    ) -> Result<Response<LinkedAccount>, Status> {
        let req = request.into_inner();
        let storefront = Self::parse_storefront(req.storefront)?;
        self.client_for(storefront).await?.complete_login(req).await
    }

    async fn refresh_session(
        &self,
        request: Request<RefreshSessionRequest>,
    ) -> Result<Response<LinkedAccount>, Status> {
        let req = request.into_inner();
        let storefront = Self::parse_storefront(req.storefront)?;
        self.client_for(storefront)
            .await?
            .refresh_session(req)
            .await
    }

    async fn revoke_session(
        &self,
        request: Request<RevokeSessionRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let storefront = Self::parse_storefront(req.storefront)?;
        self.client_for(storefront).await?.revoke_session(req).await
    }

    async fn list_games(
        &self,
        request: Request<ListGamesRequest>,
    ) -> Result<Response<ListGamesResponse>, Status> {
        // Plugin.ListGames is single-storefront; there's no storefront
        // field on the request itself here, so this entry point isn't
        // meaningful to proxy without one. Callers wanting an aggregate
        // view across storefronts should use Library.ListGames instead.
        let _ = request;
        Err(Status::unimplemented(
            "call Library.ListGames for an aggregate view across storefronts",
        ))
    }

    type WatchGamesStream =
        Pin<Box<dyn futures_core::Stream<Item = Result<GameEvent, Status>> + Send + 'static>>;

    async fn watch_games(
        &self,
        request: Request<WatchGamesRequest>,
    ) -> Result<Response<Self::WatchGamesStream>, Status> {
        // Same reasoning as ListGames above: no storefront field on this
        // request to route on.
        let _ = request;
        Err(Status::unimplemented(
            "call Library.WatchGames for an aggregate view across storefronts",
        ))
    }

    async fn get_install_manifest(
        &self,
        request: Request<GetInstallManifestRequest>,
    ) -> Result<Response<InstallManifest>, Status> {
        // Same reasoning as ListGames/WatchGames above: no storefront
        // field on this request to route on. Library.InstallGame calls
        // Plugin.GetInstallManifest directly on the resolved plugin, since
        // it has a storefront from its own request.
        let _ = request;
        Err(Status::unimplemented(
            "call Library.InstallGame, which resolves the owning plugin itself",
        ))
    }
}
