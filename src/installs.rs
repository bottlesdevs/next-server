//! Local install-state persistence for `Library`.
//!
//! Storefront plugins have no notion of "installed" — that's purely
//! local state Library owns itself, keyed by (profile_id, storefront,
//! game_id). This mirrors `ProfileService`'s `next_config`-backed
//! persistence pattern in `next-server/src/profile.rs`.

use std::path::PathBuf;

use next_config::Config;
use next_proto::bottles::common::v1::{InstallState, Storefront};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tonic::Status;

const INSTALLS_FILE: &str = "installs.toml";

#[derive(Debug, Default, Clone, Serialize, Deserialize, Config)]
#[config(version = 1)]
struct InstallsConfig {
    installs: Vec<InstallRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallRecord {
    pub profile_id: String,
    pub storefront: i32,
    pub game_id: String,
    pub version: String,
    pub install_size_bytes: Option<u64>,
    /// Paths installed relative to this record's install directory,
    /// recorded at install time so `UninstallGame` cleans up exactly
    /// what was written even if the manifest changes later.
    pub relative_paths: Vec<String>,
}

impl InstallRecord {
    fn matches(&self, profile_id: &str, storefront: Storefront, game_id: &str) -> bool {
        self.profile_id == profile_id
            && self.storefront == storefront as i32
            && self.game_id == game_id
    }

    pub fn install_state(&self) -> InstallState {
        InstallState {
            installed: true,
            bottle_id: None,
            installed_version: Some(self.version.clone()),
            install_size_bytes: self.install_size_bytes,
        }
    }
}

fn installs_path() -> Result<PathBuf, Status> {
    directories::ProjectDirs::from("com", "usebottles", "bottles-next")
        .map(|dirs| dirs.config_dir().join(INSTALLS_FILE))
        .ok_or_else(|| Status::internal("could not resolve the config directory"))
}

/// Where a game's files are written. Not part of `InstallsConfig` itself
/// since it's derived, not persisted.
pub fn install_dir(profile_id: &str, storefront: Storefront, game_id: &str) -> Result<PathBuf, Status> {
    directories::ProjectDirs::from("com", "usebottles", "bottles-next")
        .map(|dirs| {
            dirs.data_dir()
                .join("installs")
                .join(profile_id)
                .join(storefront.as_str_name())
                .join(game_id)
        })
        .ok_or_else(|| Status::internal("could not resolve the data directory"))
}

pub struct InstallsStore {
    path: PathBuf,
    state: RwLock<InstallsConfig>,
}

impl InstallsStore {
    pub async fn load() -> Result<Self, Status> {
        let path = installs_path()?;
        let state = match next_config::load::<InstallsConfig>(&path).await {
            Ok(state) => state,
            Err(next_config::error::Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                InstallsConfig::default()
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
        })
    }

    pub async fn get(
        &self,
        profile_id: &str,
        storefront: Storefront,
        game_id: &str,
    ) -> Option<InstallRecord> {
        self.state
            .read()
            .await
            .installs
            .iter()
            .find(|record| record.matches(profile_id, storefront, game_id))
            .cloned()
    }

    async fn persist(&self, state: &InstallsConfig) -> Result<(), Status> {
        next_config::save(&self.path, state)
            .await
            .map_err(|err| Status::internal(format!("failed to save installs: {err}")))
    }

    pub async fn upsert(&self, record: InstallRecord) -> Result<(), Status> {
        let mut state = self.state.write().await;
        state
            .installs
            .retain(|existing| !existing.matches(&record.profile_id, storefront_of(&record), &record.game_id));
        state.installs.push(record);
        self.persist(&state).await
    }

    /// Removes and returns the record, if any. Callers still need to
    /// delete `install_dir(...)` themselves — this only updates the
    /// record.
    pub async fn remove(
        &self,
        profile_id: &str,
        storefront: Storefront,
        game_id: &str,
    ) -> Result<Option<InstallRecord>, Status> {
        let mut state = self.state.write().await;
        let index = state
            .installs
            .iter()
            .position(|record| record.matches(profile_id, storefront, game_id));
        let Some(index) = index else {
            return Ok(None);
        };
        let record = state.installs.remove(index);
        self.persist(&state).await?;
        Ok(Some(record))
    }
}

fn storefront_of(record: &InstallRecord) -> Storefront {
    Storefront::try_from(record.storefront).unwrap_or(Storefront::Unspecified)
}
