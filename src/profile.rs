use std::{path::PathBuf, pin::Pin};

use directories::ProjectDirs;
use futures_core::Stream;
use next_config::Config;
use next_proto::bottles::{
    common::v1::{AuthState, Storefront},
    profiles::v1::{
        AccountActivationResult, ActivateProfileRequest, ActivateProfileResponse,
        ActivationOutcome, CreateProfileRequest, DeleteProfileRequest, GetActiveProfileResponse,
        GetProfileRequest, LinkAccountRequest, LinkSteamAccountRequest, ListProfilesResponse,
        ProfileEvent, RenameProfileRequest, SteamLink, SteamSessionEvent, UnlinkAccountRequest,
        UnlinkSteamAccountRequest, UserProfile, profile_event, profile_server::Profile,
    },
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
    store::v1::{
        CompleteLoginRequest, RefreshSessionRequest, RevokeSessionRequest,
        store_client::StoreClient,
    },
};
use prost_wkt_types::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tonic::{Request, Response, Result, Status, async_trait, transport::Channel};
use uuid::Uuid;

const EVENTS_CAPACITY: usize = 16;

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
    events: broadcast::Sender<ProfileEvent>,
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

        let (events, _) = broadcast::channel(EVENTS_CAPACITY);

        Ok(Self {
            path,
            state: RwLock::new(state),
            registry: Mutex::new(registry),
            events,
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

    /// Broadcasts a profile event to any active WatchActiveProfile
    /// subscribers. No-op (aside from the send failing silently) when
    /// nobody's currently watching.
    fn emit_updated(&self, profile: &UserProfile) {
        let _ = self.events.send(ProfileEvent {
            event: Some(profile_event::Event::Updated(profile.clone())),
        });
    }

    fn emit_activated(&self, profile: &UserProfile) {
        let _ = self.events.send(ProfileEvent {
            event: Some(profile_event::Event::Activated(profile.clone())),
        });
    }

    fn emit_deleted(&self, profile_id: &str) {
        let _ = self.events.send(ProfileEvent {
            event: Some(profile_event::Event::DeletedProfileId(
                profile_id.to_string(),
            )),
        });
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
        self.emit_updated(&profile);

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
        self.emit_deleted(&profile_id);
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
        self.emit_updated(&profile);
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
        self.emit_activated(&profile);

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
        self.emit_updated(&profile);
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

        {
            let state = self.state.read().await;
            if !state.profiles.iter().any(|p| p.id == req.profile_id) {
                return Err(Self::not_found(&req.profile_id));
            }
        }

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

        let mut state = self.state.write().await;
        let profile = state
            .profiles
            .iter_mut()
            .find(|profile| profile.id == req.profile_id)
            .ok_or_else(|| Self::not_found(&req.profile_id))?;
        profile
            .accounts
            .retain(|existing| existing.storefront != req.storefront);
        profile.accounts.push(account);
        let profile = profile.clone();

        self.persist(&state).await?;
        self.emit_updated(&profile);
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
        self.emit_updated(&profile);
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
        self.emit_updated(&profile);
        Ok(Response::new(profile))
    }

    type WatchActiveProfileStream =
        Pin<Box<dyn Stream<Item = Result<ProfileEvent, Status>> + Send + 'static>>;

    /// Server-streaming: UI subscribes instead of polling. Emits the
    /// current active profile (if any) as an initial Activated event, then
    /// forwards every subsequent mutation broadcast by the RPCs above.
    async fn watch_active_profile(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::WatchActiveProfileStream>, Status> {
        // Subscribe before reading state so no event can land in the gap
        // between snapshotting "current" and starting to listen for "next".
        let receiver = self.events.subscribe();

        let initial = {
            let state = self.state.read().await;
            state.active_profile_id.as_deref().and_then(|id| {
                state
                    .profiles
                    .iter()
                    .find(|profile| profile.id == id)
                    .cloned()
            })
        }
        .map(|profile| ProfileEvent {
            event: Some(profile_event::Event::Activated(profile)),
        });

        // A lagged receiver just means this subscriber missed some events
        // under backpressure — skip the gap rather than erroring the whole
        // stream out from under the caller.
        let live = BroadcastStream::new(receiver).filter_map(|item| item.ok());

        let stream = tokio_stream::iter(initial).chain(live).map(Ok);
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
