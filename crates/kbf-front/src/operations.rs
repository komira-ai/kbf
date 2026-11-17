//! `Operations`: GetOperation, over the same [`Dispatch`] as `Execution`.
//!
//! GetOperation answers the operation a name, as Execute returned it, names: its stage now,
//! as one message, where WaitExecution streams the stage as it changes. A name the
//! [`Dispatch`] does not know is NOT_FOUND, whoever asks. The call is authorized by the
//! [`Authorizers::execute`] authorizer against the instance name the operation was submitted
//! under, as WaitExecution is, once the operation is found.
//!
//! CancelOperation asks the [`Dispatch`] to stop the job of an unfinished operation
//! ([`Dispatch::cancel`]); an unknown name is NOT_FOUND, the call is authorized as
//! GetOperation is, and cancelling an operation that has finished is a no-op that succeeds.
//!
//! The other calls of the service (ListOperations, DeleteOperation and WaitOperation) are not
//! served: they are UNIMPLEMENTED. WaitExecution is the REAPI's wait.

use std::sync::Arc;

use kbf_auth::{Authorizers, authorize};
use kbf_proto::google::longrunning::operations_server::Operations;
use kbf_proto::google::longrunning::{
    CancelOperationRequest, DeleteOperationRequest, GetOperationRequest, ListOperationsRequest,
    ListOperationsResponse, Operation, WaitOperationRequest,
};
use tonic::{Request as GrpcRequest, Response, Status};

use crate::execution::{Dispatch, on_dispatch, operation};

/// The gRPC path of GetOperation, as calls are authorized.
const GET_OPERATION: &str = "/google.longrunning.Operations/GetOperation";

/// The gRPC path of CancelOperation, as calls are authorized.
const CANCEL_OPERATION: &str = "/google.longrunning.Operations/CancelOperation";

/// The `Operations` service over a [`Dispatch`].
#[derive(Debug)]
pub struct OperationsService<D> {
    dispatch: Arc<D>,
    authorizers: Arc<Authorizers>,
}

impl<D> OperationsService<D> {
    /// The service over `dispatch`, each call authorized by `authorizers`.
    #[must_use]
    pub fn new(dispatch: Arc<D>, authorizers: Arc<Authorizers>) -> Self {
        Self {
            dispatch,
            authorizers,
        }
    }
}

/// The status of a call the service does not serve.
fn not_served(call: &str) -> Status {
    Status::unimplemented(format!(
        "{call} is not served: WaitExecution waits on an operation, GetOperation reads one"
    ))
}

#[tonic::async_trait]
impl<D> Operations for OperationsService<D>
where
    D: Dispatch,
{
    async fn list_operations(
        &self,
        _request: GrpcRequest<ListOperationsRequest>,
    ) -> Result<Response<ListOperationsResponse>, Status> {
        Err(not_served("ListOperations"))
    }

    async fn get_operation(
        &self,
        request: GrpcRequest<GetOperationRequest>,
    ) -> Result<Response<Operation>, Status> {
        let caller = kbf_auth::metadata(&request);
        let name = request.into_inner().name;
        let asked = name.clone();
        let Some(ticket) = on_dispatch(&self.dispatch, move |d| d.wait(&asked)).await? else {
            return Err(Status::not_found(format!(
                "no operation {name:?}: finished operations are kept only briefly; \
                 Execute answers a finished one from the action cache"
            )));
        };
        let execute = &*self.authorizers.execute;
        authorize(execute, &caller, GET_OPERATION, &ticket.instance).await?;
        let stage = ticket.stage.borrow().clone();
        Ok(Response::new(operation(
            &ticket.name,
            &ticket.action,
            &stage,
            false,
        )))
    }

    async fn delete_operation(
        &self,
        _request: GrpcRequest<DeleteOperationRequest>,
    ) -> Result<Response<()>, Status> {
        Err(not_served("DeleteOperation"))
    }

    async fn cancel_operation(
        &self,
        request: GrpcRequest<CancelOperationRequest>,
    ) -> Result<Response<()>, Status> {
        let caller = kbf_auth::metadata(&request);
        let name = request.into_inner().name;
        let asked = name.clone();
        let Some(ticket) = on_dispatch(&self.dispatch, move |d| d.wait(&asked)).await? else {
            return Err(Status::not_found(format!("no operation {name:?}")));
        };
        let execute = &*self.authorizers.execute;
        authorize(execute, &caller, CANCEL_OPERATION, &ticket.instance).await?;
        // False: the operation finished first, and cancelling a finished one is a no-op.
        let _asked = on_dispatch(&self.dispatch, move |d| d.cancel(&name)).await?;
        Ok(Response::new(()))
    }

    async fn wait_operation(
        &self,
        _request: GrpcRequest<WaitOperationRequest>,
    ) -> Result<Response<Operation>, Status> {
        Err(not_served("WaitOperation"))
    }
}
