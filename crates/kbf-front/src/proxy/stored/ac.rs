//! `ActionCache` over [`Stored`]: a lookup is the row of the state store, checked against the
//! pack index; no client writes.

use std::sync::Arc;

use kbf_auth::{Authorizers, authorize};
use kbf_proto::reapi::action_cache_server::ActionCache;
use kbf_proto::reapi::{ActionResult, GetActionResultRequest, UpdateActionResultRequest};
use tonic::{Request, Response, Status};

use super::Stored;
use crate::wire;

/// The `ActionCache` service of a frontend with no storage node. GetActionResult is authorized
/// by [`Authorizers::ac_get`] against the request's instance name.
pub(super) struct StoredActionCache {
    pub stored: Arc<Stored>,
    pub authorizers: Arc<Authorizers>,
}

#[tonic::async_trait]
impl ActionCache for StoredActionCache {
    /// The row of the action, if every blob its result names is held; else NOT_FOUND.
    async fn get_action_result(
        &self,
        request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let call = "/build.bazel.remote.execution.v2.ActionCache/GetActionResult";
        let authorizer = &*self.authorizers.ac_get;
        authorize(authorizer, &caller, call, &request.instance_name).await?;
        wire::check_digest_function(request.digest_function)?;
        let action = wire::digest(request.action_digest.as_ref())?;
        match self.stored.action_result(&action).await? {
            Some((_, result)) => Ok(Response::new(result)),
            None => Err(Status::not_found(format!(
                "no usable action-cache entry for {action}"
            ))),
        }
    }

    /// Always PERMISSION_DENIED: a client never writes the action cache.
    async fn update_action_result(
        &self,
        _request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        Err(Status::permission_denied(
            "clients cannot write the action cache; only the job that ran an action does",
        ))
    }
}
