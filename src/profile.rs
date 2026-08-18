//! `bottles.profiles.v1.Profile` — a thin gRPC facade over
//! `bottles_core::profile::ProfileManager` for persistence and local
//! mutation, plus the Registry/Plugin dialing that manager
//! deliberately doesn't own (see its module docs): refreshing linked
//! accounts and revoking sessions.

use std::{collections::HashMap, pin::Pin};

use bottles_core::profile::{ProfileManager, error::ProfileError};
use futures_core::Stream;
use next_proto::bottles::{
    common::v1::Storefront,
    plugin::v1::{RefreshSessionRequest, plugin_client::PluginClient},
    profiles::v1::{
        AccountActivationResult, ActivateProfileRequest, ActivateProfileResponse,
        ActivationOutcome, CreateProfileRequest, DeleteProfileRequest, GetActiveProfileResponse,
        GetProfileRequest, ListProfilesResponse, ProfileEvent, RenameProfileRequest, UserProfile,
        profile_server::Profile,
    },
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
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
    pub fn new(manager: ProfileManager, registry: RegistryClient<Channel>) -> Self {
        Self {
            manager,
            registry: Mutex::new(registry),
        }
    }

    /// Resolves `storefront` to its owning plugin via the Registry and
    /// dials a fresh Plugin client — same pattern as PluginService, kept
    /// separate since ProfileService needs its own Registry connection.
    async fn plugin_client_for(
        &self,
        storefront: Storefront,
    ) -> Result<Option<PluginClient<Channel>>, Status> {
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

        let client = PluginClient::connect(endpoint.clone())
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
    /// session via the owning plugin and marks it AUTH_STATE_ACTIVE.
    /// Does not perform login from scratch — accounts with AUTH_STATE_STALE
    /// are reported back, not silently re-authenticated (that requires the
    /// interactive BeginLogin/CompleteLogin flow via PluginService and
    /// AccountsService.LinkAccount).
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

            let outcome = match self.plugin_client_for(storefront).await {
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
}
