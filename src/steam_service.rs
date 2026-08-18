//! `bottles.steam.v1.Steam` — a thin gRPC facade over
//! `bottles_core::steam::SteamManager`, which owns linking/unlinking a
//! local Steam account and watching OS-level session changes.

use std::pin::Pin;

use bottles_core::{profile::error::ProfileError, steam::SteamManager};
use futures_core::Stream;
use next_proto::bottles::steam::v1::{
    LinkSteamAccountRequest, SteamLink, SteamSessionEvent, UnlinkSteamAccountRequest,
    steam_server::Steam,
};
use tokio_stream::StreamExt;
use tonic::{Request, Response, Result, Status, async_trait};

fn to_status(err: bottles_core::Error) -> Status {
    match &err {
        bottles_core::Error::Status(status) => status.clone(),
        bottles_core::Error::Profile(ProfileError::NotFound(_)) => {
            Status::not_found(err.to_string())
        }
        bottles_core::Error::Profile(ProfileError::SteamAccountAlreadyLinked { .. }) => {
            Status::already_exists(err.to_string())
        }
        _ => Status::internal(err.to_string()),
    }
}

pub struct SteamService {
    manager: SteamManager,
}

impl SteamService {
    pub fn new(manager: SteamManager) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Steam for SteamService {
    /// `auto_activate` is accepted for backwards compatibility with
    /// callers that used to get activation folded into this call, but
    /// activation now happens through Profile.ActivateProfile directly.
    async fn link_steam_account(
        &self,
        request: Request<LinkSteamAccountRequest>,
    ) -> Result<Response<SteamLink>, Status> {
        let req = request.into_inner();
        let steam_link = self
            .manager
            .link_account(&req.profile_id, req.steam_id64)
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
            .unlink_account(&profile_id)
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
        Ok(Response::new(Box::pin(
            self.manager.watch_sessions().map(Ok),
        )))
    }
}
