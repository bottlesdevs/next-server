//! `bottles.steam.v1.Steam` — links/unlinks a local Steam account to a
//! profile and streams OS-level Steam session changes. Split out of
//! `ProfileService`; the underlying `loginusers.vdf` watching lives in
//! `crate::steam`.

use std::pin::Pin;

use bottles_core::profile::{ProfileManager, error::ProfileError};
use futures_core::Stream;
use next_proto::bottles::steam::v1::{
    LinkSteamAccountRequest, SteamLink, SteamSessionEvent, UnlinkSteamAccountRequest,
    steam_server::Steam,
};
use tonic::{Request, Response, Result, Status, async_trait};

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

pub struct SteamService {
    manager: ProfileManager,
}

impl SteamService {
    pub fn new(manager: ProfileManager) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Steam for SteamService {
    /// Links a Steam account by ID. Looks up the display name from the
    /// local Steam install's loginusers.vdf on a best-effort basis (empty
    /// if Steam isn't installed or the ID isn't found there). `auto_activate`
    /// is accepted for backwards compatibility with callers that used to
    /// get activation folded into this call, but activation now happens
    /// through Profile.ActivateProfile directly.
    async fn link_steam_account(
        &self,
        request: Request<LinkSteamAccountRequest>,
    ) -> Result<Response<SteamLink>, Status> {
        let req = request.into_inner();

        let account_name = {
            let steam_id64 = req.steam_id64.clone();
            tokio::task::spawn_blocking(move || crate::steam::account_name_for(&steam_id64))
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
        };

        let steam_link = SteamLink {
            steam_id64: req.steam_id64.clone(),
            account_name,
        };

        self.manager
            .link_steam(&req.profile_id, steam_link.clone())
            .await
            .map_err(to_status)?;

        Ok(Response::new(steam_link))
    }

    async fn unlink_steam_account(
        &self,
        request: Request<UnlinkSteamAccountRequest>,
    ) -> Result<Response<()>, Status> {
        let profile_id = request.into_inner().profile_id;
        self.manager
            .unlink_steam(&profile_id)
            .await
            .map_err(to_status)?;
        Ok(Response::new(()))
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
