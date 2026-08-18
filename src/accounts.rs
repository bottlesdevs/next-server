//! `bottles.accounts.v1.Accounts` — completes interactive storefront
//! logins (started via `Plugin.BeginLogin`) and attaches/removes the
//! resulting `LinkedAccount` on a profile. Split out of `ProfileService`
//! since it needs its own Registry/Plugin dialing, same as `ProfileService`
//! does for activation.

use bottles_core::{
    accounts::AccountManager,
    profile::{ProfileManager, error::ProfileError},
};
use next_proto::bottles::{
    accounts::v1::{
        AccountActivationResult, ActivateAccountsRequest, ActivateAccountsResponse,
        ActivationOutcome, LinkProfileRequest, RefreshAccountRequest, UnlinkProfileRequest,
        accounts_server::Accounts,
    },
    common::v1::{LinkedAccount, Storefront},
    plugin::v1::{
        CompleteLoginRequest, RefreshSessionRequest, RevokeSessionRequest,
        plugin_client::PluginClient,
    },
    registry::v1::{ResolvePluginRequest, registry_client::RegistryClient},
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Result, Status, async_trait, transport::Channel};

fn to_status(err: bottles_core::Error) -> Status {
    match &err {
        bottles_core::Error::Status(status) => status.clone(),
        bottles_core::Error::Profile(ProfileError::NotFound(_)) => {
            Status::not_found(err.to_string())
        }
        _ => Status::internal(err.to_string()),
    }
}

pub struct AccountsService {
    manager: ProfileManager,
    accounts: AccountManager,
    registry: Mutex<RegistryClient<Channel>>,
}

impl AccountsService {
    pub fn new(
        manager: ProfileManager,
        accounts: AccountManager,
        registry: RegistryClient<Channel>,
    ) -> Self {
        Self {
            manager,
            accounts,
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
    async fn link_profile(
        &self,
        request: Request<LinkProfileRequest>,
    ) -> Result<Response<LinkedAccount>, Status> {
        let LinkProfileRequest {
            profile_id,
            challenge_id,
            storefront,
            user_input,
        } = request.into_inner();

        self.manager.get(&profile_id).await.map_err(to_status)?;

        let storefront = Storefront::try_from(storefront)
            .map_err(|_| Status::invalid_argument("invalid storefront"))?;

        let mut client = self.plugin_client_for(storefront).await?.ok_or_else(|| {
            Status::unavailable(format!("no plugin registered for {storefront:?}"))
        })?;

        let account = client
            .complete_login(CompleteLoginRequest {
                challenge_id: challenge_id,
                profile_id: profile_id.clone(),
                storefront: storefront as i32,
                user_input: user_input,
            })
            .await?
            .into_inner();

        self.accounts
            .link_profile(&profile_id, account.clone())
            .await
            .map_err(to_status)?;
        Ok(Response::new(account))
    }

    async fn refresh_account(
        &self,
        request: Request<RefreshAccountRequest>,
    ) -> Result<Response<LinkedAccount>, Status> {
        let req = request.into_inner();
        let storefront = Storefront::try_from(req.storefront)
            .map_err(|_| Status::invalid_argument("invalid storefront"))?;
        let mut client = self.plugin_client_for(storefront).await?.ok_or_else(|| {
            Status::unavailable(format!("no plugin registered for {storefront:?}"))
        })?;
        let account = client
            .refresh_session(RefreshSessionRequest {
                profile_id: req.profile_id,
                storefront: storefront as i32,
            })
            .await
            .map_err(Status::from)?;
        Ok(account)
    }

    async fn unlink_profile(
        &self,
        request: Request<UnlinkProfileRequest>,
    ) -> Result<Response<()>, Status> {
        let UnlinkProfileRequest {
            profile_id,
            storefront,
        } = request.into_inner();

        let storefront = Storefront::try_from(storefront)
            .map_err(|_| Status::not_found("invalid storefront"))?;
        match self.plugin_client_for(storefront).await {
            Ok(Some(mut client)) => {
                if let Err(err) = client
                    .revoke_session(RevokeSessionRequest {
                        profile_id: profile_id.clone(),
                        storefront: storefront as i32,
                    })
                    .await
                {
                    tracing::warn!("{storefront:?} RevokeSession failed: {err}");
                }
            }
            Ok(None) => {
                tracing::debug!("no plugin registered for {storefront:?}, skipping revoke")
            }
            Err(err) => tracing::warn!("failed to reach {storefront:?} plugin: {err}"),
        }

        self.accounts
            .unlink_profile(&profile_id, storefront)
            .await
            .map_err(to_status)?;
        Ok(Response::new(()))
    }

    /// Refreshes credentials for a profile's linked accounts (or just
    /// `only`, if given) via their owning plugins, persisting any updates.
    async fn activate_accounts(
        &self,
        request: Request<ActivateAccountsRequest>,
    ) -> Result<Response<ActivateAccountsResponse>, Status> {
        let ActivateAccountsRequest { profile_id, only } = request.into_inner();

        let mut profile = self.manager.get(&profile_id).await.map_err(to_status)?;

        let mut results = Vec::new();

        for account in profile.accounts.clone() {
            let Ok(storefront) = Storefront::try_from(account.storefront) else {
                continue;
            };

            if !only.is_empty() && !only.contains(&(storefront as i32)) {
                continue;
            }

            let outcome = match self.plugin_client_for(storefront).await {
                Ok(Some(mut client)) => match client
                    .refresh_session(RefreshSessionRequest {
                        profile_id: profile_id.clone(),
                        storefront: storefront as i32,
                    })
                    .await
                {
                    Ok(response) => {
                        let updated = response.into_inner();
                        if let Some(existing) = profile
                            .accounts
                            .iter_mut()
                            .find(|a| a.storefront == account.storefront)
                        {
                            *existing = updated;
                        }
                        AccountActivationResult {
                            storefront: storefront as i32,
                            outcome: ActivationOutcome::Success as i32,
                            detail: String::new(),
                        }
                    }
                    Err(err) => AccountActivationResult {
                        storefront: storefront as i32,
                        outcome: ActivationOutcome::CredentialStale as i32,
                        detail: err.message().to_string(),
                    },
                },
                Ok(None) => AccountActivationResult {
                    storefront: storefront as i32,
                    outcome: ActivationOutcome::PluginUnavailable as i32,
                    detail: format!("no plugin registered for {storefront:?}"),
                },
                Err(status) => AccountActivationResult {
                    storefront: storefront as i32,
                    outcome: ActivationOutcome::NetworkError as i32,
                    detail: status.message().to_string(),
                },
            };

            results.push(outcome);
        }

        self.manager.update(profile).await.map_err(to_status)?;

        Ok(Response::new(ActivateAccountsResponse { results }))
    }
}
