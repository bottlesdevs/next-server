use bottles_core::proto::{
    self,
    bottles_server::{Bottles, BottlesServer},
};

#[derive(Debug, Default)]
struct BottlesService;

#[tonic::async_trait]
impl Bottles for BottlesService {
    async fn health(
        &self,
        request: tonic::Request<proto::BottlesRequest>,
    ) -> Result<tonic::Response<proto::BottlesResponse>, tonic::Status> {
        let request = request.get_ref();
        println!("Received request: {:?}", request);
        Ok(tonic::Response::new(proto::BottlesResponse { ok: true }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50052".parse().unwrap();
    let service = BottlesService::default();
    println!("Listening on {}", addr);
    tonic::transport::Server::builder()
        .add_service(BottlesServer::new(service))
        .serve(addr)
        .await?;
    Ok(())
}
