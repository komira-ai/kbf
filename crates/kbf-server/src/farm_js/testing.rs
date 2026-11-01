//! What the tests of this module share: a store, a handle on it with this server's codec and
//! rows, and a request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use js_server_core::{Admission, Admitted, Core, Denied, Held, Stream};
use js_store::{EpochPolicy, Rows};
use kbf_caps::NodeCaps;
use kbf_front::{
    Backend, Cache, Described, MemoryMetaLog, MetaLog, MetaLogError, Recorder, Stage, Submission,
    Ticket,
};
use kbf_meta::{Applied, Command as MetaCommand, MetaState, Retention};
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_sched::{DaemonInstance, Request};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::sched::usage::LeaseCost;
use kbf_types::{
    ActionKey, Digest, DigestFunction, FarmTime, LeaseId, LeaseKind, OperationId, Qos, Resources,
    WaiterId, WorkerId,
};
use tokio::sync::{mpsc, watch};
use tonic::Status;

use super::codec::KbfRowCodec;
use super::ext::KbfExt;
use super::{Caller, Connection, KbfAdapter, Wall, dispatch, open};
use crate::PlacementLoad;
use crate::action_usage::{Shapes, Subject};
use crate::farm::Reported;
use crate::farm::durable::subject;
use crate::machine::Platform;

/// A fresh store with the marker that says several writers share it.
pub(super) fn store() -> Arc<Fake> {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    kbf_state::share(&*store).expect("the shared marker");
    store
}

/// A handle of frontend `frontend` on `store`.
pub(super) fn rows(store: &Arc<Fake>, frontend: u64) -> Rows {
    let handle: Arc<dyn Store> = store.clone();
    let codec = Arc::new(KbfRowCodec);
    let (rows, _) =
        Rows::open(handle, EpochPolicy::Keep, frontend, codec, Arc::new(KbfExt)).expect("open");
    rows
}

/// A request for the action of digest tag `tag`.
pub(super) fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
        whole_worker: false,
    }
}

/// The value of a `subject/` row: what the `action usage:` line says of an operation that has
/// nothing to say.
pub(super) fn parcel() -> Vec<u8> {
    subject::encode(&Subject::of(Described::default()).0)
}

/// A cost with every field set.
pub(super) fn charged() -> LeaseCost {
    LeaseCost {
        cpu_millis: 4,
        memory_bytes: 5,
        limit_bytes: 6,
        cold: true,
        retry: true,
        key_hash: 7,
    }
}

/// Puts `value` at `key`, whatever is there.
pub(super) fn overwrite(store: &Fake, key: &str, value: Vec<u8>) {
    let txn = Txn::new().put(key.to_owned(), value, Cond::Any);
    store.txn(txn).expect("a store").expect("a write");
}

/// A store that can be made to refuse: reads of the rows under a prefix fail, and every
/// write fails, until it is told otherwise.
pub(super) struct Flaky {
    inner: Fake,
    unreadable: Mutex<Vec<String>>,
    ungettable: Mutex<Vec<(String, usize)>>,
    down: AtomicBool,
    fenced: AtomicBool,
}

impl Flaky {
    pub(super) fn new() -> Arc<Self> {
        let clock = Arc::new(ManualClock::default());
        let inner = Fake::with_flaw(clock, Flaw::None);
        kbf_state::share(&inner).expect("the shared marker");
        Arc::new(Self {
            inner,
            unreadable: Mutex::new(Vec::new()),
            ungettable: Mutex::new(Vec::new()),
            down: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
        })
    }

    /// Reads of the rows under `prefix` fail from now on, or work again (`None`).
    pub(super) fn fail_reads(&self, prefix: Option<&str>) {
        let mut unreadable = self.unreadable.lock().expect("lock");
        unreadable.clear();
        unreadable.extend(prefix.map(str::to_owned));
    }

    /// Reads of one row (not scans) under `prefix` fail from now on, or work again (`None`).
    pub(super) fn fail_gets(&self, prefix: Option<&str>) {
        self.fail_gets_after(prefix, 0);
    }

    /// Reads of one row under `prefix` work `allowed` more times, then fail.
    pub(super) fn fail_gets_after(&self, prefix: Option<&str>, allowed: usize) {
        let mut ungettable = self.ungettable.lock().expect("lock");
        ungettable.clear();
        ungettable.extend(prefix.map(|prefix| (prefix.to_owned(), allowed)));
    }

    /// The store takes writes again, or not.
    pub(super) fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// The store says a newer writer holds it: every write is refused, for good.
    pub(super) fn set_fenced(&self, fenced: bool) {
        self.fenced.store(fenced, Ordering::SeqCst);
    }

    /// The store itself, to write rows into without the refusals.
    pub(super) fn plain(&self) -> &Fake {
        &self.inner
    }

    fn unreadable(&self, key: &str) -> Result<(), StoreError> {
        let unreadable = self.unreadable.lock().expect("lock");
        let failing = unreadable.iter().any(|prefix| key.starts_with(prefix));
        let failed = || StoreError::Failed("the row cannot be read".to_owned());
        failing.then(failed).map_or(Ok(()), Err)
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.fenced.load(Ordering::SeqCst) {
            return Err(StoreError::Fenced);
        }
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.unreadable(key)?;
        let mut ungettable = self.ungettable.lock().expect("lock");
        for (_, allowed) in ungettable
            .iter_mut()
            .filter(|(p, _)| key.starts_with(p.as_str()))
        {
            if *allowed == 0 {
                return Err(StoreError::Failed("the row cannot be read".to_owned()));
            }
            *allowed -= 1;
        }
        drop(ungettable);
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.unreadable(prefix)?;
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// The wall clock of the tests: a fixed moment.
pub(super) fn wall() -> Wall {
    Arc::new(|| UNIX_EPOCH + Duration::from_secs(1_900_000_000))
}

/// A core over a store, and what a test drives it with.
pub(super) struct Rig {
    pub(super) core: Core<KbfAdapter>,
    pub(super) store: Arc<Flaky>,
    pub(super) backend: Backend,
}

/// A metadata log whose next write of an action-cache entry can be made to fail, as an
/// unreachable leader would.
#[derive(Debug)]
pub(super) struct FailingLog {
    inner: MemoryMetaLog,
    fail_action_write: AtomicBool,
}

impl FailingLog {
    /// Fails the next action-cache write with `MetaLogError::Unavailable`.
    pub(super) fn fail_next_action_write(&self) {
        self.fail_action_write.store(true, Ordering::SeqCst);
    }
}

impl MetaLog for FailingLog {
    async fn commit(&self, command: MetaCommand) -> Result<Applied, MetaLogError> {
        let failing = matches!(command, MetaCommand::PutAction { .. })
            && self.fail_action_write.swap(false, Ordering::SeqCst);
        if failing {
            return Err(MetaLogError::Unavailable("failed by the test".to_owned()));
        }
        self.inner.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.inner.query(f).await
    }
}

/// A memory cache over a metadata log that can fail an action-cache write.
pub(super) fn failing_cache() -> Arc<Cache<FailingLog, MemoryStore>> {
    let inner = MemoryMetaLog::new(Retention::default());
    let epoch = inner.alloc_epoch();
    let meta = FailingLog {
        inner,
        fail_action_write: AtomicBool::new(false),
    };
    let objects = MemoryStore::new(Capabilities::default());
    Arc::new(Cache::new(meta, objects, KeyPrefix::default(), epoch))
}

/// A core of term 7 over `store`, answering into a memory cache and telling `recorder`.
pub(super) fn rig_over(store: &Arc<Flaky>, recorder: Recorder) -> Rig {
    rig_caching(store, recorder, Arc::new(Cache::memory()).into())
}

/// A core of term 7 over `store`, answering into `backend` and telling `recorder`.
pub(super) fn rig_caching(store: &Arc<Flaky>, recorder: Recorder, backend: Backend) -> Rig {
    rig_started(store, recorder, backend, crate::EpochPolicy::Keep)
}

/// A core of term 7 over `store` that starts under `policy`, answering into `backend` and
/// telling `recorder`.
pub(super) fn rig_started(
    store: &Arc<Flaky>,
    recorder: Recorder,
    backend: Backend,
    policy: crate::EpochPolicy,
) -> Rig {
    let shared: Arc<dyn Store> = store.clone();
    let adapter =
        KbfAdapter::new(7, backend.clone(), Arc::clone(&shared), wall()).recording(recorder);
    let config = open::config(
        7,
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
        PlacementLoad::Off,
    );
    let core = open::open(&config, adapter, shared, policy, FarmTime::default())
        .expect("a core over the store");
    Rig {
        core,
        store: Arc::clone(store),
        backend,
    }
}

/// A core over an empty store.
pub(super) fn rig() -> Rig {
    rig_over(&Flaky::new(), Recorder::off())
}

/// A node's stream as a test sees it.
pub(super) struct Wire {
    pub(super) stream: Stream,
    pub(super) inbox: mpsc::UnboundedReceiver<Result<ServerMessage, Status>>,
}

impl Wire {
    /// The messages the core sent on the stream since the last call.
    pub(super) fn sent(&mut self) -> Vec<server_message::Message> {
        let mut sent = Vec::new();
        while let Ok(Ok(ServerMessage {
            message: Some(message),
        })) = self.inbox.try_recv()
        {
            sent.push(message);
        }
        sent
    }
}

/// Registers `name` with four cores and eight GiB, at farm time `at` ms.
pub(super) fn register(rig: &Rig, name: &str, at: u64) -> Wire {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(4_000, 8 << 30), caps);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let welcome = ServerMessage {
        message: Some(server_message::Message::Welcome(worker::Welcome::default())),
    };
    let stream = dispatch::register(
        &rig.core,
        FarmTime::from_millis(at),
        &WorkerId::new(name),
        DaemonInstance::new(format!("daemon-{name}")),
        DaemonInstance::new(""),
        reported,
        Connection { outbound, welcome },
    )
    .expect("a host the core hears");
    Wire { stream, inbox }
}

/// A submission of the action `request(tag)` runs.
pub(super) fn submission(tag: u8) -> Submission {
    Submission {
        request: request(tag),
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    }
}

/// Submits `request(tag)` at farm time `at` ms.
pub(super) fn submit(rig: &Rig, tag: u8, at: u64) -> Result<Ticket, Status> {
    dispatch::submit(
        &rig.core,
        FarmTime::from_millis(at),
        &Shapes::default(),
        submission(tag),
    )
}

/// Submits `request(tag)` as an Execute call of the invocation `invocation`, which `recorder`
/// is told of.
pub(super) fn submit_recorded(
    rig: &Rig,
    recorder: &Recorder,
    request: Request,
    invocation: &str,
) -> Result<Ticket, Status> {
    let action = ActionKey::try_from(&request.key)
        .expect("an action's key")
        .action;
    let metadata = kbf_front::CallMetadata {
        invocation_id: kbf_front::InvocationId::parse(invocation),
        ..Default::default()
    };
    let call = kbf_front::ExecuteCall::new(
        recorder,
        &metadata,
        &Default::default(),
        "main",
        action,
        action,
    )
    .expect("a recorded call");
    let submission = Submission {
        request,
        call: Some(call),
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    };
    dispatch::submit(
        &rig.core,
        FarmTime::from_millis(0),
        &Shapes::default(),
        submission,
    )
}

/// Refuses every host.
pub(super) struct Deny;

impl Admission for Deny {
    fn allow_host(&self, _: &WorkerId, _: &js_server_core::Reported) -> Result<(), Denied> {
        Err(Denied::new("denied by the test"))
    }

    fn allow_beat(&self, _: &WorkerId, _: &js_server_core::Beat) -> Result<(), Denied> {
        Err(Denied::new("denied by the test"))
    }

    fn allow_result(&self, _: &WorkerId, _: LeaseId) -> Result<(), Denied> {
        Err(Denied::new("denied by the test"))
    }
}

/// `rig`, whose core hears no host.
pub(super) fn denying(rig: Rig) -> Rig {
    Rig {
        core: rig.core.with_admission(Deny),
        store: rig.store,
        backend: rig.backend,
    }
}

/// A request for an action that books `cores` cores and `gib` GiB.
pub(super) fn request_of(tag: u8, cores: u64, gib: u64) -> Request {
    Request {
        resources: Resources::new(cores * 1_000, gib << 30),
        ..request(tag)
    }
}

/// An action of the farm's `ci` level that books one core, for `Caller`.
pub(super) fn caller(tag: u8) -> Caller {
    Caller {
        key: ActionKey::try_from(&request(tag).key).expect("an action's key"),
        kind: LeaseKind::Action,
        do_not_cache: false,
        usage: Subject::of(Described::default()).0,
        platform: Platform::of(&kbf_caps::Request::default()),
    }
}

/// An adapter over a fresh store that no core drives: the hooks that are told no view can be
/// called on it.
pub(super) fn adapter() -> KbfAdapter {
    let store: Arc<dyn Store> = Flaky::new();
    KbfAdapter::new(7, Arc::new(Cache::memory()).into(), store, wall())
}

/// Admits caller `waiter` of `operation` to `adapter`, as a core that numbered it would, and
/// returns its stage.
pub(super) fn enrolled(
    adapter: &mut KbfAdapter,
    waiter: u64,
    operation: u64,
    tag: u8,
) -> watch::Receiver<Stage> {
    let admitted = Admitted {
        waiter: WaiterId(waiter),
        operation: OperationId(operation),
        joined: false,
        held: Held::Here,
    };
    let (sender, stage) = watch::channel(Stage::Queued);
    adapter.enrol(&admitted, caller(tag), None, sender);
    stage
}

/// The `Result` of an action that ran and exited with `code`, for `lease`.
pub(super) fn ran(lease: LeaseId, code: i32) -> worker::Result {
    result_with(
        lease,
        ActionResult {
            exit_code: code,
            ..Default::default()
        },
    )
}

/// A `Result` for `lease` that carries `action_result`.
pub(super) fn result_with(lease: LeaseId, action_result: ActionResult) -> worker::Result {
    worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: None,
        action_result: Some(action_result),
        action_digest: None,
        memory_kill: 0,
        lost: 0,
        job_outcome: None,
    }
}

/// The lease of every `Start` among `sent`, in the order they were sent.
pub(super) fn starts(sent: &[server_message::Message]) -> Vec<LeaseId> {
    sent.iter()
        .filter_map(|message| match message {
            server_message::Message::Start(start) => start
                .lease_id
                .as_ref()
                .map(|lease| LeaseId::new(lease.term, lease.seq)),
            _ => None,
        })
        .collect()
}

/// The lease of the first `Start` among `sent`, if one was sent.
pub(super) fn started(sent: &[server_message::Message]) -> Option<LeaseId> {
    starts(sent).first().copied()
}
