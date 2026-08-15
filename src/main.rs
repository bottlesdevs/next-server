use bottles_server::{library::LibraryService, profile::ProfileService, store::StoreService};
use next_proto::bottles::{
    library::v1::library_server::LibraryServer, profiles::v1::profile_server::ProfileServer,
    registry::v1::registry_client::RegistryClient, store::v1::store_server::StoreServer,
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

    let reflection_service = Builder::configure()
        .register_encoded_file_descriptor_set(next_proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;

    let library_registry_client = RegistryClient::connect(REGISTRY_ENDPOINT).await?;
    let library_service = LibraryService::new(library_registry_client);

    let store_registry_client = RegistryClient::connect(REGISTRY_ENDPOINT).await?;
    let store_service = StoreService::new(store_registry_client);

    let profile_service = ProfileService::new().await?;

    tracing::info!("Server started on http://{LISTEN_ADDR}");

    tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(reflection_service)
        .add_service(ProfileServer::new(profile_service))
        .add_service(LibraryServer::new(library_service))
        .add_service(StoreServer::new(store_service))
        .serve(LISTEN_ADDR.parse()?)
        .await?;

    Ok(())
}
