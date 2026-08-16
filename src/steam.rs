//! Thin proto-facing wrapper over `bottles_core::steam` — all the actual
//! `loginusers.vdf` parsing/watching logic lives there now.

use std::pin::Pin;

use bottles_core::steam::SteamUser;
use futures_core::Stream;
use next_proto::bottles::profiles::v1::SteamSessionEvent;
use tokio_stream::StreamExt;
use tonic::Status;

pub use bottles_core::steam::{account_name_for, loginusers_vdf_path, parse_loginusers};

fn to_event(user: SteamUser) -> SteamSessionEvent {
    SteamSessionEvent {
        steam_id64: user.steam_id64,
        account_name: user.account_name,
        is_active: user.is_active,
    }
}

/// Wraps `bottles_core::steam::watch_active_user` for gRPC streaming —
/// maps each [`SteamUser`] to a `SteamSessionEvent` and the stream's
/// infallible items into `Result<_, Status>`.
pub fn watch_active_user()
-> Pin<Box<dyn Stream<Item = Result<SteamSessionEvent, Status>> + Send + 'static>> {
    Box::pin(bottles_core::steam::watch_active_user().map(|user| Ok(to_event(user))))
}
