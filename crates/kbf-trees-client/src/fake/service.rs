//! The generated `TreesService` over the fake's [`State`](super::state::State): each call is
//! logged with its request, then answered by the state's rules.

use std::sync::Arc;

use tierstore_trees_api::v1::trees_service_server::TreesService;
use tierstore_trees_api::v1::{
    CreateTreeRequest, CreateTreeResponse, DeleteTreeRequest, DeleteTreeResponse,
    GetPlacementRequest, GetPlacementResponse,
};
use tonic::{Request, Response, Status};

use super::{Call, Shared};

/// The service of one fake.
pub(super) struct Service {
    pub(super) shared: Arc<Shared>,
}

#[tonic::async_trait]
impl TreesService for Service {
    async fn create_tree(
        &self,
        request: Request<CreateTreeRequest>,
    ) -> Result<Response<CreateTreeResponse>, Status> {
        let request = request.into_inner();
        self.shared
            .record(Call::CreateTree(Box::new(request.clone())));
        let answer = self.shared.state().create(request)?;
        Ok(Response::new(answer))
    }

    async fn get_placement(
        &self,
        request: Request<GetPlacementRequest>,
    ) -> Result<Response<GetPlacementResponse>, Status> {
        let request = request.into_inner();
        self.shared.record(Call::GetPlacement(request.clone()));
        let answer = self.shared.state().placement(&request)?;
        Ok(Response::new(answer))
    }

    async fn delete_tree(
        &self,
        request: Request<DeleteTreeRequest>,
    ) -> Result<Response<DeleteTreeResponse>, Status> {
        let request = request.into_inner();
        self.shared.record(Call::DeleteTree(request.clone()));
        self.shared.state().delete(&request.placement)?;
        Ok(Response::new(DeleteTreeResponse {}))
    }
}
