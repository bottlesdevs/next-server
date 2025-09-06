use bottles_core::proto::bottles_server::BottlesServer;
use bottles_server::BottlesService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
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
