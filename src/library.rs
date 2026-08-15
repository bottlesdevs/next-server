use futures_core::Stream;
use next_proto::bottles::common::v1::Storefront;
use next_proto::bottles::library::v1::{GameEvent, WatchGamesRequest};
use next_proto::bottles::library::v1::{
    ListGamesRequest, ListGamesResponse, library_server::Library,
};
use std::pin::Pin;
use std::sync::Arc;
use tonic::{Request, Response, Status, async_trait};

use bottles_core::registry::StoreRegistry;

pub struct LibraryService {
    stores: Arc<StoreRegistry>,
}

impl LibraryService {
    pub fn new(stores: Arc<StoreRegistry>) -> Self {
        Self { stores }
    }
}

#[async_trait]
impl Library for LibraryService {
    async fn list_games(
        &self,
        request: Request<ListGamesRequest>,
    ) -> std::result::Result<Response<ListGamesResponse>, Status> {
        let ListGamesRequest {
            profile_id,
            storefronts,
        } = request.into_inner();

        let storefronts = storefronts
            .into_iter()
            .filter_map(|s| Storefront::try_from(s).ok())
            .collect::<Vec<_>>();

        let mut games = Vec::new();
        for storefront in storefronts {
            if let Some(plugin) = self.stores.get(storefront) {
                if let Ok(mut g) = plugin.games(&profile_id).await {
                    games.append(&mut g);
                }
            }
        }

        Ok(Response::new(ListGamesResponse { games }))
    }

    /// Server streaming response type for the WatchGames method.
    type WatchGamesStream = Pin<Box<dyn Stream<Item = Result<GameEvent, Status>> + Send + 'static>>;

    async fn watch_games(
        &self,
        request: Request<WatchGamesRequest>,
    ) -> std::result::Result<Response<Self::WatchGamesStream>, Status> {
        let _request = request.into_inner();
        let stream = tokio_stream::iter(vec![Ok(GameEvent { event: None })]);
        Ok(Response::new(Box::pin(stream)))
    }
}
