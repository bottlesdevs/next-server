use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use bottles_core::layers::{LayerManager, LayerRef, LayersError, Mount, Session, Tools};
use bottles_core::proto::{self, bottles_server::Bottles, wine_bridge_client::WineBridgeClient};
use bottles_core::runner::Wine;
use bottles_core::LaunchRequest;
use tokio::sync::Mutex;

/// Default WineBridge endpoint used when `WINEBRIDGE_ADDR` is not set, ust match the
/// loopback address WineBridge binds to by default (`WINEBRIDGE_HOST`/`WINEBRIDGE_PORT`
/// in the agent), i.e. IPv4 `127.0.0.1:50051`.
const DEFAULT_WINEBRIDGE_ADDR: &str = "http://127.0.0.1:50051";

/// A live layered prefix held by the server so its mount (and, for a launched
/// session, its WineBridge agent) outlives the RPC that created it.
#[allow(dead_code)]
enum Active {
    Prepared(Mount),
    Launched(Session),
}

#[derive(Default)]
pub struct BottlesService {
    sessions: Mutex<HashMap<u64, Active>>,
    next_id: AtomicU64,
}

impl BottlesService {
    /// Resolves the WineBridge endpoint to forward requests to.
    ///
    /// Reads `WINEBRIDGE_ADDR` and falls back to [`DEFAULT_WINEBRIDGE_ADDR`].
    fn winebridge_addr() -> String {
        std::env::var("WINEBRIDGE_ADDR").unwrap_or_else(|_| DEFAULT_WINEBRIDGE_ADDR.to_string())
    }

    fn manager() -> LayerManager {
        LayerManager::new(Tools::from_env())
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

fn to_layers(layers: &[proto::Layer]) -> Vec<LayerRef> {
    layers
        .iter()
        .map(|l| LayerRef { repo: PathBuf::from(&l.repo), state: l.state.clone() })
        .collect()
}

fn layers_status(e: LayersError) -> tonic::Status {
    tonic::Status::internal(e.to_string())
}

fn core_status(e: bottles_core::Error) -> tonic::Status {
    tonic::Status::internal(e.to_string())
}

fn env_path(var: &str) -> Result<PathBuf, tonic::Status> {
    std::env::var(var)
        .map(PathBuf::from)
        .map_err(|_| tonic::Status::failed_precondition(format!("{var} not set")))
}

#[tonic::async_trait]
impl Bottles for BottlesService {
    async fn health(
        &self,
        request: tonic::Request<proto::HealthRequest>,
    ) -> Result<tonic::Response<proto::HealthResponse>, tonic::Status> {
        let request = request.get_ref();
        tracing::info!("Received request: {:?}", request);
        Ok(tonic::Response::new(proto::HealthResponse { ok: true }))
    }

    async fn notify(
        &self,
        request: tonic::Request<proto::NotifyRequest>,
    ) -> Result<tonic::Response<proto::NotifyResponse>, tonic::Status> {
        let request = request.get_ref();
        tracing::info!("Received request: {:?}", request);
        let addr = Self::winebridge_addr();
        let mut client = WineBridgeClient::connect(addr)
            .await
            .map_err(|e| tonic::Status::from_error(Box::new(e)))?;

        let request = proto::MessageRequest {
            message: request.message.clone(),
        };
        let response = client.message(request).await?;
        let response = response.get_ref();
        Ok(tonic::Response::new(proto::NotifyResponse {
            success: response.success,
        }))
    }

    async fn prepare_prefix(
        &self,
        request: tonic::Request<proto::PreparePrefixRequest>,
    ) -> Result<tonic::Response<proto::PrefixSession>, tonic::Status> {
        let req = request.into_inner();
        let lowers = to_layers(&req.lowers);
        let mount = Self::manager()
            .prepare(&lowers, &PathBuf::from(req.upper), &PathBuf::from(&req.mountpoint))
            .map_err(layers_status)?;
        let mountpoint = mount.path().display().to_string();
        let session_id = self.next_id();
        self.sessions.lock().await.insert(session_id, Active::Prepared(mount));
        Ok(tonic::Response::new(proto::PrefixSession { session_id, mountpoint }))
    }

    async fn launch_in_prefix(
        &self,
        request: tonic::Request<proto::LaunchInPrefixRequest>,
    ) -> Result<tonic::Response<proto::LaunchResult>, tonic::Status> {
        let req = request.into_inner();
        let lowers = to_layers(&req.lowers);
        let wine = Wine::new(env_path("WINE_BIN")?).map_err(core_status)?;
        let winebridge = env_path("WINEBRIDGE_BIN")?;
        let launch = LaunchRequest::new(&req.executable)
            .args(req.args)
            .terminal(req.terminal)
            .maybe_work_dir(req.work_dir.map(PathBuf::from));

        let session = Self::manager()
            .prepare_and_launch(
                &wine,
                winebridge,
                &lowers,
                &PathBuf::from(req.upper),
                &PathBuf::from(&req.mountpoint),
                launch,
            )
            .await
            .map_err(core_status)?;

        let mountpoint = session.mount.path().display().to_string();
        let pid = session.pid;
        let session_id = self.next_id();
        self.sessions.lock().await.insert(session_id, Active::Launched(session));
        Ok(tonic::Response::new(proto::LaunchResult { session_id, mountpoint, pid }))
    }

    async fn capture_layer(
        &self,
        request: tonic::Request<proto::CaptureLayerRequest>,
    ) -> Result<tonic::Response<proto::CaptureLayerResponse>, tonic::Status> {
        let req = request.into_inner();
        Self::manager()
            .capture(&PathBuf::from(&req.upper), &PathBuf::from(&req.base_dir), &req.message)
            .map_err(layers_status)?;
        Ok(tonic::Response::new(proto::CaptureLayerResponse { ok: true }))
    }

    async fn release_prefix(
        &self,
        request: tonic::Request<proto::ReleasePrefixRequest>,
    ) -> Result<tonic::Response<proto::ReleasePrefixResponse>, tonic::Status> {
        let session_id = request.into_inner().session_id;
        let ok = self.sessions.lock().await.remove(&session_id).is_some();
        Ok(tonic::Response::new(proto::ReleasePrefixResponse { ok }))
    }
}
