//! What an old child may still write after a takeover (`docs/design/frontend-proxy.md`,
//! section 8, "Who may write which keys"): the history rows of the calls it still serves.
//! Two children over one shared in-memory state store, each behind a recording wrapper.

use kbf_front::REQUEST_METADATA_HEADER;
use kbf_history::{HistoryIndex, HistoryRoot, IndexRoot, Inline, InvKey, Reader};
use kbf_objstore::{Floor, FsStore, KeyPrefix};
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{ExecuteRequest, RequestMetadata, ToolDetails};
use kbf_state::recording::RecordingStore;
use prost::Message as _;
use tonic::metadata::MetadataValue;

use super::*;

/// The build the run belongs to.
const BUILD: &str = "5d0c6f2e-7a41-4f0b-8f0e-9a3d6b1c2e77";

/// An Execute of `job`, naming invocation `BUILD` the way buck2 does.
async fn execute_as_build(
    client: &Client,
    job: &Job,
) -> tonic::Streaming<kbf_proto::google::longrunning::Operation> {
    let metadata = RequestMetadata {
        tool_details: Some(ToolDetails {
            tool_name: "buck2".to_owned(),
            tool_version: "0.1.0".to_owned(),
        }),
        tool_invocation_id: BUILD.to_owned(),
        ..Default::default()
    };
    let mut request = tonic::Request::new(ExecuteRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(job.action.proto.clone()),
        ..Default::default()
    });
    request.metadata_mut().insert_bin(
        REQUEST_METADATA_HEADER,
        MetadataValue::from_bytes(&metadata.encode_to_vec()),
    );
    ExecutionClient::new(client.channel())
        .execute(request)
        .await
        .expect("Execute")
        .into_inner()
}

/// A child over `store` that stops when `stop` is sent.
async fn child_until(
    flags: &FrontendFlags,
    store: Arc<dyn Store>,
    stop: oneshot::Receiver<()>,
) -> (Child, JoinHandle<()>) {
    let managed = Managed::start(flags, StoreSource::Shared(store), async move {
        let _ = stop.await;
    })
    .await
    .expect("starts");
    let view = managed.view();
    let child = Child {
        admin: managed.admin(),
        reapi: view.reapi.expect("a REAPI listener"),
        api: view.api.expect("an operator API"),
        id: view.instance_id,
    };
    let serving = tokio::spawn(async move {
        let _ = managed.serve().await;
    });
    (child, serving)
}

/// Catches: an old child whose history is lost at a swap. A holds a run it recorded for
/// build `BUILD` (the writer cuts its chunk at a stop, or after two minutes). B takes the
/// generation, then A stops and its writer flushes: the chunk's object is put and its
/// index rows follow. Every write through A's handle is refused once B holds the
/// generation, and the history rows ride the same handle, so none lands and B has no
/// history of the build: `GET /v1/invocations/{id}` is 404 and no `ichunk` row exists.
/// The design's rule is that the old child may still write the rows carrying its own
/// writer id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_child_flushes_its_history_after_a_takeover_and_the_new_child_reads_it() {
    let node = Node::start().await;
    let base = shared();
    let scratch = Scratch::new("old-child-history");
    let token_flag = scratch.token_flag();
    // Two children cannot open one `file://` directory (it is locked); each has its own
    // and the objects of A are read back from A's after it stops.
    let dir_a = scratch.0.join("history-a");
    let history_a = format!("--history-objects=file://{}", dir_a.display());
    let history_b = format!(
        "--history-objects=file://{}",
        scratch.0.join("history-b").display()
    );
    let a_store = Arc::new(RecordingStore::new(Arc::clone(&base) as Arc<dyn Store>));
    let b_store = Arc::new(RecordingStore::new(Arc::clone(&base) as Arc<dyn Store>));
    let (stop_a, stopped_a) = oneshot::channel();
    let (a, a_serving) = child_until(
        &managed_flags(&node, &[&token_flag, &history_a]),
        a_store.clone(),
        stopped_a,
    )
    .await;
    let (_stop_b, stopped_b) = oneshot::channel();
    let (b, _b_serving) = child_until(
        &managed_flags(&node, &["--await-takeover", &token_flag, &history_b]),
        b_store.clone(),
        stopped_b,
    )
    .await;

    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a run recorded across a takeover", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = execute_as_build(&client, &job).await;
    let lease = daemon.start().await.lease_id;
    let result = support::output(&client, "built", 0).await;
    assert!(daemon.report(ran(lease, &result)).await.accepted);
    done(&mut ops).await;

    // A's writer has taken its first step: it holds a stream block, so its flush at the
    // stop is spent on rows and not on a refill of the counter.
    until("A reserved a stream block", async || {
        (a_store.writes().iter()).any(|w| w.key == "ui/streams" && w.committed)
    })
    .await;
    let (held, _) = generation(&base);
    assert_eq!(a.post("quiesce").await.0, 200);
    let (code, took) = b.takeover(held).await;
    assert_eq!(code, 200, "{took}");
    a_store.clear();
    stop_a.send(()).expect("A is still serving");
    tokio::time::timeout(Duration::from_secs(30), a_serving)
        .await
        .expect("A stops")
        .expect("A's task");

    let rows: Vec<String> = base
        .scan_prefix("ui/ichunk/")
        .expect("scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert!(
        !rows.is_empty(),
        "A's writer flushed after the takeover and no chunk row landed; A tried {:?}",
        a_store.writes()
    );
    // What a reader of the next generation finds: the index rows in the shared store and
    // the objects A put.
    let objects = FsStore::open(&dir_a, Floor::KeepFree).expect("A's objects");
    let root = HistoryRoot::under(&KeyPrefix::new("kbf/").expect("prefix")).expect("root");
    let index = HistoryIndex::new(
        Arc::clone(&base) as Arc<dyn Store>,
        IndexRoot::default(),
        Arc::new(Inline),
    );
    let reader = Reader::new(objects, root, index);
    let view = reader
        .invocation(&InvKey::from_record_id(BUILD), &|_| true)
        .await
        .expect("a read")
        .expect("A's invocation is in the index");
    assert_eq!(view.header.runs, 1, "{:?}", view.header);
}
