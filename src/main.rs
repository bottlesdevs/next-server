use bottles_core::proto::bottles_server::BottlesServer;
use bottles_server::BottlesService;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("bottles_server=trace")),
        )
        .init();

    let addr = "[::1]:50052".parse().unwrap();
    let service = BottlesService::default();
    tracing::info!("Listening on {}", addr);
    tonic::transport::Server::builder()
        .add_service(BottlesServer::new(service))
        .serve(addr)
        .await?;
    Ok(())
}
