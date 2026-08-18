//! `bottles.profiles.v1.Profile` — a thin gRPC facade over
//! `bottles_core::profile::ProfileManager` for persistence and local
//! mutation, plus the Registry/Plugin dialing that manager
//! deliberately doesn't own (see its module docs): refreshing linked
//! accounts and revoking sessions.

use std::pin::Pin;

use bottles_core::profile::ProfileManager;
use futures_core::Stream;
use next_proto::bottles::{
    accounts::v1::{RefreshAccountRequest, accounts_client::AccountsClient},
    common::v1::Storefront,
    profiles::v1::{
        AccountActivationResult, ActivateProfileRequest, ActivateProfileResponse,
        ActivationOutcome, CreateProfileRequest, DeleteProfileRequest, GetActiveProfileResponse,
        GetProfileRequest, ListProfilesResponse, ProfileEvent, RenameProfileRequest, UserProfile,
        profile_server::Profile,
    },
};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Result, Status, async_trait, transport::Channel};

pub struct ProfileService {
    manager: ProfileManager,
    accounts: Mutex<AccountsClient<Channel>>,
}

impl ProfileService {
    pub fn new(manager: ProfileManager, accounts: AccountsClient<Channel>) -> Self {
        Self {
            manager,
            accounts: Mutex::new(accounts),
        }
    }
}

#[async_trait]
impl Profile for ProfileService {
    async fn list_profiles(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ListProfilesResponse>, Status> {
        Ok(Response::new(ListProfilesResponse {
            profiles: self.manager.list().await,
        }))
    }

    async fn get_profile(
        &self,
        request: Request<GetProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let profile_id = request.into_inner().profile_id;
        let profile = self.manager.get(&profile_id).await.map_err(Status::from)?;
        Ok(Response::new(profile))
    }

    async fn create_profile(
        &self,
        request: Request<CreateProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let req = request.into_inner();
        let profile = self
            .manager
            .create(req.name, req.icon)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(profile))
    }

    async fn delete_profile(
        &self,
        request: Request<DeleteProfileRequest>,
    ) -> Result<Response<()>, Status> {
        let profile_id = request.into_inner().profile_id;
        self.manager
            .delete(&profile_id)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(()))
    }

    async fn rename_profile(
        &self,
        request: Request<RenameProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let req = request.into_inner();
        let profile = self
            .manager
            .rename(&req.profile_id, req.name)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(profile))
    }

    async fn update_profile(&self, request: Request<UserProfile>) -> Result<Response<()>, Status> {
        let profile = request.into_inner();
        self.manager.update(profile).await.map_err(Status::from)?;
        Ok(Response::new(()))
    }

    async fn get_active_profile(
        &self,
        _request: Request<()>,
    ) -> Result<Response<GetActiveProfileResponse>, Status> {
        Ok(Response::new(GetActiveProfileResponse {
            profile: self.manager.active().await,
        }))
    }

    /// Activates a profile: for every linked account, verifies/refreshes its
    /// session via the owning plugin and marks it AUTH_STATE_ACTIVE.
    async fn activate_profile(
        &self,
        request: Request<ActivateProfileRequest>,
    ) -> Result<Response<ActivateProfileResponse>, Status> {
        let ActivateProfileRequest { profile_id, .. } = request.into_inner();

        let profile = self.manager.get(&profile_id).await.map_err(Status::from)?;

        let mut results = Vec::new();

        for account in profile.accounts {
            let Ok(_) = Storefront::try_from(account.storefront) else {
                continue;
            };

            let response = self
                .accounts
                .lock()
                .await
                .refresh_account(RefreshAccountRequest {
                    profile_id: profile_id.clone(),
                    storefront: account.storefront,
                })
                .await;

            let outcome = match response {
                Ok(response) => {
                    let mut profile = self.manager.get(&profile_id).await.map_err(Status::from)?;
                    let updated = response.into_inner();
                    if let Some(existing) = profile
                        .accounts
                        .iter_mut()
                        .find(|a| a.storefront == account.storefront)
                    {
                        *existing = updated;
                    }
                    self.manager.update(profile).await.map_err(Status::from)?;
                    AccountActivationResult {
                        storefront: account.storefront,
                        outcome: ActivationOutcome::Success as i32,
                        detail: String::new(),
                    }
                }
                Err(err) => AccountActivationResult {
                    storefront: account.storefront,
                    outcome: ActivationOutcome::CredentialStale as i32,
                    detail: err.message().to_string(),
                },
            };

            results.push(outcome);
        }

        let profile = self
            .manager
            .activate(&profile_id)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(ActivateProfileResponse {
            profile: Some(profile),
            results,
        }))
    }

    type WatchActiveProfileStream =
        Pin<Box<dyn Stream<Item = Result<ProfileEvent, Status>> + Send + 'static>>;

    /// Server-streaming: UI subscribes instead of polling.
    async fn watch_active_profile(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::WatchActiveProfileStream>, Status> {
        let stream = self.manager.watch_active_profile().map(Ok);
        Ok(Response::new(Box::pin(stream)))
    }
}
