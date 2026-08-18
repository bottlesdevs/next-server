//! `bottles.accounts.v1.Accounts` — completes interactive storefront
//! logins (started via `Plugin.BeginLogin`) and attaches/removes the
//! resulting `LinkedAccount` on a profile. Split out of `ProfileService`
//! since it needs its own Registry/Plugin dialing, same as `ProfileService`
//! does for activation.

use bottles_core::profile::{ProfileManager, error::ProfileError};
use next_proto::bottles::{
    accounts::v1::{LinkAccountRequest, UnlinkAccountRequest, accounts_server::Accounts},
    common::v1::{LinkedAccount, Storefront},
    plugin::v1::{CompleteLoginRequest, RevokeSessionRequest, plugin_client::PluginClient},
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Result, Status, async_trait, transport::Channel};

fn to_status(err: bottles_core::Error) -> Status {
    match &err {
        bottles_core::Error::Status(status) => status.clone(),
        bottles_core::Error::Profile(ProfileError::NotFound(_)) => Status::not_found(err.to_string()),
        _ => Status::internal(err.to_string()),
    }
}

pub struct AccountsService {
    manager: ProfileManager,
    registry: Mutex<RegistryClient<Channel>>,
}

impl AccountsService {
    pub fn new(manager: ProfileManager, registry: RegistryClient<Channel>) -> Self {
        Self {
            manager,
            registry: Mutex::new(registry),
        }
    }

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
impl Accounts for AccountsService {
    /// Completes an interactive login started via Plugin.BeginLogin and
    /// attaches the resulting LinkedAccount to the profile. Verifies the
    /// profile exists up front so a bad profile_id fails fast instead of
    /// burning the plugin's one-shot login challenge for nothing.
    async fn link_account(
        &self,
        request: Request<LinkAccountRequest>,
    ) -> Result<Response<LinkedAccount>, Status> {
        let req = request.into_inner();

        self.manager
            .ensure_exists(&req.profile_id)
            .await
            .map_err(to_status)?;

        let storefront = Storefront::try_from(req.storefront)
            .map_err(|_| Status::invalid_argument("invalid storefront"))?;

        let mut client = self
            .plugin_client_for(storefront)
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

        self.manager
            .link_account(&req.profile_id, account.clone())
            .await
            .map_err(to_status)?;
        Ok(Response::new(account))
    }

    async fn unlink_account(
        &self,
        request: Request<UnlinkAccountRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();

        // Best-effort: revoke on the owning plugin before dropping the
        // LinkedAccount. A plugin that's unreachable shouldn't block
        // unlinking on our side — log and proceed regardless.
        if let Ok(storefront) = Storefront::try_from(req.storefront) {
            match self.plugin_client_for(storefront).await {
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

        self.manager
            .unlink_account(&req.profile_id, req.storefront)
            .await
            .map_err(to_status)?;
        Ok(Response::new(()))
    }
}
