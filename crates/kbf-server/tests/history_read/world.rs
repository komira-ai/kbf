//! The stores and events the history read tests share: an object store in memory that
//! servers started one after another keep, and can be made to fail; a state store with
//! hooks; and the events of a build.

#![allow(dead_code, reason = "each test binary uses some of the helpers")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_auth::Policy;
use kbf_history::records::InvocationHeader;
use kbf_history::{HistoryRoot, IndexRoot, InvKey, Limits};
use kbf_invocation::{
    Event, InvocationId, NumaBytes, Outcome, ReapiTimes, RecordKey, Run, RunUsage,
};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_server::history::{Options, over};
use kbf_server::{EpochPolicy, StateStore};
use kbf_state::Store;
use prost::Message as _;
use tokio::sync::Notify;

use crate::pages::{
    Server, cached, digest, finished, granted, hit, later, listing, queued, started,
};

#[path = "../support/hooked.rs"]
pub mod hooked;
use hooked::Hooked;

/// A policy that lets a caller read instances whose names start with `a`.
pub const ONLY_A: &str = r#"{"authenticationPolicy": {"allow": {}},
    "invocations": {"getAuthorizer": {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["a"]}}}}"#;

/// An object store in memory the tests share between servers, whose writes can be made
/// to fail.
#[derive(Clone)]
pub struct Objects(Arc<ObjectsInner>);

pub struct ObjectsInner {
    memory: MemoryStore,
    fail_puts: AtomicBool,
    fail_gets: AtomicBool,
    /// Reads that start at the first byte (a whole chunk) fail.
    fail_chunk_reads: AtomicBool,
    puts: AtomicU64,
    /// Woken whenever a write is asked of the store.
    attempted: Notify,
    /// Writes asked of the store, whatever they answered.
    put_attempts: AtomicU64,
    gets: AtomicU64,
}

impl Objects {
    pub fn new() -> Self {
        Self(Arc::new(ObjectsInner {
            memory: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            fail_puts: AtomicBool::new(false),
            fail_gets: AtomicBool::new(false),
            fail_chunk_reads: AtomicBool::new(false),
            puts: AtomicU64::new(0),
            attempted: Notify::new(),
            put_attempts: AtomicU64::new(0),
            gets: AtomicU64::new(0),
        }))
    }

    pub fn fail_puts(&self, fail: bool) {
        self.0.fail_puts.store(fail, Ordering::SeqCst);
    }

    pub fn fail_gets(&self, fail: bool) {
        self.0.fail_gets.store(fail, Ordering::SeqCst);
    }

    pub fn fail_chunk_reads(&self, fail: bool) {
        self.0.fail_chunk_reads.store(fail, Ordering::SeqCst);
    }

    /// Writes that succeeded.
    pub fn puts(&self) -> u64 {
        self.0.puts.load(Ordering::SeqCst)
    }

    /// Writes asked of the store, whatever they answered.
    pub fn put_attempts(&self) -> u64 {
        self.0.put_attempts.load(Ordering::SeqCst)
    }

    /// Waits until the writer has asked the store for a write: it has cut a chunk and
    /// tried to put it, so what the server does next comes after the cut.
    pub async fn put_attempted(&self) {
        until(&self.0.attempted, || self.put_attempts() > 0).await;
    }

    /// Reads asked of the store, whatever they answered.
    pub fn gets(&self) -> u64 {
        self.0.gets.load(Ordering::SeqCst)
    }
}

impl ObjectStore for Objects {
    fn capabilities(&self) -> Capabilities {
        self.0.memory.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.put_attempts.fetch_add(1, Ordering::SeqCst);
        self.0.attempted.notify_waiters();
        if self.0.fail_puts.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Protocol("planted failure".into()));
        }
        let put = self.0.memory.put_new(key, body, retain_until).await;
        if put.is_ok() {
            self.0.puts.fetch_add(1, Ordering::SeqCst);
        }
        put
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.gets.fetch_add(1, Ordering::SeqCst);
        let chunk = range.offset() == 0 && self.0.fail_chunk_reads.load(Ordering::SeqCst);
        if chunk || self.0.fail_gets.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Protocol("planted cause".into()));
        }
        self.0.memory.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.memory.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.memory.list(prefix, after, max_keys).await
    }
}

/// Waits until `done` holds, re-checking it each time `changed` is woken. The wait is
/// registered before `done` is read, so a wake between the read and the wait is not lost.
pub async fn until(changed: &Notify, mut done: impl FnMut() -> bool) {
    loop {
        let woken = changed.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        if done() {
            return;
        }
        woken.await;
    }
}

/// Index reads to fail, so many that the server's own reads cannot use them up before
/// the one under test.
pub const STICKY: usize = 1_000_000;

pub fn root() -> HistoryRoot {
    HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root")
}

/// Options for a writer that steps often and cuts a chunk as soon as it has a record.
pub fn quick() -> Options {
    Options {
        step_every: Duration::from_millis(20),
        limits: Limits {
            chunk_age_ms: 0,
            ..Limits::default()
        },
        ..Options::default()
    }
}

/// The stores servers are started over, one after another.
pub struct World {
    pub objects: Objects,
    pub state: Arc<Hooked>,
}

impl World {
    pub fn new() -> Self {
        Self {
            objects: Objects::new(),
            state: Hooked::new(),
        }
    }

    /// A server over the stores, running `policy`, with history on.
    pub fn start(&self, policy: &str) -> Server {
        let store: Arc<dyn Store> = self.state.clone();
        let history = over(
            self.objects.clone(),
            root(),
            Arc::clone(&store),
            IndexRoot::default(),
            quick(),
        );
        let state = StateStore {
            store,
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: Some(history),
        };
        Server::start_over(
            Policy::from_json(policy).expect("a policy"),
            kbf_front::DEFAULT_INVOCATION_MEMORY,
            Some(state),
        )
    }

    /// The index rows under `prefix`, keys only.
    pub fn rows(&self, prefix: &str) -> Vec<String> {
        self.state
            .fake
            .scan_prefix(prefix)
            .expect("a scan")
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    }

    /// Waits until the writer has put the header of `inv` with `records` runs and hits
    /// counted: its chunks and their rows are in the stores.
    pub async fn flushed(&self, inv: &str, records: u64) {
        let prefix = IndexRoot::default().header_rows(&InvKey::from_record_id(inv));
        until(&self.state.written, || {
            // Each writer keeps a header row of its own: the records are the sum.
            let counted: u64 = (self.state.fake.scan_prefix(&prefix).expect("a read").iter())
                .filter_map(|(_, row)| InvocationHeader::decode(row.value.as_slice()).ok())
                .map(|h| h.runs + h.hits)
                .sum();
            counted >= records
        })
        .await;
    }

    /// The header row of `inv`.
    pub fn header(&self, inv: &str) -> InvocationHeader {
        let (_, row) = header_row(&self.state, inv).expect("a header");
        InvocationHeader::decode(row.value.as_slice()).expect("a header")
    }

    /// Replaces `inv`'s header row with `header`.
    pub fn set_header(&self, inv: &str, header: &InvocationHeader) {
        set_header(&self.state, inv, header);
    }
}

/// The header row a writer keeps of `inv` (the first, if several writers wrote one).
fn header_row(state: &Hooked, inv: &str) -> Option<(String, kbf_state::Versioned)> {
    let prefix = IndexRoot::default().header_rows(&InvKey::from_record_id(inv));
    state
        .fake
        .scan_prefix(&prefix)
        .expect("a read")
        .into_iter()
        .next()
}

/// Replaces `inv`'s header row (the writer's own, which is the one a reader finds first;
/// the row of a build before headers were per writer if none exists).
pub fn set_header(state: &Hooked, inv: &str, header: &InvocationHeader) {
    let key = header_row(state, inv).map_or_else(
        || IndexRoot::default().invid(&InvKey::from_record_id(inv)),
        |(key, _)| key,
    );
    let put = kbf_state::Txn::new().put(key, header.encode_to_vec(), kbf_state::Cond::Any);
    state.fake.txn(put).expect("a write").expect("no conflict");
}

pub fn key(id: &str) -> RecordKey {
    RecordKey::owner(InvocationId::new(id))
}

/// What a lease measured, as a daemon reports it: every REAPI time that is kept, and the
/// whole cgroup block.
pub fn measured(t: u64) -> Run {
    Run {
        times: ReapiTimes {
            queued: Some(later(t)),
            worker_start: Some(later(t + 15)),
            input_fetch_start: Some(later(t + 16)),
            input_fetch_completed: Some(later(t + 20)),
            execution_start: Some(later(t + 21)),
            execution_completed: Some(later(t + 40)),
            output_upload_start: Some(later(t + 41)),
            output_upload_completed: Some(later(t + 45)),
            worker_completed: Some(later(t + 46)),
        },
        usage: Some(RunUsage {
            wall_micros: 13,
            cpu_usage_micros: Some(26),
            cpu_user_micros: Some(20),
            cpu_system_micros: Some(6),
            cpu_periods: Some(4),
            cpu_throttled_periods: Some(1),
            cpu_throttled_micros: Some(2),
            memory_peak_bytes: Some(777),
            numa: vec![
                NumaBytes { node: 0, bytes: 6 },
                NumaBytes { node: 1, bytes: 4 },
            ],
            ..RunUsage::default()
        }),
        booked_memory_bytes: None,
        booked_millicpus: None,
        cap_bytes: None,
    }
}

/// The events of action `n` of invocation `id` on `instance`: queued as operation
/// `op`, run on a node and finished with `exit_code`.
pub fn ran(id: &str, instance: &str, n: u8, op: u64, exit_code: i32) -> Vec<Event> {
    ran_action(id, instance, n, n, op, exit_code)
}

/// [`ran`] for the action whose digest is that of `action`, at the time of `n`: the same
/// action run again, later.
pub fn ran_action(
    id: &str,
    instance: &str,
    action: u8,
    n: u8,
    op: u64,
    exit_code: i32,
) -> Vec<Event> {
    let t = u64::from(n) * 1_000;
    let l = listing(
        digest(100 + u64::from(n)),
        exit_code,
        &[digest(120 + u64::from(n))],
        (Some(digest(140 + u64::from(n))), None),
    );
    vec![
        queued(id, instance, later(t), digest(u64::from(action)), op),
        granted(op, 1, "node-a", later(t + 10)),
        started(op, 1, later(t + 12)),
        Event::Ran {
            operation: kbf_types::OperationId(op),
            lease: crate::pages::lease(1),
            run: Box::new(measured(t)),
        },
        finished(op, 1, Outcome::Ok { exit_code }, Some(l), later(t + 50)),
    ]
}

/// A `GetActionResult` hit of action `n` of `id`.
pub fn hit_of(id: &str, n: u8) -> Event {
    let l = listing(digest(200), 0, &[], (None, None));
    hit(id, later(u64::from(n) * 1_000), digest(u64::from(n)), l)
}

/// An Execute of action `n` of `id` answered from the action cache.
pub fn cached_of(id: &str, n: u8) -> Event {
    let l = listing(digest(201), 0, &[], (None, None));
    cached(id, later(u64::from(n) * 1_000), digest(u64::from(n)), l)
}
