use next_proto::bottles::{
    common::v1::{LinkedAccount, Storefront},
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
    store::v1::{
        BeginLoginRequest, CompleteLoginRequest, ListGamesRequest, ListGamesResponse,
        LoginChallenge, RefreshSessionRequest, RevokeSessionRequest, store_client::StoreClient,
        store_server::Store,
    },
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status, async_trait, transport::Channel};

/// Forwards each Store call to the plugin that owns the request's
/// storefront, resolved fresh through the Registry on every call — the
/// same pattern LibraryService uses to fan out ListGames.
pub struct StoreService {
    registry: Mutex<RegistryClient<Channel>>,
}

impl StoreService {
    pub fn new(registry: RegistryClient<Channel>) -> Self {
        Self {
            registry: Mutex::new(registry),
        }
    }

    async fn client_for(&self, storefront: Storefront) -> Result<StoreClient<Channel>, Status> {
        let resolved = {
            let mut registry = self.registry.lock().await;
            registry
                .resolve_plugin(ResolvePluginRequest {
                    storefront: storefront as i32,
                })
                .await?
                .into_inner()
        };

        let endpoint = resolved
            .endpoint
            .ok_or_else(|| Status::unavailable(format!("no plugin registered for {storefront:?}")))?;

        StoreClient::connect(endpoint.clone()).await.map_err(|err| {
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
impl Store for StoreService {
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
        self.client_for(storefront)
            .await?
            .complete_login(req)
            .await
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
        self.client_for(storefront)
            .await?
            .revoke_session(req)
            .await
    }

    async fn list_games(
        &self,
        request: Request<ListGamesRequest>,
    ) -> Result<Response<ListGamesResponse>, Status> {
        // Store.ListGames is single-storefront; there's no storefront
        // field on the request itself here, so this entry point isn't
        // meaningful to proxy without one. Callers wanting an aggregate
        // view across storefronts should use Library.ListGames instead.
        let _ = request;
        Err(Status::unimplemented(
            "call Library.ListGames for an aggregate view across storefronts",
        ))
    }
}
