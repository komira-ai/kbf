//! GetOperation end to end, against a server that keeps its operations in a state store: a REAPI
//! client, the server and its executor. The name Execute returns is the operation's id in the
//! store, and GetOperation answers it with the stage the operation is at, executing and then
//! done.
//!
//! A module of the `execute` test binary, not a binary of its own.

use kbf_proto::google::longrunning::operations_client::OperationsClient;
use kbf_proto::google::longrunning::{GetOperationRequest, Operation};
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use tonic::Code;

use crate::support::{Cell, Client, FakeState, Job, Ran, done, response, stage};

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

/// Asserts that GetOperation answers the name Execute returned by a server over a state store:
/// the name is `operations/<id>`, the operation shows executing, under the name it was asked
/// for, once its job runs, and done with the result after the job ends, still there after the
/// Execute stream is closed; and that a name nobody gave is NOT_FOUND.
#[tokio::test]
async fn get_operation_follows_an_operation_of_a_server_over_a_store() {
    let state = FakeState::new();
    let cell = Cell::start_with_state(state.handle()).await;
    let job = Job::new("asked by name", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "asked by name"));
    let mut ops = cell.execute(&job.action).await;
    let first = ops.message().await.expect("a stream").expect("an update");
    let name = first.name;
    assert!(name.starts_with("operations/"), "{name}");
    assert!(!name.contains('-'), "{name} is in the old form");
    let id = cell.direct.run(&job.action).await;

    let running = loop {
        let asked = get(&cell, &name).await.expect("an answer");
        if stage(&asked) == ExecStage::Executing as i32 {
            break asked;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };

    assert_eq!(running.name, name);
    assert!(!running.done);
    assert!(cell.direct.fake.finish(&id, 0));
    let result = response(&done(&mut ops).await).result.expect("a result");
    let finished = get(&cell, &name).await.expect("kept after the finish");
    assert!(finished.done);
    assert_eq!(response(&finished).result, Some(result));
    let nobody = get(&cell, "operations/987654").await;
    assert_eq!(nobody.map(drop), Err(Code::NotFound));
}
