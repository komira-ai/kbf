//! `--pack-store-start-wait-secs` and the pack store's check of the start ([`check`]): the
//! flag's default and values, the check made again while the store answers that it is busy,
//! ended at once by a refusal (exit 2) and given up at the bound (exit 1), each with the line
//! the frontend prints. The store is [`Busy`], a memory store that answers its writes as it is
//! told before it answers as itself, and time is [`Clock`], which a wait moves forward only
//! when it is waited on: a bound of 10 s is 10 s of waits, counted, never taken.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::start_retry::{DEFAULT_BOUND, RetryClock, StartRetry};
use kbf_front::{PackSettings, Packs};
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey,
    ObjectStore, ObjectStoreError, PageSize,
};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_state::ManualClock;

use super::check;
use crate::frontend::exit_of;
use crate::frontend::tests::parsed;
use crate::serve::ServeError;

/// The virtual clock: the time is the waits waited on so far, added up.
#[derive(Default)]
pub(in crate::frontend) struct Clock {
    now: Mutex<Duration>,
    waits: Mutex<Vec<Duration>>,
}

impl Clock {
    /// The waits waited on, in order.
    pub(in crate::frontend) fn waits(&self) -> Vec<Duration> {
        self.waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl RetryClock for Clock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            *self.now.lock().unwrap_or_else(PoisonError::into_inner) += wait;
            self.waits
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(wait);
        })
    }
}

/// A retry of `bound` seconds on a virtual clock, and the clock.
pub(in crate::frontend) fn on_clock(bound: u64) -> (StartRetry, Arc<Clock>) {
    let clock = Arc::new(Clock::default());
    let retry = StartRetry::with_clock(Duration::from_secs(bound), clock.clone());
    (retry, clock)
}

/// The store's answer `status` `code`.
pub(in crate::frontend) fn service(status: u16, code: &str, message: &str) -> ObjectStoreError {
    ObjectStoreError::Service {
        status,
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// What [`Busy`] answers its writes before it answers as itself.
#[derive(Default)]
struct Script {
    /// The answers to the next writes, one each.
    next: VecDeque<ObjectStoreError>,
    /// The answer to every write after them, for ever.
    always: Option<ObjectStoreError>,
    /// The writes asked, the refused ones included.
    writes: usize,
}

/// A memory store with create-only writes, whose writes (whole or streamed) are answered as
/// its script says before they are made.
pub(in crate::frontend) struct Busy {
    inner: MemoryStore,
    script: Mutex<Script>,
}

impl Busy {
    /// A store that answers its next writes `next`, one each, and every write after them
    /// `always` if it is given.
    pub(in crate::frontend) fn new(
        next: impl IntoIterator<Item = ObjectStoreError>,
        always: Option<ObjectStoreError>,
    ) -> Arc<Self> {
        let script = Script {
            next: next.into_iter().collect(),
            always,
            writes: 0,
        };
        Arc::new(Self {
            inner: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            script: Mutex::new(script),
        })
    }

    /// The writes asked so far.
    pub(in crate::frontend) fn writes(&self) -> usize {
        self.script().writes
    }

    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The scripted answer of the next write, if it has one.
    fn scripted(&self) -> Result<(), ObjectStoreError> {
        let mut script = self.script();
        script.writes += 1;
        match script.next.pop_front().or_else(|| script.always.clone()) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Packs over this store.
    pub(in crate::frontend) fn packs(self: &Arc<Self>) -> Packs {
        let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
        let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
        let packs = Packs::new(Arc::clone(self), index, keys, PackSettings::default());
        packs.expect("packs")
    }
}

impl ObjectStore for Busy {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.scripted()?;
        self.inner.put_new(key, body, retain_until).await
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        self.scripted()?;
        self.inner.put_stream(key, len, body).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_range(key, range).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        self.inner.may_hold(key).await
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        self.inner.len(key).await
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

fn secs(seconds: &[f64]) -> Vec<Duration> {
    seconds
        .iter()
        .map(|s| Duration::from_secs_f64(*s))
        .collect()
}

/// The exit code `error` ends the frontend with, as text.
fn exit(error: &ServeError) -> String {
    format!("{:?}", exit_of(error))
}

/// Asserts the flag: 120 s by default (the bound `kbf_front` names), 0 and a long bound taken,
/// and what is not a whole number of seconds refused. Catches: a default that differs from the
/// one documented, and a bound capped below what an operator asks for.
#[test]
fn the_flag_has_its_default_and_takes_any_seconds() {
    let bound = |more: &[&str]| parsed(more).map(|f| f.pack_store_start_wait);
    assert_eq!(bound(&[]).expect("parses"), DEFAULT_BOUND);
    assert_eq!(DEFAULT_BOUND, Duration::from_secs(120));
    assert_eq!(
        bound(&["--pack-store-start-wait-secs=0"]).expect("parses"),
        Duration::ZERO
    );
    assert_eq!(
        bound(&["--pack-store-start-wait-secs=86400"]).expect("parses"),
        Duration::from_secs(86400)
    );
    for bad in ["-1", "soon", "1.5", ""] {
        let flag = format!("--pack-store-start-wait-secs={bad}");
        assert!(bound(&[flag.as_str()]).is_err(), "{bad}");
    }
}

/// Asserts that a frontend with no packs checks nothing. Catches: a start that fails for a
/// store it does not have.
#[tokio::test]
async fn without_packs_nothing_is_checked() {
    let (retry, clock) = on_clock(120);

    check(None, &retry).await.expect("nothing to check");

    assert!(clock.waits().is_empty());
}

/// Asserts that the check the store answers 500 twice passes on its third attempt, after waits
/// of 0.5 and 1 s. Catches: a frontend whose start ends at the first answer of a store that is
/// restarting.
#[tokio::test]
async fn a_check_answered_500_twice_passes_on_the_third_attempt() {
    let store = Busy::new([0, 1].map(|_| service(500, "InternalError", "busy")), None);
    let (retry, clock) = on_clock(120);

    check(Some(&store.packs()), &retry).await.expect("passes");

    assert_eq!(clock.waits(), secs(&[0.5, 1.0]));
    assert_eq!(store.writes(), 4, "two refused, then the check's two");
}

/// Asserts that a 403 to the check ends the start at once, as before the bound: no wait, the
/// line names `--pack-store-endpoint` and the store's answer, and the frontend exits 2. Catches:
/// a refusal held for the whole bound, and one that exits 1, which a unit would restart for
/// ever.
#[tokio::test]
async fn a_403_to_the_check_ends_the_start_at_once_with_exit_two() {
    let store = Busy::new([service(403, "AccessDenied", "no")], None);
    let (retry, clock) = on_clock(120);

    let error = check(Some(&store.packs()), &retry)
        .await
        .expect_err("refused");

    assert!(matches!(error, ServeError::Packs(_)), "{error}");
    assert_eq!(
        error.to_string(),
        "--pack-store-endpoint: object store: store answered 403 AccessDenied: no"
    );
    assert!(clock.waits().is_empty(), "{:?}", clock.waits());
    assert_eq!(store.writes(), 1);
    assert_eq!(
        exit(&error),
        format!("{:?}", std::process::ExitCode::from(2))
    );
}

/// Asserts that a check the store answers 500 for ever is given up when the bound of 10 s has
/// passed: waits of 0.5, 1, 2 and 4 s, the wait to the bound, and a line that says the store's
/// last answer, the attempts, the 10 s and the flag, with exit 1. Catches: a start held for ever,
/// one given up before the bound, a line that does not say how long it tried, and an exit 2,
/// which a unit that prevents a restart on 2 would never start again.
#[tokio::test]
async fn a_check_answered_500_for_ever_is_given_up_at_the_bound_with_exit_one() {
    let store = Busy::new([], Some(service(500, "InternalError", "busy")));
    let (retry, clock) = on_clock(10);

    let error = check(Some(&store.packs()), &retry)
        .await
        .expect_err("given up");

    assert!(matches!(error, ServeError::StartWait { .. }), "{error}");
    assert_eq!(
        error.to_string(),
        "--pack-store-endpoint: object store: store answered 500 InternalError: busy (still \
         failing after 5 attempts over 10.0 s); gave up at --pack-store-start-wait-secs=10"
    );
    assert_eq!(clock.waits(), secs(&[0.5, 1.0, 2.0, 4.0, 2.5]));
    assert_eq!(
        exit(&error),
        format!("{:?}", std::process::ExitCode::from(1))
    );
}
