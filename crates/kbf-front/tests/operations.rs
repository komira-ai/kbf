//! `Operations` over gRPC, against a scripted dispatch that stands in for the scheduler:
//! GetOperation reads the stage of an operation by the name Execute returned, and the other
//! calls of the service are not served.

mod common;

use std::sync::Arc;

use common::Farm;
use kbf_front::{Dispatch, Finished, Stage, Submission, Ticket};
use kbf_proto::google::longrunning::operations_client::OperationsClient;
use kbf_proto::google::longrunning::{
    CancelOperationRequest, DeleteOperationRequest, GetOperationRequest, ListOperationsRequest,
    Operation, WaitOperationRequest, operation,
};
use kbf_proto::google::rpc;
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_proto::reapi::{ExecuteOperationMetadata, ExecuteResponse};
use kbf_types::Digest;
use prost::Message;
use tokio::sync::watch;
use tonic::{Code, Status};

/// The one operation the dispatch knows, `operations/7`, under the instance `main`.
struct Known {
    stage: watch::Sender<Stage>,
}

impl Known {
    fn new(stage: Stage) -> Arc<Self> {
        Arc::new(Self {
            stage: watch::channel(stage).0,
        })
    }
}

/// The action `operations/7` runs.
fn action() -> Digest {
    kbf_segments::sha256(b"known")
}

impl Dispatch for Known {
    fn submit(&self, _submission: Submission) -> Result<Ticket, Status> {
        Err(Status::unavailable("the stub runs nothing"))
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        (name == "operations/7").then(|| Ticket {
            name: name.to_owned(),
            instance: "main".to_owned(),
            action: action(),
            stage: self.stage.subscribe(),
        })
    }
}

async fn get(farm: &Farm, name: &str) -> Result<Operation, Status> {
    let request = GetOperationRequest {
        name: name.to_owned(),
    };
    let mut client = OperationsClient::new(farm.channel());
    let answer = client.get_operation(request).await;
    answer.map(tonic::Response::into_inner)
}

fn metadata(op: &Operation) -> ExecuteOperationMetadata {
    let any = op.metadata.as_ref().expect("metadata");
    ExecuteOperationMetadata::decode(any.value.as_slice()).expect("metadata decodes")
}

/// Asserts that GetOperation answers the operation a name names with its stage now, as one
/// message and with the name it was asked for, the action it runs in its metadata, and, once
/// the operation is done, its response; and that it answers again with the newer stage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_operation_answers_the_stage_the_operation_is_at() {
    let known = Known::new(Stage::Executing);
    let farm = Farm::with_execution(Arc::clone(&known)).await;

    let running = get(&farm, "operations/7").await.expect("an answer");

    assert_eq!(running.name, "operations/7");
    assert!(!running.done);
    assert_eq!(metadata(&running).stage, ExecStage::Executing as i32);
    let digest = kbf_front::digest_to_proto(&action());
    assert_eq!(metadata(&running).action_digest, Some(digest));
    let failed = rpc::Status {
        code: Code::Internal as i32,
        message: "the farm could not run it".to_owned(),
        details: Vec::new(),
    };
    let finished = Finished::Failed(failed.clone());
    known.stage.send_replace(Stage::Done(finished));
    let done = get(&farm, "operations/7").await.expect("an answer");
    assert!(done.done);
    assert_eq!(metadata(&done).stage, ExecStage::Completed as i32);
    let Some(operation::Result::Response(any)) = &done.result else {
        panic!("no response in {done:?}");
    };
    let response = ExecuteResponse::decode(any.value.as_slice()).expect("a response");
    assert_eq!(response.status, Some(failed));
}

/// Asserts that GetOperation on a name the dispatch does not know is NOT_FOUND.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_operation_on_an_unknown_name_is_not_found() {
    let farm = Farm::with_execution(Known::new(Stage::Queued)).await;

    let error = get(&farm, "operations/8").await.expect_err("unknown");

    assert_eq!(error.code(), Code::NotFound, "{error:?}");
}

/// Asserts that the other calls of the service are not served: each is UNIMPLEMENTED, and
/// says so naming the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_other_calls_of_the_service_are_not_served() {
    let farm = Farm::with_execution(Known::new(Stage::Queued)).await;
    let mut client = OperationsClient::new(farm.channel());

    let list = ListOperationsRequest::default();
    let list = client.list_operations(list).await.map(drop);
    let delete = DeleteOperationRequest::default();
    let delete = client.delete_operation(delete).await.map(drop);
    let cancel = CancelOperationRequest::default();
    let cancel = client.cancel_operation(cancel).await.map(drop);
    let wait = WaitOperationRequest::default();
    let wait = client.wait_operation(wait).await.map(drop);

    let refused = [
        ("ListOperations", list),
        ("DeleteOperation", delete),
        ("CancelOperation", cancel),
        ("WaitOperation", wait),
    ];
    for (call, answer) in refused {
        let error = answer.expect_err("not served");
        assert_eq!(error.code(), Code::Unimplemented, "{call}: {error:?}");
        assert!(error.message().contains(call), "{call}: {error:?}");
    }
}
