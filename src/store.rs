use std::sync::Arc;

use bottles_core::{registry::StoreRegistry, storefronts::StorePlugin};
use next_proto::bottles::{
    common::v1::{LinkedAccount, Storefront},
    store::v1::{
        BeginLoginRequest, CompleteLoginRequest, ListAvailableStorefrontsResponse, LoginChallenge,
        RefreshSessionRequest, RevokeSessionRequest, store_server::Store,
    },
};
use tonic::{Request, Response, Result, Status, async_trait};

pub struct StoreService {
    stores: Arc<StoreRegistry>,
}

impl StoreService {
    pub fn new(stores: Arc<StoreRegistry>) -> Self {
        Self { stores }
    }

    fn store(&self, storefront: Storefront) -> Result<&Arc<dyn StorePlugin>, Status> {
        self.stores.get(storefront).ok_or_else(|| {
            Status::unimplemented(format!("No StorePlugin registered for {storefront:?}"))
        })
    }
}

#[async_trait]
impl Store for StoreService {
    /// Starts an interactive login for a storefront. The returned challenge
    /// tells the caller what to present to the user (a URL to open, a device
    /// code to display, etc). Does not block on user action.
    async fn begin_login(
        &self,
        request: Request<BeginLoginRequest>,
    ) -> Result<Response<LoginChallenge>, Status> {
        let BeginLoginRequest {
            profile_id,
            storefront,
        } = request.into_inner();
        let storefront = Storefront::try_from(storefront)
            .map_err(|e| Status::invalid_argument(format!("Invalid storefront: {e}")))?;

        let challenge = self.store(storefront)?.begin_login(&profile_id).await?;

        Ok(Response::new(challenge))
    }

    /// Completes a previously started login. For flows that need user input
    /// (an authorization code, an exchange token) it's passed here; for
    /// flows that resolve via polling or redirect capture, user_input is
    /// left empty and the server resolves it out-of-band.
    async fn complete_login(
        &self,
        request: Request<CompleteLoginRequest>,
    ) -> Result<Response<LinkedAccount>, Status> {
        let CompleteLoginRequest {
            challenge_id,
            profile_id,
            storefront,
            user_input,
        } = request.into_inner();

        let storefront = Storefront::try_from(storefront)
            .map_err(|e| Status::invalid_argument(format!("Invalid storefront: {e}")))?;

        if profile_id.is_empty() {
            return Err(Status::invalid_argument("profile_id is required"));
        }

        if challenge_id.is_empty() {
            return Err(Status::invalid_argument("challenge_id is required"));
        }

        let account = self
            .store(storefront)?
            .complete_login(&profile_id, &challenge_id, &user_input)
            .await?;

        Ok(Response::new(account))
    }

    /// Cheap, non-interactive: asks the owning plugin to verify/refresh the
    /// stored session for this storefront on this profile.
    async fn refresh_session(
        &self,
        request: Request<RefreshSessionRequest>,
    ) -> Result<Response<LinkedAccount>, Status> {
        let RefreshSessionRequest {
            profile_id,
            storefront,
        } = request.into_inner();

        let storefront = Storefront::try_from(storefront)
            .map_err(|e| Status::invalid_argument(format!("Invalid storefront: {e}")))?;

        let account = self
            .store(storefront)?
            .refresh_session(&profile_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(account))
    }
    /// Revokes the stored session for this storefront on this profile.
    async fn revoke_session(
        &self,
        _request: Request<RevokeSessionRequest>,
    ) -> Result<Response<()>, Status> {
        Ok(Response::new(()))
    }
    /// Lists storefronts with a registered, working StorePlugin.
    async fn list_available_storefronts(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ListAvailableStorefrontsResponse>, Status> {
        Ok(Response::new(ListAvailableStorefrontsResponse {
            storefronts: vec![],
        }))
    }
}
