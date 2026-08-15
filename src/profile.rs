use std::{path::PathBuf, pin::Pin};

use directories::ProjectDirs;
use futures_core::Stream;
use next_config::Config;
use next_proto::bottles::{
    common::v1::{AuthState, Storefront},
    profiles::v1::{
        AccountActivationResult, ActivateProfileRequest, ActivateProfileResponse,
        ActivationOutcome, CreateProfileRequest, DeleteProfileRequest, GetActiveProfileResponse,
        GetProfileRequest, LinkSteamAccountRequest, ListProfilesResponse, ProfileEvent,
        RenameProfileRequest, SteamLink, SteamSessionEvent, UnlinkAccountRequest,
        UnlinkSteamAccountRequest, UserProfile, profile_server::Profile,
    },
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
    store::v1::{RefreshSessionRequest, RevokeSessionRequest, store_client::StoreClient},
};
use prost_wkt_types::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tonic::{Request, Response, Result, Status, async_trait, transport::Channel};
use uuid::Uuid;

const PROFILES_FILE: &str = "profiles.toml";

#[derive(Debug, Default, Clone, Serialize, Deserialize, Config)]
#[config(version = 1)]
struct ProfilesConfig {
    active_profile_id: Option<String>,
    profiles: Vec<UserProfile>,
}

fn profiles_path() -> Result<PathBuf, Status> {
    ProjectDirs::from("com", "usebottles", "bottles-next")
        .map(|dirs| dirs.config_dir().join(PROFILES_FILE))
        .ok_or_else(|| Status::internal("could not resolve the config directory"))
}

fn now() -> Timestamp {
    Timestamp::from(std::time::SystemTime::now())
}

pub struct ProfileService {
    path: PathBuf,
    state: RwLock<ProfilesConfig>,
    registry: Mutex<RegistryClient<Channel>>,
}

impl ProfileService {
    pub async fn new(registry: RegistryClient<Channel>) -> Result<Self, Status> {
        let path = profiles_path()?;

        let state = match next_config::load::<ProfilesConfig>(&path).await {
            Ok(state) => state,
            Err(next_config::error::Error::Io(err))
                if err.kind() == std::io::ErrorKind::NotFound =>
            {
                ProfilesConfig::default()
            }
            Err(err) => {
                return Err(Status::internal(format!(
                    "failed to load {}: {err}",
                    path.display()
                )));
            }
        };

        Ok(Self {
            path,
            state: RwLock::new(state),
            registry: Mutex::new(registry),
        })
    }

    async fn persist(&self, state: &ProfilesConfig) -> Result<(), Status> {
        next_config::save(&self.path, state)
            .await
            .map_err(|err| Status::internal(format!("failed to save profiles: {err}")))
    }

    fn not_found(profile_id: &str) -> Status {
        Status::not_found(format!("no profile with id {profile_id}"))
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
        let state = self.state.read().await;
        Ok(Response::new(ListProfilesResponse {
            profiles: state.profiles.clone(),
        }))
    }

    async fn get_profile(
        &self,
        request: Request<GetProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let profile_id = request.into_inner().profile_id;
        let state = self.state.read().await;
        let profile = state
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
            .ok_or_else(|| Self::not_found(&profile_id))?;
        Ok(Response::new(profile))
    }

    async fn create_profile(
        &self,
        request: Request<CreateProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let req = request.into_inner();
        let mut state = self.state.write().await;

        let profile = UserProfile {
            id: Uuid::new_v4().to_string(),
            name: req.name,
            icon: req.icon,
            accounts: Vec::new(),
            steam_link: None,
            created_at: Some(now()),
            last_activated_at: None,
        };

        state.profiles.push(profile.clone());
        self.persist(&state).await?;

        Ok(Response::new(profile))
    }

    async fn delete_profile(
        &self,
        request: Request<DeleteProfileRequest>,
    ) -> Result<Response<()>, Status> {
        let profile_id = request.into_inner().profile_id;
        let mut state = self.state.write().await;

        let len_before = state.profiles.len();
        state.profiles.retain(|profile| profile.id != profile_id);
        if state.profiles.len() == len_before {
            return Err(Self::not_found(&profile_id));
        }

        if state.active_profile_id.as_deref() == Some(profile_id.as_str()) {
            state.active_profile_id = None;
        }

        self.persist(&state).await?;
        Ok(Response::new(()))
    }

    async fn rename_profile(
        &self,
        request: Request<RenameProfileRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let req = request.into_inner();
        let mut state = self.state.write().await;

        let profile = state
            .profiles
            .iter_mut()
            .find(|profile| profile.id == req.profile_id)
            .ok_or_else(|| Self::not_found(&req.profile_id))?;
        profile.name = req.name;
        let profile = profile.clone();

        self.persist(&state).await?;
        Ok(Response::new(profile))
    }

    async fn get_active_profile(
        &self,
        _request: Request<()>,
    ) -> Result<Response<GetActiveProfileResponse>, Status> {
        let state = self.state.read().await;
        let profile = state.active_profile_id.as_deref().and_then(|id| {
            state
                .profiles
                .iter()
                .find(|profile| profile.id == id)
                .cloned()
        });
        Ok(Response::new(GetActiveProfileResponse { profile }))
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

        let accounts = {
            let state = self.state.read().await;
            let profile = state
                .profiles
                .iter()
                .find(|profile| profile.id == req.profile_id)
                .ok_or_else(|| Self::not_found(&req.profile_id))?;
            profile.accounts.clone()
        };

        let targets = accounts.into_iter().filter(|account| {
            req.only.is_empty() || req.only.contains(&account.storefront)
        });

        let mut results = Vec::new();
        // Ok(account) replaces the stored LinkedAccount with the refreshed
        // one; Err marks it stale in place without touching other fields.
        let mut updates: std::collections::HashMap<i32, std::result::Result<_, ()>> =
            std::collections::HashMap::new();

        for account in targets {
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

        let mut state = self.state.write().await;
        let profile = state
            .profiles
            .iter_mut()
            .find(|profile| profile.id == req.profile_id)
            .ok_or_else(|| Self::not_found(&req.profile_id))?;

        for account in &mut profile.accounts {
            match updates.remove(&account.storefront) {
                Some(Ok(refreshed)) => *account = refreshed,
                Some(Err(())) => account.auth_state = AuthState::Stale as i32,
                None => {}
            }
        }
        profile.last_activated_at = Some(now());
        let profile = profile.clone();

        state.active_profile_id = Some(req.profile_id);
        self.persist(&state).await?;

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

        let mut state = self.state.write().await;
        let profile = state
            .profiles
            .iter_mut()
            .find(|profile| profile.id == req.profile_id)
            .ok_or_else(|| Self::not_found(&req.profile_id))?;
        profile
            .accounts
            .retain(|account| account.storefront != req.storefront);
        let profile = profile.clone();

        self.persist(&state).await?;
        Ok(Response::new(profile))
    }

    async fn link_steam_account(
        &self,
        request: Request<LinkSteamAccountRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let req = request.into_inner();
        let mut state = self.state.write().await;

        let profile = state
            .profiles
            .iter_mut()
            .find(|profile| profile.id == req.profile_id)
            .ok_or_else(|| Self::not_found(&req.profile_id))?;
        profile.steam_link = Some(SteamLink {
            steam_id64: req.steam_id64,
            account_name: String::new(),
        });
        let profile = profile.clone();

        self.persist(&state).await?;
        Ok(Response::new(profile))
    }

    async fn unlink_steam_account(
        &self,
        request: Request<UnlinkSteamAccountRequest>,
    ) -> Result<Response<UserProfile>, Status> {
        let profile_id = request.into_inner().profile_id;
        let mut state = self.state.write().await;

        let profile = state
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
            .ok_or_else(|| Self::not_found(&profile_id))?;
        profile.steam_link = None;
        let profile = profile.clone();

        self.persist(&state).await?;
        Ok(Response::new(profile))
    }

    type WatchActiveProfileStream =
        Pin<Box<dyn Stream<Item = Result<ProfileEvent, Status>> + Send + 'static>>;

    /// Server-streaming: UI subscribes instead of polling.
    async fn watch_active_profile(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::WatchActiveProfileStream>, Status> {
        // TODO: replace with a real broadcast channel fed by every mutation
        // above, instead of a one-shot snapshot of the current state.
        let state = self.state.read().await;
        let active = state.active_profile_id.as_deref().and_then(|id| {
            state
                .profiles
                .iter()
                .find(|profile| profile.id == id)
                .cloned()
        });
        let events = match active {
            Some(profile) => vec![Ok(ProfileEvent {
                event: Some(
                    next_proto::bottles::profiles::v1::profile_event::Event::Activated(profile),
                ),
            })],
            None => Vec::new(),
        };
        let stream = tokio_stream::iter(events);
        Ok(Response::new(Box::pin(stream)))
    }

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
        // TODO: real filesystem watch on Steam's loginusers.vdf/registry.vdf.
        let stream = tokio_stream::iter(Vec::new());
        Ok(Response::new(Box::pin(stream)))
    }
}
