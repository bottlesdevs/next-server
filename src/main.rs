use bottles_core::{
    credentials::os::OsCredentialStore,
    plugins::{StorePlugin, egs::EpicGamesService},
    registry::StoreRegistry,
};
use bottles_server::{library::LibraryService, profile::ProfileService, store::StoreService};
use next_proto::bottles::{
    library::v1::library_server::LibraryServer, profiles::v1::profile_server::ProfileServer,
    store::v1::store_server::StoreServer,
};
use std::sync::Arc;
use tonic_health::server::health_reporter;
use tonic_reflection::server::Builder;
use tracing_subscriber::EnvFilter;

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

    let reflection_service = Builder::configure()
        .register_encoded_file_descriptor_set(next_proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;

    let credentials = Arc::new(OsCredentialStore::new());

    let stores = StoreRegistry::new([
        Arc::new(EpicGamesService::new(credentials.clone())) as Arc<dyn StorePlugin>
    ]);

    tracing::info!("Server started on http://127.0.0.1:50052");

    let _ = tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(reflection_service)
        .add_service(ProfileServer::new(ProfileService::new()))
        .add_service(StoreServer::new(StoreService::new(Arc::new(
            stores.clone(),
        ))))
        .add_service(LibraryServer::new(LibraryService::new(Arc::new(stores))))
        .serve("0.0.0.0:50052".parse()?)
        .await?;

    Ok(())
}
