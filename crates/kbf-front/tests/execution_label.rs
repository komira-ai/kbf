//! The target and action kind an Execute's client sends in its `RequestMetadata` header
//! (`target_id`, `action_mnemonic`), as the mounted Execution handler hands them to the
//! dispatcher with the action (`Submission::described`'s label): the executor sends them to the
//! job service as the job's history key.

mod common;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use common::{Blob, Farm};
use kbf_front::{ActionLabel, Dispatch, RequestMetadataLayer, Stage, Submission, Ticket};
use kbf_proto::reapi::{Action, Command, Directory, ExecuteRequest, RequestMetadata};
use prost::Message as _;
use tokio::sync::watch;
use tonic::Status;
use tonic::metadata::{MetadataMap, MetadataValue};

/// A hang guard only: nothing here is timed.
const HANG: Duration = Duration::from_secs(10);

/// A dispatcher that keeps the label of every action submitted to it and leaves each operation
/// executing.
#[derive(Default)]
struct Labels {
    sent: Mutex<Vec<ActionLabel>>,
    stages: Mutex<Vec<watch::Sender<Stage>>>,
}

impl Labels {
    fn sent(&self) -> Vec<ActionLabel> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Dispatch for Labels {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let label = submission.described.label;
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(label);
        let (stage, watched) = watch::channel(Stage::Executing);
        let mut stages = self.stages.lock().unwrap_or_else(PoisonError::into_inner);
        let name = format!("operations/{}", stages.len());
        stages.push(stage);
        Ok(Ticket {
            name,
            instance: submission.key.instance,
            action: submission.key.action,
            stage: watched,
        })
    }

    fn wait(&self, _name: &str) -> Option<Ticket> {
        None
    }

    fn cancel(&self, _name: &str) -> bool {
        false
    }
}

/// The `RequestMetadata` header of a call that names `target` and `mnemonic` (empty: not sent).
fn header(target: &str, mnemonic: &str) -> MetadataMap {
    let sent = RequestMetadata {
        tool_invocation_id: "build-1".to_owned(),
        target_id: target.to_owned(),
        action_mnemonic: mnemonic.to_owned(),
        ..Default::default()
    };
    let mut metadata = MetadataMap::new();
    let value = MetadataValue::from_bytes(&sent.encode_to_vec());
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

/// Sends an Execute of `action` with `metadata` and reads its first operation, which comes
/// once the action was submitted.
async fn execute(farm: &Farm, action: &Blob, metadata: MetadataMap) {
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
    let first = tokio::time::timeout(HANG, ops.message())
        .await
        .expect("an answer in time")
        .expect("a healthy stream");
    assert!(first.is_some(), "a first operation");
}

/// Asserts that an Execute whose header names a target and an action kind hands both, as sent,
/// to the dispatcher with its action, and that one whose header names neither (as Buck2's
/// does) hands none. Catches the client's target and kind lost between the header and the
/// executor, which would leave every job keyed by its paths alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_target_and_kind_an_execute_names_reach_the_dispatcher() {
    let dispatch = Arc::new(Labels::default());
    let farm = Farm::with_execution_and_request_metadata(
        Arc::clone(&dispatch),
        RequestMetadataLayer::new(),
    )
    .await;
    let named = action(&farm, "named").await;
    let bare = action(&farm, "bare").await;
    execute(&farm, &named, header("//app/Server:server", "CppCompile")).await;
    execute(&farm, &bare, header("", "")).await;
    assert_eq!(
        dispatch.sent(),
        vec![
            ActionLabel {
                mnemonic: Some("CppCompile".to_owned()),
                target: Some("//app/Server:server".to_owned()),
            },
            ActionLabel::default(),
        ]
    );
}
