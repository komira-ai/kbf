//! GetOperation end to end, against a server that keeps its state in a store: a REAPI client,
//! the server and a fake daemon. The name Execute returns is the operation's id in the store,
//! and GetOperation answers it with the stage the operation is at, executing and then done.
//!
//! A module of the `execute` test binary, not a binary of its own: `Farm` is generic, and
//! coverage counts each generic function by its best-covered instantiation.

use std::sync::Arc;

use kbf_proto::google::longrunning::operations_client::OperationsClient;
use kbf_proto::google::longrunning::{GetOperationRequest, Operation};
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_server::{EpochPolicy, StateStore};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use tonic::Code;

use crate::support::{Cell, Client, Job, done, output, ran, response, stage};

/// GetOperation on `name`: the operation, or the code.
async fn get(cell: &Client, name: &str) -> Result<Operation, Code> {
    let request = GetOperationRequest {
        name: name.to_owned(),
    };
    let mut client = OperationsClient::new(cell.channel());
    let answer = client.get_operation(request).await;
    answer
        .map(tonic::Response::into_inner)
        .map_err(|status| status.code())
}

/// Asserts that GetOperation answers the name Execute returned by a server over a state
/// store: the name is `operations/<id>`, the operation shows executing, under the name it was
/// asked for, while the daemon runs it, and done with the daemon's result after the report;
/// and that a name nobody gave is NOT_FOUND.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_operation_follows_an_operation_of_a_server_over_a_store() {
    let clock = Arc::new(ManualClock::default());
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let state = StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    };
    let cell = Cell::start_with_state(state).await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let job = Job::new("asked by name", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let first = ops.message().await.expect("a stream").expect("an update");
    let name = first.name;
    assert!(name.starts_with("operations/"), "{name}");
    assert!(!name.contains('-'), "{name} is in the old form");
    let start = daemon.start().await;

    let running = get(&cell, &name).await.expect("an answer");

    assert_eq!(running.name, name);
    assert!(!running.done);
    assert_eq!(stage(&running), ExecStage::Executing as i32);
    let result = output(&cell, "asked by name", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    let finished = get(&cell, &name).await.expect("kept after the finish");
    assert!(finished.done);
    assert_eq!(response(&finished).result, Some(result));
    let nobody = get(&cell, "operations/987654").await;
    assert_eq!(nobody.map(drop), Err(Code::NotFound));
}
