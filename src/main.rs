use std::{sync::Arc, time::Duration};

use bottles_core::library::InstallsStore;
use bottles_server::{
    bottle::BottleService, library::LibraryService, profile::ProfileService, store::StoreService,
};
use download_manager::manager::{DownloadManager, DownloadManagerConfig};
use next_proto::bottles::{
    bottle::v1::bottle_server::BottleServer, library::v1::library_server::LibraryServer,
    profiles::v1::profile_server::ProfileServer, registry::v1::registry_client::RegistryClient,
    store::v1::store_server::StoreServer,
};
use tonic_health::server::health_reporter;
use tonic_reflection::server::Builder;
use tracing_subscriber::EnvFilter;

const LISTEN_ADDR: &str = "0.0.0.0:50052";
const REGISTRY_ENDPOINT: &str = "http://127.0.0.1:50250";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("bottles_server=trace")),
        )
        .init();

    let (health_reporter, health_service) = health_reporter();
    health_reporter
        .set_serving::<ProfileServer<ProfileService>>()
        .await;
    health_reporter
        .set_serving::<LibraryServer<LibraryService>>()
        .await;
    health_reporter
        .set_serving::<StoreServer<StoreService>>()
        .await;
    health_reporter
        .set_serving::<BottleServer<BottleService>>()
        .await;

    let reflection_service = Builder::configure()
        .register_encoded_file_descriptor_set(next_proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;

    // No timeout on reqwest's defaults means a chunk request that gets a
    // connection but then no further bytes (a stale/expired CDN URL, a
    // silently dropped response — the exact failure mode game-install
    // chunk downloads hit in practice) hangs forever instead of erroring,
    // stalling the whole install with no visible cause. `read_timeout`
    // bounds idle time between reads rather than total request time, so
    // a large chunk that's still actively transferring isn't penalized.
    let reqwest_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| format!("failed to build HTTP client: {err}"))?;
    let http_client = Arc::new(
        http_client::ReqwestClient::with_client(reqwest_client)
            .map_err(|err| format!("failed to build HTTP client: {err}"))?,
    );
    let downloads = Arc::new(
        DownloadManager::new(http_client, DownloadManagerConfig::default())
            .map_err(|err| format!("failed to start download manager: {err}"))?,
    );
    let installs = Arc::new(InstallsStore::load().await?);

    // Owns the addon catalog, downloader, and prefix/storage context every
    // Bottle handle shares; kept alive for the process's lifetime by this
    // binding outliving `.serve()` below. Left at its defaults (no FVS
    // daemon, default catalog URLs) — bottle creation/snapshot RPCs will
    // fail until those are actually configured for this environment.
    // Library needs the same BottleManager to install games directly
    // into a bottle's prefix.
    let bottles = bottles_core::Bottles::open(bottles_core::Config::default()).await?;
    let bottle_service = BottleService::new(bottles.bottles().clone());

    let library_registry_client = RegistryClient::connect(REGISTRY_ENDPOINT).await?;
    let library_service = LibraryService::new(
        library_registry_client,
        downloads,
        installs,
        bottles.bottles().clone(),
    );

    let store_registry_client = RegistryClient::connect(REGISTRY_ENDPOINT).await?;
    let store_service = StoreService::new(store_registry_client);

    let profile_registry_client = RegistryClient::connect(REGISTRY_ENDPOINT).await?;
    let profile_service = ProfileService::new(profile_registry_client).await?;

    tracing::info!("Server started on http://{LISTEN_ADDR}");

    tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(reflection_service)
        .add_service(ProfileServer::new(profile_service))
        .add_service(LibraryServer::new(library_service))
        .add_service(StoreServer::new(store_service))
        .add_service(BottleServer::new(bottle_service))
        .serve(LISTEN_ADDR.parse()?)
        .await?;

    Ok(())
}
