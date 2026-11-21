//! The open Execute and WaitExecution streams by build, as the mounted REAPI handlers count
//! them (`ExecuteMetrics::open_streams`): each stream under the `tool_invocation_id` of its
//! call's `RequestMetadata` header, read by the same layer kbf-server puts in front of its REAPI
//! listener. A forced stop of a drain reports these counts.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use common::{Blob, Farm};
use kbf_front::metrics::StreamCensus;
use kbf_front::{Dispatch, Finished, RequestMetadataLayer, Stage, Submission, Ticket};
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::{
    Action, Command, Directory, ExecuteRequest, RequestMetadata, WaitExecutionRequest,
};
use prost::Message as _;
use tokio::sync::watch;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::{Status, Streaming};

/// A hang guard only: nothing here is timed.
const HANG: Duration = Duration::from_secs(10);

/// An operation of [`Running`]: its instance, its action and its stage.
type Op = (String, kbf_types::Digest, watch::Sender<Stage>);

/// A dispatcher whose operations run until the test ends them: each submit is a new operation,
/// executing, named `operations/<n>`, which WaitExecution finds by that name.
#[derive(Default)]
struct Running {
    ops: Mutex<BTreeMap<String, Op>>,
}

impl Running {
    /// Ends the operation `name`: its streams get the done operation and end.
    fn finish(&self, name: &str) {
        let ops = self.ops.lock().unwrap_or_else(PoisonError::into_inner);
        let (_, _, stage) = ops.get(name).expect("an operation of that name");
        stage.send_replace(Stage::Done(Finished::Ran(Box::default())));
    }

    fn ticket(&self, name: &str) -> Option<Ticket> {
        let ops = self.ops.lock().unwrap_or_else(PoisonError::into_inner);
        ops.get(name).map(|(instance, action, stage)| Ticket {
            name: name.to_owned(),
            instance: instance.clone(),
            action: *action,
            stage: stage.subscribe(),
        })
    }
}

impl Dispatch for Running {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let name = {
            let mut ops = self.ops.lock().unwrap_or_else(PoisonError::into_inner);
            let name = format!("operations/{}", ops.len());
            let (stage, _) = watch::channel(Stage::Executing);
            let key = submission.key;
            ops.insert(name.clone(), (key.instance, key.action, stage));
            name
        };
        self.ticket(&name)
            .ok_or_else(|| Status::internal("just made"))
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        self.ticket(name)
    }

    fn cancel(&self, _name: &str) -> bool {
        false
    }
}

/// The `RequestMetadata` header of a call of the build `id`.
fn of_build(id: &str) -> MetadataMap {
    let header = RequestMetadata {
        tool_invocation_id: id.to_owned(),
        ..Default::default()
    };
    let mut metadata = MetadataMap::new();
    let value = MetadataValue::from_bytes(&header.encode_to_vec());
    metadata.insert_bin(kbf_front::REQUEST_METADATA_HEADER, value);
    metadata
}

/// An action running `argv` on an empty input tree, its blobs uploaded to `farm`.
async fn action(farm: &Farm, argv: &str) -> Blob {
    let root = Blob::of(&Directory::default());
    let command = Blob::of(&Command {
        arguments: vec![argv.to_owned()],
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(root.proto.clone()),
        ..Default::default()
    });
    farm.upload(&[&root, &command, &action]).await;
    action
}

/// An Execute of `action` sent with `metadata`, read up to its first operation.
async fn execute(
    farm: &Farm,
    action: &Blob,
    metadata: MetadataMap,
) -> (Streaming<Operation>, String) {
    let mut request = tonic::Request::new(ExecuteRequest {
        action_digest: Some(action.proto.clone()),
        skip_cache_lookup: true,
        ..Default::default()
    });
    *request.metadata_mut() = metadata;
    let mut ops = farm
        .exec()
        .execute(request)
        .await
        .expect("Execute")
        .into_inner();
    let first = next(&mut ops).await.expect("a first operation");
    (ops, first.name)
}

/// A WaitExecution of `name` sent with `metadata`, read up to its first operation.
async fn wait(farm: &Farm, name: &str, metadata: MetadataMap) -> Streaming<Operation> {
    let mut request = tonic::Request::new(WaitExecutionRequest {
        name: name.to_owned(),
    });
    *request.metadata_mut() = metadata;
    let mut ops = farm
        .exec()
        .wait_execution(request)
        .await
        .expect("WaitExecution")
        .into_inner();
    next(&mut ops).await.expect("a first operation");
    ops
}

/// The next operation on `ops`, or `None` once the stream has ended.
async fn next(ops: &mut Streaming<Operation>) -> Option<Operation> {
    tokio::time::timeout(HANG, ops.message())
        .await
        .expect("an answer in time")
        .expect("a healthy stream")
}

/// Reads `ops` to its end, which comes after the done operation.
async fn ends(ops: &mut Streaming<Operation>) {
    while next(ops).await.is_some() {}
}

/// The census once it reads `want`: a stream's guard is dropped by the server's task as the
/// stream ends, just after the client has read the end.
async fn census_reaches(farm: &Farm, want: StreamCensus) {
    let metrics = Arc::clone(&farm.cache.metrics().execute);
    let reached = async {
        while metrics.open_streams() != want {
            tokio::task::yield_now().await;
        }
    };
    tokio::time::timeout(HANG, reached)
        .await
        .unwrap_or_else(|_| panic!("census {:?}, not {want:?}", metrics.open_streams()));
}

/// Catches an Execute or a WaitExecution whose stream is not counted under the build its
/// call's header names (the header not read for that call, or read and not given to its
/// stream's guard), a call with no header counted as a build, and a stream that stays counted
/// once it has ended: two Executes and a WaitExecution of build `build-a`, an Execute of
/// `build-b` and one with no header are three builds' worth of streams, two named builds and
/// one unnamed stream; as the operations end, their streams leave the census.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_stream_is_counted_under_the_build_its_call_names_until_it_ends() {
    let dispatch = Arc::new(Running::default());
    let farm = Farm::with_execution_and_request_metadata(
        Arc::clone(&dispatch),
        RequestMetadataLayer::new(),
    )
    .await;
    let first = action(&farm, "first").await;
    let second = action(&farm, "second").await;
    let third = action(&farm, "third").await;
    let fourth = action(&farm, "fourth").await;

    let (mut a1, a1_name) = execute(&farm, &first, of_build("build-a")).await;
    let (mut a2, a2_name) = execute(&farm, &second, of_build("build-a")).await;
    let mut a_wait = wait(&farm, &a1_name, of_build("build-a")).await;
    let (mut b, b_name) = execute(&farm, &third, of_build("build-b")).await;
    let (mut none, none_name) = execute(&farm, &fourth, MetadataMap::new()).await;
    census_reaches(
        &farm,
        StreamCensus {
            streams: 5,
            builds: 2,
            unnamed: 1,
        },
    )
    .await;

    dispatch.finish(&b_name);
    ends(&mut b).await;
    dispatch.finish(&none_name);
    ends(&mut none).await;
    census_reaches(
        &farm,
        StreamCensus {
            streams: 3,
            builds: 1,
            unnamed: 0,
        },
    )
    .await;

    dispatch.finish(&a1_name);
    ends(&mut a1).await;
    ends(&mut a_wait).await;
    census_reaches(
        &farm,
        StreamCensus {
            streams: 1,
            builds: 1,
            unnamed: 0,
        },
    )
    .await;
    dispatch.finish(&a2_name);
    ends(&mut a2).await;
    census_reaches(&farm, StreamCensus::default()).await;
}
