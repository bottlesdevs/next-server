//! `bottles.profiles.v1.Profile` — a thin gRPC facade over
//! `bottles_core::profile::ProfileManager` for persistence and local
//! mutation, plus the Registry/Store-plugin dialing that manager
//! deliberately doesn't own (see its module docs): refreshing linked
//! accounts, completing interactive logins, and revoking sessions.

use std::{collections::HashMap, pin::Pin};

use bottles_core::profile::{ProfileManager, error::ProfileError};
use futures_core::Stream;
use next_proto::bottles::{
    common::v1::Storefront,
    profiles::v1::{
        AccountActivationResult, ActivateProfileRequest, ActivateProfileResponse,
        ActivationOutcome, CreateProfileRequest, DeleteProfileRequest, GetActiveProfileResponse,
        GetProfileRequest, LinkAccountRequest, LinkSteamAccountRequest, ListProfilesResponse,
        ProfileEvent, RenameProfileRequest, SteamLink, SteamSessionEvent, UnlinkAccountRequest,
        UnlinkSteamAccountRequest, UserProfile, profile_server::Profile,
    },
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
    store::v1::{
        CompleteLoginRequest, RefreshSessionRequest, RevokeSessionRequest,
        store_client::StoreClient,
    },
};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Result, Status, async_trait, transport::Channel};

fn to_status(err: bottles_core::Error) -> Status {
    match &err {
        bottles_core::Error::Status(status) => status.clone(),
        bottles_core::Error::Profile(ProfileError::NotFound(_)) => Status::not_found(err.to_string()),
        bottles_core::Error::Profile(ProfileError::SteamAccountAlreadyLinked { .. }) => {
            Status::already_exists(err.to_string())
        }
        _ => Status::internal(err.to_string()),
    }
}

pub struct ProfileService {
    manager: ProfileManager,
    registry: Mutex<RegistryClient<Channel>>,
}

impl ProfileService {
    pub async fn new(registry: RegistryClient<Channel>) -> Result<Self, Status> {
        let manager = ProfileManager::load().await.map_err(to_status)?;
        Ok(Self {
            manager,
            registry: Mutex::new(registry),
        })
    }

    /// Resolves `storefront` to its owning plugin via the Registry and
    /// dials a fresh Store client — same pattern as StoreService, kept
    /// separate since ProfileService needs its own Registry connection.
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
        let profile = self.manager.get(&profile_id).await.map_err(to_status)?;
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
            .map_err(to_status)?;
        Ok(Response::new(profile))
    }

    async fn delete_profile(
        &self,
        request: Request<DeleteProfileRequest>,
    ) -> Result<Response<()>, Status> {
        let profile_id = request.into_inner().profile_id;
        self.manager.delete(&profile_id).await.map_err(to_status)?;
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
            .map_err(to_status)?;
        Ok(Response::new(profile))
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
    /// session via the owning StorePlugin and marks it AUTH_STATE_ACTIVE.
    /// Does not perform login from scratch — accounts with AUTH_STATE_STALE
    /// are reported back, not silently re-authenticated (that requires the
    /// interactive BeginLogin/CompleteLogin flow in StoreService).
    async fn activate_profile(
        &self,
        request: Request<ActivateProfileRequest>,
    ) -> Result<Response<ActivateProfileResponse>, Status> {
        let req = request.into_inner();

        let accounts = self
            .manager
            .accounts_for_activation(&req.profile_id, &req.only)
            .await
            .map_err(to_status)?;

        let mut results = Vec::new();
        // Ok(account) replaces the stored LinkedAccount with the refreshed
        // one; Err marks it stale in place without touching other fields.
        let mut updates: HashMap<i32, std::result::Result<_, ()>> = HashMap::new();

        for account in accounts {
            let Ok(storefront) = Storefront::try_from(account.storefront) else {
                continue;
            };

            let outcome = match self.store_client_for(storefront).await {
                Ok(Some(mut client)) => {
                    match client
                        .refresh_session(RefreshSessionRequest {
                            profile_id: req.profile_id.clone(),
                            storefront: account.storefront,
                        })
                        .await
                    {
                        Ok(response) => {
                            updates.insert(account.storefront, Ok(response.into_inner()));
                            AccountActivationResult {
                                storefront: account.storefront,
                                outcome: ActivationOutcome::Success as i32,
                                detail: String::new(),
                            }
                        }
                        Err(err) => {
                            updates.insert(account.storefront, Err(()));
                            AccountActivationResult {
                                storefront: account.storefront,
                                outcome: ActivationOutcome::CredentialStale as i32,
                                detail: err.message().to_string(),
                            }
                        }
                    }
                }
                Ok(None) => AccountActivationResult {
                    storefront: account.storefront,
                    outcome: ActivationOutcome::PluginUnavailable as i32,
                    detail: format!("no plugin registered for {storefront:?}"),
                },
                Err(err) => AccountActivationResult {
                    storefront: account.storefront,
                    outcome: ActivationOutcome::NetworkError as i32,
                    detail: err.message().to_string(),
                },
            };

            results.push(outcome);
        }

        let profile = self
            .manager
            .apply_activation(&req.profile_id, updates)
            .await
            .map_err(to_status)?;

        Ok(Response::new(ActivateProfileResponse {
            profile: Some(profile),
            results,
        }))
    }

    async fn unlink_account(
        &self,
        request: Request<UnlinkAccountRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let req = request.into_inner();

        // Best-effort: revoke on the owning plugin before dropping the
        // LinkedAccount. A plugin that's unreachable shouldn't block
        // unlinking on our side — log and proceed regardless.
        if let Ok(storefront) = Storefront::try_from(req.storefront) {
            match self.store_client_for(storefront).await {
                Ok(Some(mut client)) => {
                    if let Err(err) = client
                        .revoke_session(RevokeSessionRequest {
                            profile_id: req.profile_id.clone(),
                            storefront: req.storefront,
                        })
                        .await
                    {
                        tracing::warn!("{storefront:?} RevokeSession failed: {err}");
                    }
                }
                Ok(None) => tracing::debug!("no plugin registered for {storefront:?}, skipping revoke"),
                Err(err) => tracing::warn!("failed to reach {storefront:?} plugin: {err}"),
            }
        }

        let profile = self
            .manager
            .unlink_account(&req.profile_id, req.storefront)
            .await
            .map_err(to_status)?;
        Ok(Response::new(profile))
    }

    /// Completes an interactive login started via Store.BeginLogin and
    /// attaches the resulting LinkedAccount to the profile. Verifies the
    /// profile exists up front so a bad profile_id fails fast instead of
    /// burning the plugin's one-shot login challenge for nothing.
    async fn link_account(
        &self,
        request: Request<LinkAccountRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let req = request.into_inner();

        self.manager
            .ensure_exists(&req.profile_id)
            .await
            .map_err(to_status)?;

        let storefront = Storefront::try_from(req.storefront)
            .map_err(|_| Status::invalid_argument("invalid storefront"))?;

        let mut client = self
            .store_client_for(storefront)
            .await?
            .ok_or_else(|| {
                Status::unavailable(format!("no plugin registered for {storefront:?}"))
            })?;

        let account = client
            .complete_login(CompleteLoginRequest {
                challenge_id: req.challenge_id,
                profile_id: req.profile_id.clone(),
                storefront: req.storefront,
                user_input: req.user_input,
            })
            .await?
            .into_inner();

        let profile = self
            .manager
            .link_account(&req.profile_id, account)
            .await
            .map_err(to_status)?;
        Ok(Response::new(profile))
    }

    /// Links a Steam account by ID. Looks up the display name from the
    /// local Steam install's loginusers.vdf on a best-effort basis (empty
    /// if Steam isn't installed or the ID isn't found there). When
    /// `auto_activate` is set, immediately runs the same activation path
    /// as ActivateProfile so linked storefront accounts get refreshed too.
    async fn link_steam_account(
        &self,
        request: Request<LinkSteamAccountRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let req = request.into_inner();

        let account_name = {
            let steam_id64 = req.steam_id64.clone();
            tokio::task::spawn_blocking(move || crate::steam::account_name_for(&steam_id64))
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
        };

        self.manager
            .link_steam(
                &req.profile_id,
                SteamLink {
                    steam_id64: req.steam_id64.clone(),
                    account_name,
                },
            )
            .await
            .map_err(to_status)?;

        if req.auto_activate {
            let activated = self
                .activate_profile(Request::new(ActivateProfileRequest {
                    profile_id: req.profile_id.clone(),
                    only: Vec::new(),
                }))
                .await?
                .into_inner();
            let profile = activated
                .profile
                .ok_or_else(|| Status::internal("activation didn't return a profile"))?;
            return Ok(Response::new(profile));
        }

        let profile = self
            .manager
            .get(&req.profile_id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(profile))
    }

    async fn unlink_steam_account(
        &self,
        request: Request<UnlinkSteamAccountRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let profile_id = request.into_inner().profile_id;
        let profile = self
            .manager
            .unlink_steam(&profile_id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(profile))
    }

    type WatchActiveProfileStream =
        Pin<Box<dyn Stream<Item = Result<ProfileEvent, Status>> + Send + 'static>>;

    /// Server-streaming: UI subscribes instead of polling.
    async fn watch_active_profile(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::WatchActiveProfileStream>, Status> {
        let stream = self.manager.watch().map(Ok);
        Ok(Response::new(Box::pin(stream)))
    }

    type WatchSteamSessionsStream =
        Pin<Box<dyn Stream<Item = Result<SteamSessionEvent, Status>> + Send + 'static>>;

    /// Server-streaming: fires when Bottles detects the OS-level Steam
    /// active user has changed (via filesystem watch on loginusers.vdf).
    async fn watch_steam_sessions(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::WatchSteamSessionsStream>, Status> {
        Ok(Response::new(crate::steam::watch_active_user()))
    }
}
