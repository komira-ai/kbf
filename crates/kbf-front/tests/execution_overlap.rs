//! Execute's reads overlap where nothing orders them, and read nothing more for it: the Action
//! is read while the action cache is looked up, and the Command while the input tree is walked
//! (its Directory reads and its held-file check); the answer is still made in the order the
//! reads would have run one after another.
//!
//! The store is a real in-memory storage node behind a wrapper that records every call it
//! takes, in the order taken, and holds the answer of the calls a test names for an hour of
//! tokio's paused clock (or fails them). A call another one is overlapped with is then in the
//! record before the held call's answer; one that waits for it comes after. The time passes
//! only when nothing else can run, and no test reads the clock.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::{
    Backend, Cache, CacheStorage, Closer, Dispatch, ExecutionService, MemoryMetaLog,
    SegmentsConfig, Stage, Submission, Ticket,
};
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::MemoryStore;
use kbf_proto::google::rpc::{self, PreconditionFailure};
use kbf_proto::reapi::execution_server::Execution as _;
use kbf_proto::reapi::{
    self, Action, ActionResult, Command, Directory, ExecuteRequest, FileNode, Platform,
};
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use tokio::sync::watch;
use tonic::{Code, Status};

/// How long a held call waits before it is answered: long, so that only a call that waits
/// for it comes after its answer.
const HOLD: Duration = Duration::from_secs(3_600);

/// A call the store takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    /// An action-cache lookup.
    Lookup,
    /// A read of a blob.
    Read(Digest),
    /// A FindMissing call.
    Ask,
    /// Any other call, by its name.
    Other(&'static str),
}

/// What the store recorded: a call taken, or the answer of a held call given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Made(Call),
    Answered(Call),
}

/// A storage node that records every call and holds or fails the calls it is told to.
struct Recorded {
    inner: Arc<dyn Storage>,
    log: Mutex<Vec<Event>>,
    held: Mutex<Vec<Call>>,
    failing: Mutex<Vec<Call>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn down() -> StorageError {
    StorageError::Unavailable(Arc::new(std::io::Error::other("the store is down")))
}

impl Recorded {
    /// `call` taken: recorded, then failed, held for [`HOLD`] (its answer recorded when
    /// given) or answered by `inner` at once, as the test said.
    fn take<'a, T: Send + 'a>(&'a self, call: Call, inner: Answer<'a, T>) -> Answer<'a, T> {
        lock(&self.log).push(Event::Made(call));
        if lock(&self.failing).contains(&call) {
            return Box::pin(async { Err(down()) });
        }
        if !lock(&self.held).contains(&call) {
            return inner;
        }
        Box::pin(async move {
            tokio::time::sleep(HOLD).await;
            lock(&self.log).push(Event::Answered(call));
            inner.await
        })
    }

    /// The calls taken, in the order taken.
    fn made(&self) -> Vec<Call> {
        let log = lock(&self.log);
        log.iter()
            .filter_map(|event| match event {
                Event::Made(call) => Some(*call),
                Event::Answered(_) => None,
            })
            .collect()
    }

    /// Where `event` is in the record.
    fn at(&self, event: Event) -> usize {
        let log = lock(&self.log);
        log.iter()
            .position(|e| *e == event)
            .unwrap_or_else(|| panic!("{event:?} is not in {log:?}"))
    }
}

impl Storage for Recorded {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.take(Call::Ask, self.inner.find_missing(digests))
    }

    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        let inner = self.inner.find_durable(digests);
        self.take(Call::Other("find_durable"), inner)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        self.take(Call::Read(*digest), self.inner.read_blob(digest))
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        let inner = self.inner.read_range(digest, offset, limit);
        self.take(Call::Other("read_range"), inner)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.take(Call::Other("put"), self.inner.put(bytes))
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        lock(&self.log).push(Event::Made(Call::Other("stream")));
        self.inner.stream(size)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.take(Call::Lookup, self.inner.action_result(action))
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        let inner = self.inner.prepare_action_result(result);
        self.take(Call::Other("prepare_action_result"), inner)
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        let inner = self.inner.commit_action_record(role, action, record);
        self.take(Call::Other("commit_action_record"), inner)
    }
}

/// A dispatch that takes every submission and runs nothing.
#[derive(Default)]
struct Taken {
    submitted: Mutex<Vec<Submission>>,
}

impl Dispatch for Taken {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let key = submission.key.clone();
        lock(&self.submitted).push(submission);
        Ok(Ticket {
            name: "operations/1".to_owned(),
            instance: key.instance,
            action: key.action,
            stage: watch::channel(Stage::Queued).1,
        })
    }

    fn wait(&self, _name: &str) -> Option<Ticket> {
        None
    }

    fn cancel(&self, _name: &str) -> bool {
        false
    }
}

/// An action: its digest, and the digests of its Command, its input root and its one file.
#[derive(Clone, Copy, Debug)]
struct Job {
    action: Digest,
    command: Digest,
    root: Digest,
    file: Digest,
}

/// The Execution service over one node behind [`Recorded`], as a frontend with no packs
/// sets it up.
struct Rig {
    cache: Arc<Cache<MemoryMetaLog, MemoryStore>>,
    store: Arc<Recorded>,
    dispatch: Arc<Taken>,
    service: ExecutionService<Taken>,
    _closer: Closer,
}

impl Rig {
    fn new() -> Self {
        let cache = Arc::new(Cache::memory());
        let inner: Arc<dyn Storage> = Arc::new(CacheStorage::new(
            Arc::clone(&cache),
            SegmentsConfig::DEFAULT,
        ));
        let store = Arc::new(Recorded {
            inner,
            log: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
            failing: Mutex::new(Vec::new()),
        });
        let storage: Arc<dyn Storage> = store.clone();
        let dispatch = Arc::new(Taken::default());
        let (closer, closing) = kbf_front::closing();
        let service = ExecutionService::with_authorizers(
            Backend::remote(storage),
            Arc::clone(&dispatch),
            closing,
            Arc::new(Authorizers::allow_all()),
        );
        Self {
            cache,
            store,
            dispatch,
            service,
            _closer: closer,
        }
    }

    /// Stores `bytes` (not through the record) and answers its digest.
    async fn put(&self, bytes: Bytes) -> Digest {
        self.store
            .inner
            .put(bytes)
            .await
            .expect("the blob is stored")
    }

    /// The action `name`, with `platform` in its Command, all of it stored but what `absent`
    /// names: `command` and `file` leave those blobs out.
    async fn job(&self, name: &str, platform: &[(&str, &str)], absent: &[&str]) -> Job {
        let file_bytes = Bytes::from(format!("input of {name}"));
        let file = kbf_segments::sha256(&file_bytes);
        if !absent.contains(&"file") {
            self.put(file_bytes).await;
        }
        let root = Directory {
            files: vec![FileNode {
                name: "in".to_owned(),
                digest: Some(kbf_front::digest_to_proto(&file)),
                ..Default::default()
            }],
            ..Default::default()
        };
        let root = self.put(Bytes::from(root.encode_to_vec())).await;
        #[allow(deprecated)] // The Command's platform, as clients before REAPI 2.2 set it.
        let command = Command {
            arguments: vec![name.to_owned()],
            platform: (!platform.is_empty()).then(|| Platform {
                properties: platform
                    .iter()
                    .map(|(name, value)| reapi::platform::Property {
                        name: (*name).to_owned(),
                        value: (*value).to_owned(),
                    })
                    .collect(),
            }),
            ..Default::default()
        };
        let command_bytes = Bytes::from(command.encode_to_vec());
        let command = kbf_segments::sha256(&command_bytes);
        if !absent.contains(&"command") {
            self.put(command_bytes).await;
        }
        let action = Action {
            command_digest: Some(kbf_front::digest_to_proto(&command)),
            input_root_digest: Some(kbf_front::digest_to_proto(&root)),
            ..Default::default()
        };
        let action = self.put(Bytes::from(action.encode_to_vec())).await;
        Job {
            action,
            command,
            root,
            file,
        }
    }

    /// Holds the answer of `call` for [`HOLD`].
    fn hold(&self, call: Call) {
        lock(&self.store.held).push(call);
    }

    /// Fails `call` UNAVAILABLE.
    fn fail(&self, call: Call) {
        lock(&self.store.failing).push(call);
    }

    /// Executes `job` (with the action-cache lookup unless `skip`): the status of a refusal,
    /// if refused.
    async fn execute(&self, job: &Job, skip: bool) -> Result<(), Status> {
        let request = ExecuteRequest {
            instance_name: "main".to_owned(),
            action_digest: Some(kbf_front::digest_to_proto(&job.action)),
            skip_cache_lookup: skip,
            ..ExecuteRequest::default()
        };
        let answered = self.service.execute(tonic::Request::new(request)).await;
        answered.map(drop)
    }

    /// How many submissions the dispatch took.
    fn submitted(&self) -> usize {
        lock(&self.dispatch.submitted).len()
    }

    /// Writes a cached result for `job`'s action.
    async fn cache_result(&self, job: &Job) {
        let result = ActionResult::default();
        let written = self
            .cache
            .write_action_result(Role::Daemon, job.action, &result)
            .await;
        written.expect("the result is cached");
    }
}

/// The subjects of a FAILED_PRECONDITION's MISSING violations, in order.
fn missing_subjects(status: &Status) -> Vec<String> {
    assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
    let details = rpc::Status::decode(status.details()).expect("details decode");
    let [any] = details.details.as_slice() else {
        panic!("one detail expected: {details:?}");
    };
    PreconditionFailure::decode(any.value.as_slice())
        .expect("a PreconditionFailure")
        .violations
        .into_iter()
        .map(|v| {
            assert_eq!(v.r#type, "MISSING");
            v.subject
        })
        .collect()
}

fn subject(digest: &Digest) -> String {
    format!("blobs/{}/{}", digest.hash_hex(), digest.size_bytes)
}

/// Asserts that the Action is read while the action-cache lookup is still unanswered, and that
/// the Execute then goes on to submit: the lookup is held, and the Action's read is in the
/// record before the lookup's answer. Catches an Action read that waits for the lookup to end.
#[tokio::test(start_paused = true)]
async fn the_action_is_read_while_the_cache_lookup_is_unanswered() {
    let rig = Rig::new();
    let job = rig.job("overlapped with the lookup", &[], &[]).await;
    rig.hold(Call::Lookup);

    rig.execute(&job, false).await.expect("submitted");

    let read = rig.store.at(Event::Made(Call::Read(job.action)));
    let looked = rig.store.at(Event::Answered(Call::Lookup));
    assert!(
        read < looked,
        "the Action read waited for the lookup: {:?}",
        lock(&rig.store.log)
    );
    assert_eq!(rig.submitted(), 1);
}

/// Asserts that the Command is read while the input tree is walked: with the Command's read
/// held, the walk's read of the root Directory and its FindMissing of the file are both in the
/// record before the Command's answer, and the Execute submits once the Command is read.
/// Catches a walk, or its held-file check, that waits for the Command.
#[tokio::test(start_paused = true)]
async fn the_command_is_read_while_the_input_tree_is_walked() {
    let rig = Rig::new();
    let job = rig.job("overlapped with the walk", &[], &[]).await;
    rig.hold(Call::Read(job.command));

    rig.execute(&job, true).await.expect("submitted");

    let command = rig.store.at(Event::Answered(Call::Read(job.command)));
    for call in [Call::Read(job.root), Call::Ask] {
        let made = rig.store.at(Event::Made(call));
        assert!(
            made < command,
            "{call:?} waited for the Command: {:?}",
            lock(&rig.store.log)
        );
    }
    assert_eq!(rig.submitted(), 1);
}

/// Asserts the calls of one Execute, each made exactly once and nothing else asked of the
/// store: with the lookup, a miss looks up, reads the Action, the Command and the root
/// Directory and asks one FindMissing; with `skip_cache_lookup`, the same without the lookup;
/// a hit looks up, reads the Action once and the Command once (for its platform), walks
/// nothing and submits nothing. The counts are those of the same Execute with its reads one
/// after another. Catches an overlap that reads a blob twice (the Action again for a hit's
/// platform check, say), walks for a hit, or looks up when told to skip.
#[tokio::test(start_paused = true)]
async fn an_execute_makes_each_call_once_and_a_hit_walks_nothing() {
    let sorted = |mut calls: Vec<Call>| {
        calls.sort_by_key(|call| format!("{call:?}"));
        calls
    };
    for (why, skip) in [("a miss", false), ("skipped", true)] {
        let rig = Rig::new();
        let job = rig.job(why, &[], &[]).await;
        rig.execute(&job, skip).await.expect(why);
        let mut expected = vec![
            Call::Read(job.action),
            Call::Read(job.command),
            Call::Read(job.root),
            Call::Ask,
        ];
        if !skip {
            expected.push(Call::Lookup);
        }
        assert_eq!(sorted(rig.store.made()), sorted(expected), "{why}");
        assert_eq!(rig.submitted(), 1, "{why}");
    }

    let rig = Rig::new();
    let job = rig.job("a hit", &[], &[]).await;
    rig.cache_result(&job).await;
    rig.hold(Call::Lookup);
    rig.execute(&job, false)
        .await
        .expect("answered from the cache");
    let expected = vec![
        Call::Lookup,
        Call::Read(job.action),
        Call::Read(job.command),
    ];
    assert_eq!(sorted(rig.store.made()), sorted(expected), "a hit");
    let read = rig.store.at(Event::Made(Call::Read(job.action)));
    let looked = rig.store.at(Event::Answered(Call::Lookup));
    assert!(read < looked, "the hit's Action read waited for the lookup");
    assert_eq!(rig.submitted(), 0, "a hit runs nothing");
}

/// Asserts that an input the store does not hold is still answered MISSING while the Command's
/// read runs beside the walk, and that nothing is submitted: the file absent alone is the one
/// violation; the Command absent too, both are, the Command's first, as when the reads ran one
/// after another. Catches a MISSING found by the walk lost while the Command is read, a job
/// submitted for an action with an absent input, and violations put out of order.
#[tokio::test(start_paused = true)]
async fn an_absent_input_is_missing_and_nothing_is_submitted() {
    let rig = Rig::new();
    let job = rig.job("file absent", &[], &["file"]).await;
    rig.hold(Call::Read(job.command));
    let status = rig.execute(&job, false).await.expect_err("missing");
    assert_eq!(missing_subjects(&status), vec![subject(&job.file)]);
    assert!(
        rig.store.at(Event::Made(Call::Ask))
            < rig.store.at(Event::Answered(Call::Read(job.command))),
        "the walk's check ran beside the Command's read"
    );

    let both = rig
        .job("command and file absent", &[], &["command", "file"])
        .await;
    let status = rig.execute(&both, false).await.expect_err("missing");
    assert_eq!(
        missing_subjects(&status),
        vec![subject(&both.command), subject(&both.file)]
    );
    assert_eq!(rig.submitted(), 0, "nothing runs for an absent input");
}

/// Asserts that the answers are put in the order the reads would have run one after another,
/// whichever read ends first:
/// - a lookup that fails is the answer though the Action read beside it succeeded, and no
///   Command is read and no tree walked after it;
/// - a lookup that hits beside an Action read the store fails is UNAVAILABLE, not the cached
///   result: the hit's platform cannot be checked, as when the Action was read after it;
/// - a Command whose platform is refused is INVALID_ARGUMENT, with no MISSING details, over a
///   walk that found the file absent (the walk's answer comes first, the Command's is held);
/// - a Command the store fails to read is UNAVAILABLE over a walk that found the file absent;
/// - a walk the store fails (the root Directory unreadable) is UNAVAILABLE beside a Command
///   read that succeeded.
///
/// Nothing is submitted in any case. Catches an overlap that answers the first read to end
/// (MISSING for a refused platform, so the client uploads for nothing), or that drops a
/// failure for the other read's answer.
#[tokio::test(start_paused = true)]
async fn the_answers_keep_the_order_of_reads_made_one_after_another() {
    let rig = Rig::new();
    let job = rig.job("lookup fails", &[], &[]).await;
    rig.fail(Call::Lookup);
    let status = rig.execute(&job, false).await.expect_err("unavailable");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert_eq!(rig.store.made(), vec![Call::Lookup, Call::Read(job.action)]);

    let rig = Rig::new();
    let job = rig.job("hit, action unreadable", &[], &[]).await;
    rig.cache_result(&job).await;
    rig.fail(Call::Read(job.action));
    let status = rig.execute(&job, false).await.expect_err("unavailable");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert_eq!(rig.store.made(), vec![Call::Lookup, Call::Read(job.action)]);

    let rig = Rig::new();
    let job = rig
        .job("refused platform", &[("kbf-lease", "vm")], &["file"])
        .await;
    rig.hold(Call::Read(job.command));
    let status = rig.execute(&job, false).await.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.details().is_empty(), "not MISSING: {status:?}");
    assert!(
        rig.store.at(Event::Made(Call::Ask))
            < rig.store.at(Event::Answered(Call::Read(job.command))),
        "the walk ended first"
    );

    let rig = Rig::new();
    let job = rig.job("command unreadable", &[], &["file"]).await;
    rig.fail(Call::Read(job.command));
    let status = rig.execute(&job, false).await.expect_err("unavailable");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");

    let rig = Rig::new();
    let job = rig.job("root unreadable", &[], &[]).await;
    rig.fail(Call::Read(job.root));
    let status = rig.execute(&job, false).await.expect_err("unavailable");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert_eq!(rig.submitted(), 0);
}
