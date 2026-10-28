//! What an Execute reports of its input walk over a farm routed by a map: the
//! Directories it read and the ones it skipped as verified, the time of each phase, in
//! the `kbf_execute_walk_*` metrics and in the warning an Execute slow before its first
//! operation logs. The two figures that matter in an incident (is the memory firing; is
//! the time in the Directories or in the files) were each fed from the other's source
//! without any test noticing.
//!
//! Three storage nodes in process, each over a cache whose object store GETs take
//! `GET` of virtual time and whose FindMissing takes `FIND`, so that every phase has a
//! time of its own and an exact value. This binary owns the global tracing subscriber.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_auth::Authorizers;
use kbf_front::{
    Backend, Cache, CacheStorage, Channels, Dispatch, ExecutionService, MemoryMetaLog, Routed,
    SegmentsConfig, Stage, Submission, Ticket, VerifiedBlob,
};
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_meta::{ActionRecord, Retention, Role};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::reapi::execution_server::Execution;
use kbf_proto::reapi::{
    Action, ActionResult, Command, Directory, DirectoryNode, ExecuteRequest, FileNode,
};
use kbf_proto::worker::segments_server::Segments;
use kbf_route::{MapSource, MapView, Nodes, RefetchError};
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use prost::Message;
use tokio::sync::watch;
use tonic::Status;

/// How long each store GET takes: a Directory, the action and the Command each cost one.
const GET: Duration = Duration::from_secs(6);

/// How long each FindMissing call to a node takes.
const FIND: Duration = Duration::from_secs(3);

/// A memory bucket whose range GETs take `delay`.
struct Store {
    inner: MemoryStore,
    delay: Duration,
}

impl ObjectStore for Store {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        tokio::time::sleep(self.delay).await;
        self.inner.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

/// A dispatch that queues every submission and never runs it.
#[derive(Default)]
struct Queue {
    submitted: Mutex<Vec<watch::Sender<Stage>>>,
    /// The evidence each submission carried, in order.
    evidence: Mutex<Vec<kbf_invocation::Evidence>>,
}

impl Dispatch for Queue {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let (tx, stage) = watch::channel(Stage::Queued);
        self.evidence
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(submission.evidence);
        let mut held = self
            .submitted
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let name = format!("operations/{}", held.len());
        held.push(tx);
        let key = kbf_types::ActionKey::try_from(&submission.request.key).expect("an action's key");
        Ok(Ticket {
            name,
            instance: key.instance,
            action: key.action,
            stage,
        })
    }

    fn wait(&self, _name: &str) -> Option<Ticket> {
        None
    }
}

/// Three nodes' storage, as the router reaches them: the node's cache storage, with
/// every FindMissing taking [`FIND`].
struct Slow(Arc<dyn Storage>);

impl Storage for Slow {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async move {
            tokio::time::sleep(FIND).await;
            self.0.find_missing(digests).await
        })
    }

    /// Forwards: the inner storage decides what is durable.
    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.0.find_durable(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        self.0.read_blob(digest)
    }

    fn read_range<'a>(&'a self, d: &'a Digest, offset: u64, limit: u64) -> Answer<'a, ReadStream> {
        self.0.read_range(d, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.0.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.0.stream(size)
    }

    fn put_streamed(
        &self,
        size: u64,
        bytes: futures::stream::BoxStream<'static, std::io::Result<Bytes>>,
        claim: futures::future::BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        self.0.put_streamed(size, bytes, claim)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.0.segments()
    }

    fn action_result<'a>(&'a self, a: &'a Digest) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.0.action_result(a)
    }

    fn prepare_action_result<'a>(&'a self, r: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.0.prepare_action_result(r)
    }

    fn commit_action_record(&self, role: Role, a: Digest, r: ActionRecord) -> Answer<'_, ()> {
        self.0.commit_action_record(role, a, r)
    }
}

type NodeCache = Cache<MemoryMetaLog, Store>;

/// Three storage nodes in process and the map of them, every member's boot known (so
/// the router has an epoch and the walk may remember).
struct Farm {
    caches: BTreeMap<MemberId, Arc<NodeCache>>,
    nodes: BTreeMap<MemberId, Arc<dyn Storage>>,
    map: Arc<Map>,
}

impl Farm {
    async fn new() -> Arc<Self> {
        let mut caches = BTreeMap::new();
        let mut nodes = BTreeMap::new();
        for id in 1..=3 {
            let store = Store {
                inner: MemoryStore::new(Capabilities::default()),
                delay: GET,
            };
            let log = MemoryMetaLog::new(Retention::default());
            let cache = Arc::new(
                Cache::open(log, store, KeyPrefix::default())
                    .await
                    .expect("open"),
            );
            let storage = CacheStorage::new(Arc::clone(&cache), SegmentsConfig::DEFAULT);
            nodes.insert(
                MemberId(id),
                Arc::new(Slow(Arc::new(storage))) as Arc<dyn Storage>,
            );
            caches.insert(MemberId(id), cache);
        }
        let members = (1..=3)
            .map(|id| Member {
                id: MemberId(id),
                addr: format!("127.0.0.1:{id}"),
                zone: String::new(),
                weight: 1,
                state: MemberState::Active,
                boot: 1,
            })
            .collect();
        let map = Map::new(1, members, Rule::Rendezvous, 1, 1, Vec::new()).expect("a map");
        Arc::new(Self {
            caches,
            nodes,
            map: Arc::new(map),
        })
    }

    /// Each blob on the node the map writes it to.
    async fn store(&self, blobs: &[&Blob]) {
        for b in blobs {
            let owner = self.map.write_owners(&b.digest)[0];
            self.caches[&owner]
                .store_blobs(vec![b.verified.clone()])
                .await
                .expect("store");
        }
    }

    /// An Execute service routed by the map, and the backend it reads, for its metrics.
    fn service(self: &Arc<Self>) -> (Arc<ExecutionService<Queue>>, Backend) {
        self.service_over(Arc::new(Queue::default()))
    }

    /// [`Farm::service`] over `queue`, which the caller keeps to read what was submitted.
    fn service_over(
        self: &Arc<Self>,
        queue: Arc<Queue>,
    ) -> (Arc<ExecutionService<Queue>>, Backend) {
        let routed = Arc::new(Routed::new(self.clone(), self.clone(), self.clone()));
        let backend = Backend::routed(routed, Arc::clone(&self.nodes[&MemberId(1)]));
        let (_, closing) = kbf_front::closing();
        let service = ExecutionService::with_authorizers(
            backend.clone(),
            queue,
            closing,
            Arc::new(Authorizers::allow_all()),
        );
        (Arc::new(service), backend)
    }
}

impl Nodes for Farm {
    fn node(&self, member: &Member, _: u64) -> Result<Arc<dyn Storage>, StorageError> {
        Ok(Arc::clone(&self.nodes[&member.id]))
    }
}

impl Channels for Farm {
    fn channel(&self, _: &Member) -> Result<tonic::transport::Channel, StorageError> {
        Err(StorageError::Internal("no channels in process".to_owned()))
    }
}

impl MapSource for Farm {
    fn current(&self) -> Option<MapView> {
        Some(MapView {
            map: Arc::clone(&self.map),
            previous: None,
            earlier: Vec::new(),
        })
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            Err(RefetchError {
                at_least,
                reason: "nothing newer".to_owned(),
            })
        })
    }
}

struct Blob {
    verified: VerifiedBlob,
    digest: Digest,
}

fn blob(data: Vec<u8>) -> Blob {
    let verified = VerifiedBlob::hashed(Bytes::from(data));
    let digest = verified.digest();
    Blob { verified, digest }
}

fn proto(blob: &Blob) -> kbf_proto::reapi::Digest {
    kbf_proto::reapi::Digest {
        hash: blob.digest.hash_hex(),
        size_bytes: i64::try_from(blob.digest.size_bytes).expect("size"),
    }
}

fn file_node(name: &str, blob: &Blob) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(proto(blob)),
        ..Default::default()
    }
}

/// An action over a root of three files and a subdirectory of one file: two Directories
/// read, four files asked about, all held.
struct Job {
    action: Blob,
    blobs: Vec<Blob>,
}

fn job() -> Job {
    job_running("walk")
}

/// [`job`] with a Command of its own: the same input tree, another action.
fn job_running(argument: &str) -> Job {
    let files: Vec<Blob> = (0..4)
        .map(|n| blob(format!("file {n}").into_bytes()))
        .collect();
    let sub = blob(
        Directory {
            files: vec![file_node("f", &files[3])],
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let root = blob(
        Directory {
            files: (0..3)
                .map(|n| file_node(&format!("f{n}"), &files[n]))
                .collect(),
            directories: vec![DirectoryNode {
                name: "sub".to_owned(),
                digest: Some(proto(&sub)),
            }],
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let command = blob(
        Command {
            arguments: vec![argument.to_owned()],
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let action = blob(
        Action {
            command_digest: Some(proto(&command)),
            input_root_digest: Some(proto(&root)),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let mut blobs = vec![command, root, sub];
    blobs.extend(files);
    Job { action, blobs }
}

/// One Execute, to the point of its first operation.
async fn execute(service: &Arc<ExecutionService<Queue>>, job: &Job) {
    let request = ExecuteRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(proto(&job.action)),
        ..Default::default()
    };
    service
        .execute(tonic::Request::new(request))
        .await
        .expect("Execute");
}

/// The log line the Execute of `job` that is `nth` (0-based) warned with.
fn warning(job: &Job, nth: usize) -> String {
    let action = job.action.digest;
    let lines = log_lines();
    let found: Vec<_> = lines
        .iter()
        .filter(|l| l.contains(&format!("action={action}")) && l.contains("presubmit_ms="))
        .cloned()
        .collect();
    assert!(found.len() > nth, "warning {nth} missing from {lines:?}");
    found[nth].clone()
}

fn field(line: &str, name: &str) -> String {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("{name} missing from {line}"))
        .to_owned()
}

/// Catches, over an Execute (not the walk alone):
/// - `dirs_skipped` fed from the Directories read (in the warning or the metric): the
///   first Execute reads two and skips none, the second reads none and skips one, so
///   each source gives another figure on one of them.
/// - the warning's `files_ms` fed from the Directories' time (or `dirs_ms` from the
///   files'): the first walk spends 12 s reading two Directories in a chain and 3 s on
///   FindMissing, every figure of the line distinct.
/// - the same swap in the phase histograms (`kbf_execute_walk_dirs_seconds` taking the
///   files' time, and the reverse), and in the counters: reads 2, skipped 1, asks 1,
///   files 4 (no two alike but skipped and asks).
/// - a warm Execute that spends time on a walk it skipped.
/// The action and the Command are two more GETs, so each Execute is 12 s before the walk
/// and slow enough (10 s) to warn.
#[tokio::test(start_paused = true)]
async fn an_execute_reports_what_its_walk_read_skipped_and_spent() {
    let _ = log_lines();
    let farm = Farm::new().await;
    let job = job();
    farm.store(&[&job.action]).await;
    farm.store(&job.blobs.iter().collect::<Vec<_>>()).await;
    let (service, backend) = farm.service();
    let m = backend.metrics();

    execute(&service, &job).await;
    let cold = warning(&job, 0);
    assert_eq!(field(&cold, "dir_reads"), "2", "{cold}");
    assert_eq!(field(&cold, "dirs_skipped"), "0", "{cold}");
    assert_eq!(field(&cold, "dir_cache_hits"), "0", "{cold}");
    assert_eq!(field(&cold, "dirs_present_checked"), "0", "{cold}");
    assert_eq!(
        field(&cold, "dirs_ms"),
        "12000",
        "two Directories in a chain: {cold}"
    );
    assert_eq!(
        field(&cold, "files_ms"),
        "3000",
        "one round of FindMissing: {cold}"
    );
    assert_eq!(field(&cold, "file_asks"), "1", "{cold}");
    assert_eq!(field(&cold, "files_checked"), "4", "{cold}");
    assert_eq!(field(&cold, "walk_ms"), "15000", "{cold}");
    assert_eq!(field(&cold, "presubmit_ms"), "27000", "{cold}");

    execute(&service, &job).await;
    let warm = warning(&job, 1);
    assert_eq!(field(&warm, "dir_reads"), "0", "{warm}");
    assert_eq!(field(&warm, "dirs_skipped"), "1", "{warm}");
    assert_eq!(field(&warm, "dir_cache_hits"), "0", "{warm}");
    assert_eq!(field(&warm, "dirs_present_checked"), "0", "{warm}");
    assert_eq!(field(&warm, "dirs_ms"), "0", "{warm}");
    assert_eq!(field(&warm, "files_ms"), "0", "{warm}");
    assert_eq!(field(&warm, "file_asks"), "0", "{warm}");
    assert_eq!(field(&warm, "files_checked"), "0", "{warm}");
    assert_eq!(field(&warm, "walk_ms"), "0", "{warm}");
    assert_eq!(field(&warm, "presubmit_ms"), "12000", "{warm}");

    assert_eq!(m.execute_walk_dirs_read.get(), 2);
    assert_eq!(m.execute_walk_dirs_skipped.get(), 1);
    assert_eq!(m.execute_walk_file_asks.get(), 1);
    assert_eq!(m.execute_walk_files_checked.get(), 4);
    assert_eq!(m.execute_walk_dirs.count(), 2);
    assert_eq!(
        m.execute_walk_dirs.sum(),
        12_000_000_000,
        "the cold walk's 12 s, the warm 0"
    );
    assert_eq!(m.execute_walk_files.count(), 2);
    assert_eq!(
        m.execute_walk_files.sum(),
        3_000_000_000,
        "the cold walk's 3 s, the warm 0"
    );
    assert_eq!(m.execute_walk.sum(), 15_000_000_000);
    assert_eq!(
        m.execute_walk_dir_read.count(),
        2,
        "each read of the cold walk"
    );
    assert_eq!(m.execute_walk_dir_read.sum(), 12_000_000_000);
    assert_eq!(m.execute_dir_cache_misses.get(), 2);
    assert_eq!(m.execute_dir_cache_hits.get(), 0);
    assert_eq!(m.execute_dir_cache_present_checked.get(), 0);
    // The root names 3 files and a Directory, the other one file: 5 digests and two
    // entries of 160 bytes of overhead.
    let held = 5 * std::mem::size_of::<Digest>() as u64 + 2 * 160;
    assert_eq!(m.execute_dir_cache_bytes.get(), held);

    // Past the verified memory's 45 s the tree is not skipped; its Directories are in
    // the content cache, so none is read, and the store is asked about the two of them
    // with the four files, in one call. The Execute spends the files' 3 s only.
    tokio::time::advance(Duration::from_secs(46)).await;
    execute(&service, &job).await;
    let cached = warning(&job, 2);
    assert_eq!(field(&cached, "dir_reads"), "0", "{cached}");
    assert_eq!(field(&cached, "dirs_skipped"), "0", "{cached}");
    assert_eq!(field(&cached, "dir_cache_hits"), "2", "{cached}");
    assert_eq!(field(&cached, "dirs_present_checked"), "2", "{cached}");
    assert_eq!(field(&cached, "dirs_ms"), "0", "{cached}");
    assert_eq!(field(&cached, "files_ms"), "3000", "{cached}");
    assert_eq!(field(&cached, "file_asks"), "1", "{cached}");
    assert_eq!(field(&cached, "files_checked"), "4", "{cached}");
    assert_eq!(field(&cached, "walk_ms"), "3000", "{cached}");
    assert_eq!(field(&cached, "presubmit_ms"), "15000", "{cached}");
    assert_eq!(m.execute_dir_cache_hits.get(), 2);
    assert_eq!(m.execute_dir_cache_present_checked.get(), 2);
    assert_eq!(m.execute_dir_cache_misses.get(), 2, "no new lead");
    assert_eq!(m.execute_dir_cache_merged.get(), 0);
    assert_eq!(m.execute_dir_cache_evictions.get(), 0);
    assert_eq!(m.execute_dir_cache_bytes.get(), held);
    assert_eq!(m.execute_walk_dirs_read.get(), 2, "nothing read");
    assert_eq!(m.execute_walk_dir_read.count(), 2, "and so nothing timed");
    assert_eq!(m.execute_walk_files.count(), 3);
    assert_eq!(m.execute_walk_files.sum(), 6_000_000_000);

    let text = backend.render_metrics();
    for line in [
        "kbf_execute_dir_cache_hits_total 2",
        "kbf_execute_dir_cache_misses_total 2",
        "kbf_execute_dir_cache_present_checked_total 2",
        "kbf_execute_walk_dir_read_seconds_count 2",
    ] {
        assert!(text.lines().any(|l| l == line), "{line} missing");
    }
}

/// Catches the warning's (or the metric's) `dirs_present_checked` fed from
/// `dir_cache_hits`: the two are 2 and 2 for an Execute that takes both Directories
/// from the cache, so nothing told them apart. Two Executes of different actions over
/// one input tree start together. One reads both Directories; the other waits
/// for each read and takes the Directory it produced, so it has 2 cache hits (both
/// merged) and has asked the store about none of them: `dirs_present_checked` 0. A
/// cached Directory is the only one the store is asked about.
#[tokio::test(start_paused = true)]
async fn a_directory_taken_from_a_read_in_flight_is_a_hit_and_is_not_asked_about() {
    let _ = log_lines();
    let farm = Farm::new().await;
    let (first, second) = (job_running("first"), job_running("second"));
    farm.store(&[&first.action, &second.action]).await;
    farm.store(&first.blobs.iter().collect::<Vec<_>>()).await;
    farm.store(&[&second.blobs[0]]).await;
    let (service, backend) = farm.service();
    let m = backend.metrics();

    tokio::join!(execute(&service, &first), execute(&service, &second));

    // Which of the two reads is down to who polls first; the other waits.
    let (mut read, mut waited) = (warning(&first, 0), warning(&second, 0));
    if field(&read, "dir_reads") == "0" {
        std::mem::swap(&mut read, &mut waited);
    }
    assert_eq!(field(&read, "dir_reads"), "2", "{read}");
    assert_eq!(field(&read, "dir_cache_hits"), "0", "{read}");
    assert_eq!(field(&read, "dirs_present_checked"), "0", "{read}");
    assert_eq!(field(&read, "files_checked"), "4", "{read}");
    assert_eq!(field(&waited, "dir_reads"), "0", "{waited}");
    assert_eq!(field(&waited, "dirs_skipped"), "0", "{waited}");
    assert_eq!(field(&waited, "dir_cache_hits"), "2", "{waited}");
    assert_eq!(
        field(&waited, "dirs_present_checked"),
        "0",
        "a Directory taken from a read in flight was not asked about: {waited}"
    );
    assert_eq!(field(&waited, "files_checked"), "4", "{waited}");

    assert_eq!(m.execute_walk_dirs_read.get(), 2);
    assert_eq!(m.execute_dir_cache_misses.get(), 2);
    assert_eq!(m.execute_dir_cache_hits.get(), 2);
    assert_eq!(m.execute_dir_cache_merged.get(), 2);
    assert_eq!(m.execute_dir_cache_present_checked.get(), 0);
}

/// Where the global subscriber writes.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The lines logged so far, the global subscriber installed on first use.
fn log_lines() -> Vec<String> {
    static LOG: OnceLock<Log> = OnceLock::new();
    let log = LOG.get_or_init(|| {
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");
        log
    });
    let bytes = log.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
    String::from_utf8(bytes)
        .expect("utf-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Catches, over an Execute: the input totals of a full walk wrong (summed from the
/// Directories instead of the distinct files, a file counted twice, size and count
/// swapped), or a walk that skipped a verified subtree reporting a total of what it did
/// see (a lower bound that reads as a size) instead of nothing. The cold Execute reads
/// both Directories: four files of six bytes. The warm one skips the verified root and
/// saw no file, so its totals are absent, not 0.
#[tokio::test(start_paused = true)]
async fn the_input_totals_are_those_of_a_full_walk_and_absent_after_a_skip() {
    let farm = Farm::new().await;
    // Another action than the other tests': they read the process's log by action.
    let job = job_running("totals");
    farm.store(&[&job.action]).await;
    farm.store(&job.blobs.iter().collect::<Vec<_>>()).await;
    let queue = Arc::new(Queue::default());
    let (service, _) = farm.service_over(Arc::clone(&queue));

    execute(&service, &job).await;
    execute(&service, &job).await;

    let seen = queue.evidence.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].input_files, Some(4), "four distinct files");
    assert_eq!(seen[0].input_bytes, Some(24), "'file n' is six bytes");
    assert_eq!(seen[1].input_files, None, "the root was skipped");
    assert_eq!(seen[1].input_bytes, None, "not 0: unknown");
}
