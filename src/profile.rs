use std::pin::Pin;

use futures_core::Stream;
use next_proto::bottles::profiles::v1::{
    ActivateProfileRequest, ActivateProfileResponse, CreateProfileRequest, DeleteProfileRequest,
    GetActiveProfileResponse, GetProfileRequest, LinkSteamAccountRequest, ListProfilesResponse,
    ProfileEvent, RenameProfileRequest, SteamSessionEvent, UnlinkAccountRequest,
    UnlinkSteamAccountRequest, UserProfile, profile_server::Profile,
};
use tonic::{Request, Response, Result, Status, async_trait};

pub struct ProfileService;

impl ProfileService {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Profile for ProfileService {
    /// Lists all profiles for the current user.
    async fn list_profiles(
        &self,
        __request: Request<()>,
    ) -> Result<Response<ListProfilesResponse>, Status> {
        Ok(Response::new(ListProfilesResponse::default()))
    }

    /// Retrieves a specific profile by its ID.
    async fn get_profile(
        &self,
        _request: Request<GetProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        Ok(Response::new(UserProfile::default()))
    }

    /// Creates a new profile for the current user.
    async fn create_profile(
        &self,
        _request: Request<CreateProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        Ok(Response::new(UserProfile::default()))
    }

    /// Deletes a profile by its ID.
    async fn delete_profile(
        &self,
        _request: Request<DeleteProfileRequest>,
    ) -> Result<Response<()>, Status> {
        Ok(Response::new(()))
    }

    /// Renames a profile by its ID.
    async fn rename_profile(
        &self,
        _request: Request<RenameProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        Ok(Response::new(UserProfile::default()))
    }

    /// Retrieves the active profile for the current user.
    async fn get_active_profile(
        &self,
        _request: Request<()>,
    ) -> Result<Response<GetActiveProfileResponse>, Status> {
        Ok(Response::new(GetActiveProfileResponse::default()))
    }

    /// Activates a profile: for every linked account, verifies/refreshes its
    /// session via the owning StorePlugin and marks it AUTH_STATE_ACTIVE.
    /// Does not perform login from scratch — accounts with AUTH_STATE_STALE
    /// are reported back, not silently re-authenticated (that requires the
    /// interactive BeginLogin/CompleteLogin flow in StoreService).
    async fn activate_profile(
        &self,
        _request: Request<ActivateProfileRequest>,
    ) -> Result<Response<ActivateProfileResponse>, Status> {
        Ok(Response::new(ActivateProfileResponse::default()))
    }
    /// Unlinks an account from a profile.
    async fn unlink_account(
        &self,
        _request: Request<UnlinkAccountRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        Ok(Response::new(UserProfile::default()))
    }
    /// Links a Steam account to a profile.
    async fn link_steam_account(
        &self,
        _request: Request<LinkSteamAccountRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        Ok(Response::new(UserProfile::default()))
    }

    /// Unlinks a Steam account from a profile.
    async fn unlink_steam_account(
        &self,
        _request: Request<UnlinkSteamAccountRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        Ok(Response::new(UserProfile::default()))
    }

    type WatchActiveProfileStream =
        Pin<Box<dyn Stream<Item = Result<ProfileEvent, Status>> + Send + 'static>>;

    /// Server-streaming: UI subscribes instead of polling.
    async fn watch_active_profile(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::WatchActiveProfileStream>, Status> {
        let stream = tokio_stream::iter(vec![Ok(ProfileEvent { event: None })]);
        Ok(Response::new(Box::pin(stream)))
    }

    /// Server streaming response type for the WatchSteamSessions method.
    type WatchSteamSessionsStream =
        Pin<Box<dyn Stream<Item = Result<SteamSessionEvent, Status>> + Send + 'static>>;

    /// Server-streaming: fires when Bottles detects the OS-level Steam
    /// active user has changed (via filesystem watch on loginusers.vdf /
    /// registry.vdf). Consumers that want auto-switch behavior listen here
    /// and call ActivateProfile themselves, or rely on next-core's own
    /// internal watcher doing so if auto-activation is enabled on the link.
    async fn watch_steam_sessions(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::WatchSteamSessionsStream>, Status> {
        let stream = tokio_stream::iter(vec![Ok(SteamSessionEvent {
            steam_id64: "".to_string(),
            account_name: "".to_string(),
            is_active: false,
        })]);
        Ok(Response::new(Box::pin(stream)))
    }
}
