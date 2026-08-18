use std::{sync::Arc, time::Duration};

use bottles_core::{library::InstallsStore, profile::ProfileManager};
use bottles_server::{
    accounts::AccountsService, bottle::BottleService, library::LibraryService,
    plugin::PluginService, profile::ProfileService, steam_service::SteamService,
};
use download_manager::manager::{DownloadManager, DownloadManagerConfig};
use next_proto::bottles::{
    accounts::v1::accounts_server::AccountsServer, bottle::v1::bottle_server::BottleServer,
    library::v1::library_server::LibraryServer, plugin::v1::plugin_server::PluginServer,
    profiles::v1::profile_server::ProfileServer, registry::v1::registry_client::RegistryClient,
    steam::v1::steam_server::SteamServer,
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
        .set_serving::<PluginServer<PluginService>>()
        .await;
    health_reporter
        .set_serving::<SteamServer<SteamService>>()
        .await;
    health_reporter
        .set_serving::<AccountsServer<AccountsService>>()
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

    let bottles = bottles_core::Bottles::open(bottles_core::Config::default()).await?;
    let bottle_service = BottleService::new(bottles.bottles().clone());

    let library_registry_client = RegistryClient::connect(REGISTRY_ENDPOINT).await?;
    let profile = ProfileManager::load().await?;
    let library_service = LibraryService::new(
        library_registry_client,
        downloads,
        installs,
        profile,
        bottles.bottles().clone(),
    );

    let plugin_registry_client = RegistryClient::connect(REGISTRY_ENDPOINT).await?;
    let plugin_service = PluginService::new(plugin_registry_client);

    let profile_manager = ProfileManager::load().await?;

    let profile_registry_client = RegistryClient::connect(REGISTRY_ENDPOINT).await?;
    let profile_service = ProfileService::new(profile_manager.clone(), profile_registry_client);

    let steam_service = SteamService::new(profile_manager.clone());

    let accounts_registry_client = RegistryClient::connect(REGISTRY_ENDPOINT).await?;
    let accounts_service = AccountsService::new(profile_manager, accounts_registry_client);

    tracing::info!("Server started on http://{LISTEN_ADDR}");

    tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(reflection_service)
        .add_service(ProfileServer::new(profile_service))
        .add_service(LibraryServer::new(library_service))
        .add_service(PluginServer::new(plugin_service))
        .add_service(SteamServer::new(steam_service))
        .add_service(AccountsServer::new(accounts_service))
        .add_service(BottleServer::new(bottle_service))
        .serve(LISTEN_ADDR.parse()?)
        .await?;

    Ok(())
}
