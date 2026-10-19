//! What an old child may still write after a takeover (`docs/design/frontend-proxy.md`,
//! section 8, "Who may write which keys"): the history rows of the calls it still serves.
//! Two children over one shared in-memory state store, each behind a recording wrapper.

use kbf_front::REQUEST_METADATA_HEADER;
use kbf_history::{
    HistoryIndex, HistoryRoot, IndexRoot, Inline, InvKey, ListQuery, Ownership, Reader, WriterId,
};
use kbf_objstore::{Floor, FsStore, KeyPrefix};
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{ExecuteRequest, RequestMetadata, ToolDetails};
use kbf_server::write_rules::{Rule, classify};
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

/// Two children over one shared store, each behind a [`RecordingStore`] and with a history
/// directory of its own (a `file://` directory is locked by the process that opens it).
struct Swap {
    base: Arc<Fake>,
    scratch: Scratch,
    /// The storage node, kept up for the test.
    _node: Node,
    a: Child,
    a_store: Arc<RecordingStore>,
    stop_a: Option<oneshot::Sender<()>>,
    a_serving: Option<JoinHandle<()>>,
    b: Child,
    b_store: Arc<RecordingStore>,
    stop_b: Option<oneshot::Sender<()>>,
    b_serving: Option<JoinHandle<()>>,
}

impl Swap {
    async fn start(name: &str) -> Swap {
        let node = Node::start().await;
        let base = shared();
        let scratch = Scratch::new(name);
        let token = scratch.token_flag();
        let history =
            |dir: &str| format!("--history-objects=file://{}", scratch.0.join(dir).display());
        let a_store = Arc::new(RecordingStore::new(Arc::clone(&base) as Arc<dyn Store>));
        let b_store = Arc::new(RecordingStore::new(Arc::clone(&base) as Arc<dyn Store>));
        let (stop_a, stopped_a) = oneshot::channel();
        let (a, a_serving) = child_until(
            &managed_flags(&node, &[&token, &history("history-a")]),
            a_store.clone(),
            stopped_a,
        )
        .await;
        let (stop_b, stopped_b) = oneshot::channel();
        let (b, b_serving) = child_until(
            &managed_flags(&node, &["--await-takeover", &token, &history("history-b")]),
            b_store.clone(),
            stopped_b,
        )
        .await;
        Swap {
            base,
            scratch,
            _node: node,
            a,
            a_store,
            stop_a: Some(stop_a),
            a_serving: Some(a_serving),
            b,
            b_store,
            stop_b: Some(stop_b),
            b_serving: Some(b_serving),
        }
    }

    /// A run of build `BUILD`, executed through `child` and answered by a daemon that
    /// connects to it.
    async fn run_on(&self, child: &Child, text: &str) {
        let worker = Child::worker_of(&child.role().await).expect("a worker listener");
        let client = Client::connect(child.reapi, worker).await;
        let mut daemon = client.daemon(&format!("node-{text}"), 4, 8).await;
        let job = Job::new(text, &[]);
        client.upload(&job.blobs()).await;
        let mut ops = execute_as_build(&client, &job).await;
        let lease = daemon.start().await.lease_id;
        let result = support::output(&client, "built", 0).await;
        assert!(daemon.report(ran(lease, &result)).await.accepted);
        done(&mut ops).await;
    }

    /// Waits until a child's history writer has reserved its first stream block, so its
    /// flush at a stop is spent on rows and not on a refill of the counter (a step sends
    /// one transaction).
    async fn holds_a_block(store: &RecordingStore) {
        until("a stream block is reserved", async || {
            (store.writes().iter()).any(|w| w.key == "ui/streams" && w.committed)
        })
        .await;
    }

    /// Quiesces A and has B take the generation; forgets what A wrote so far.
    async fn take_over(&self) {
        let (held, _) = generation(&self.base);
        assert_eq!(self.a.post("quiesce").await.0, 200);
        let (code, took) = self.b.takeover(held).await;
        assert_eq!(code, 200, "{took}");
        self.a_store.clear();
    }

    /// Stops a child and waits for its writer's last step.
    async fn stop(stop: &mut Option<oneshot::Sender<()>>, serving: &mut Option<JoinHandle<()>>) {
        stop.take().expect("it runs").send(()).expect("it serves");
        tokio::time::timeout(Duration::from_secs(30), serving.take().expect("it runs"))
            .await
            .expect("it stops")
            .expect("its task");
    }

    async fn stop_a(&mut self) {
        Self::stop(&mut self.stop_a, &mut self.a_serving).await;
    }

    async fn stop_b(&mut self) {
        Self::stop(&mut self.stop_b, &mut self.b_serving).await;
    }

    /// A reader of history as the next generation has it: the index rows in the shared
    /// store, and the objects of both children (B's copied beside A's; both have stopped,
    /// so their directories are free to open).
    fn reader(&self) -> Reader<FsStore> {
        let dir = |name: &str| self.scratch.0.join(name);
        copy_tree(
            &dir("history-b").join("objects"),
            &dir("history-a").join("objects"),
        );
        let objects = FsStore::open(&dir("history-a"), Floor::KeepFree).expect("the objects");
        let root = HistoryRoot::under(&KeyPrefix::new("kbf/").expect("prefix")).expect("root");
        let index = HistoryIndex::new(
            Arc::clone(&self.base) as Arc<dyn Store>,
            IndexRoot::default(),
            Arc::new(Inline),
        );
        Reader::new(objects, root, index)
    }
}

/// Copies the files of `from` under `to`, as they are laid out.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    std::fs::create_dir_all(to).expect("a directory");
    for entry in entries {
        let entry = entry.expect("an entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("a copy");
        }
    }
}

/// The design's proof of its write rule (section 8): the old child, after the takeover,
/// writes only keys that carry its own id or are inserts that name their content, and no
/// key of a fenced family. A runs a build and holds its record; B takes the generation;
/// A stops and its writer flushes. Everything A's store saw after the takeover is judged by
/// the table of `kbf_server::write_rules`: each committed write is of A's own writer id
/// (`WriterId::from_instance` of the id `role` reports) or content-keyed, and the shared
/// rows A also tries (the shape row) are refused by the fence and are not committed. Then
/// every key in the store, the scheduler's included, is named by the table, and the next
/// generation reads A's build: its header row, chunk row and objects.
///
/// Catches: the old child's history lost at a swap (every write through its handle was
/// refused, the history rows included); a history key without the writer's id (the rows
/// would carry a clock, not the process); a shared row written around the fence (two
/// children racing on one row); a key no family names. Mutants: the permit accepting the
/// shape row; the writer named by its start time and not its instance id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_old_child_writes_only_keys_that_carry_its_id_after_a_takeover() {
    let mut swap = Swap::start("old-child-writes").await;
    swap.run_on(&swap.a, "a build recorded across a takeover")
        .await;
    Swap::holds_a_block(&swap.a_store).await;
    swap.take_over().await;
    swap.stop_a().await;

    let root = IndexRoot::default();
    let a_writer = WriterId::from_instance(&swap.a.id);
    let writes = swap.a_store.writes();
    let (mut own, mut inserts, mut refused_shared) = (0, 0, 0);
    for write in &writes {
        let kind = classify(&write.key, &root)
            .unwrap_or_else(|| panic!("{} is of no family of the table", write.key));
        match (kind.rule, write.committed) {
            (Rule::OwnInstance, true) => {
                let ownership = root.kind(&write.key).map(kbf_history::RowKind::ownership);
                assert_eq!(
                    ownership,
                    Some(Ownership::Writer(a_writer)),
                    "{}",
                    write.key
                );
                own += 1;
            }
            (Rule::ContentKeyed, true) => inserts += 1,
            // A write the fence refused, with the rest of its transaction.
            (Rule::Fenced, false) => refused_shared += 1,
            (_, false) => {}
            (rule, true) => panic!(
                "A committed {} (a {rule:?} family) after the takeover",
                write.key
            ),
        }
    }
    assert!(own >= 2, "A's chunk and header rows landed: {writes:?}");
    assert!(
        inserts >= 2,
        "its stream row and list entry landed: {writes:?}"
    );
    assert!(
        refused_shared >= 1,
        "the shared shape row was tried and fenced: {writes:?}"
    );

    for (key, _) in swap.base.scan_prefix("").expect("scan") {
        assert!(classify(&key, &root).is_some(), "{key} is of no family");
    }
    let view = swap
        .reader()
        .invocation(&InvKey::from_record_id(BUILD), &|_| true)
        .await
        .expect("a read")
        .expect("A's invocation is in the index");
    assert_eq!(view.header.runs, 1, "{:?}", view.header);
    swap.stop_b().await;
}

/// Readers see every writer's rows: the build continues on B after A has flushed its half,
/// and the two children's header rows (one each, under their own writer ids) read back as
/// one invocation: two runs, two chunks, listed once, though each child put a list entry
/// at the time of its own first record.
///
/// Catches: B's header writing into A's row or A's into B's (the counts would be one
/// child's, not the sum); a reader that takes the first writer's row; a list that shows the
/// build twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_generation_reads_the_history_of_both_children_as_one() {
    let mut swap = Swap::start("both-children").await;
    swap.run_on(&swap.a, "the first half of a build").await;
    Swap::holds_a_block(&swap.a_store).await;
    swap.take_over().await;
    swap.stop_a().await;
    swap.run_on(&swap.b, "the second half of a build").await;
    Swap::holds_a_block(&swap.b_store).await;
    swap.stop_b().await;

    let inv = InvKey::from_record_id(BUILD);
    let headers = swap
        .base
        .scan_prefix(&IndexRoot::default().header_rows(&inv))
        .expect("scan");
    let mut writers: Vec<String> = headers.into_iter().map(|(key, _)| key).collect();
    writers.sort();
    let mut want: Vec<String> = [&swap.a.id, &swap.b.id]
        .iter()
        .map(|id| IndexRoot::default().header(&inv, WriterId::from_instance(id)))
        .collect();
    want.sort();
    assert_eq!(writers, want, "one header row for each child");

    let reader = swap.reader();
    let view = reader
        .invocation(&inv, &|_| true)
        .await
        .expect("a read")
        .expect("the invocation is in the index");
    assert_eq!(
        (view.header.runs, view.header.chunks),
        (2, 2),
        "{:?}",
        view.header
    );
    let listed = reader
        .list_invocations(&ListQuery::new("main", 10), &|_| true)
        .await
        .expect("a list");
    assert_eq!(listed.items.len(), 1, "the build is listed once");
}
