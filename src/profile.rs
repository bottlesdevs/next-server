//! `bottles.profiles.v1.Profile` — a thin gRPC facade over
//! `bottles_core::profile::ProfileManager` for persistence and local
//! mutation. Storefront credential refresh/activation lives on
//! `bottles.accounts.v1.Accounts` (`ActivateAccounts`) instead, since it
//! needs its own Registry/Plugin dialing that `ProfileManager`
//! deliberately doesn't own (see its module docs).

use std::pin::Pin;

use bottles_core::profile::ProfileManager;
use futures_core::Stream;
use next_proto::bottles::profiles::v1::{
    ActivateProfileRequest, CreateProfileRequest, DeleteProfileRequest,
    GetActiveProfileResponse, GetProfileRequest, ListProfilesResponse, ProfileEvent,
    RenameProfileRequest, UserProfile, profile_server::Profile,
};
use tokio_stream::StreamExt;
use tonic::{Request, Response, Result, Status, async_trait};

pub struct ProfileService {
    manager: ProfileManager,
}

impl ProfileService {
    pub fn new(manager: ProfileManager) -> Self {
        Self { manager }
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

    /// Marks a profile as the active one. Doesn't touch storefront
    /// credentials — see `Accounts.ActivateAccounts` for that.
    async fn activate_profile(
        &self,
        request: Request<ActivateProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let ActivateProfileRequest { profile_id } = request.into_inner();

        let profile = self
            .manager
            .activate(&profile_id)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(profile))
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
