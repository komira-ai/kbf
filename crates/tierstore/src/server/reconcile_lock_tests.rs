//! The reconciler of a storage node under `--lock-file`: it works only while its process holds
//! the lock file.
//!
//! The runtime's clock is paused and moved by the test, no test asserts a time, and a wait for
//! the reconciler is a count of steps of the paused clock with a yield between them, which a
//! broken reconciler fails and does not hang.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_testkit::dir::TestDir;
use tokio::sync::Notify;

use super::cli::{Gate, reconcile_gated};
use super::lock::try_take;
use crate::{
    Backend, FaultyBackend, MemoryBackend, Op, Proxy, ProxyConfig, Reconciler, ReconcilerConfig,
    TestClock,
};

const MEBIBYTE: u64 = 1 << 20;

/// The key of the reconciler's lease in the hot tier.
const LEASE: &str = "_tiering/lease";

/// The tiers of a storage node after a proxy wrote one object: the hot tier, the hot tier as the
/// reconciler sees it (counting its calls), and the memory of the cold tier.
struct Tiers {
    hot: Arc<MemoryBackend>,
    seen: Arc<FaultyBackend>,
    cold_mem: Arc<MemoryBackend>,
}

impl Tiers {
    /// The tiers, and a reconciler over them.
    async fn new() -> (Self, Reconciler) {
        let clock = Arc::new(TestClock::new());
        let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let seen = Arc::new(FaultyBackend::new(hot.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        let mut config = ProxyConfig::new("p1", MEBIBYTE, clock.clone());
        config.linger = Duration::from_millis(1);
        let opened = Proxy::open(hot.clone(), cold.clone(), config).await;
        let proxy = opened.unwrap();
        proxy.put("k", Bytes::from_static(b"v")).await.unwrap();
        let settings = ReconcilerConfig::new("p1", MEBIBYTE, clock);
        let reconciler = Reconciler::new(seen.clone(), cold, settings);
        let tiers = Self {
            hot,
            seen,
            cold_mem,
        };
        (tiers, reconciler)
    }

    /// The calls the reconciler has made to the hot tier that read or write.
    fn hot_calls(&self) -> u64 {
        let ops = [Op::Put, Op::PutIfMatch, Op::Get, Op::Head, Op::List];
        ops.into_iter().map(|op| self.seen.calls(op)).sum()
    }

    /// Moves the paused clock by `step` and yields, until the lease is (`present`) or is not in
    /// the hot tier, for at most a hundred thousand steps.
    async fn lease(&self, present: bool, step: Duration) {
        for _ in 0..100_000 {
            if self.hot.get(LEASE, None).await.is_ok() == present {
                return;
            }
            tokio::time::advance(step).await;
            tokio::task::yield_now().await;
        }
        let what = if present { "taken" } else { "given up" };
        panic!("the lease was not {what}");
    }
}

/// Asserts that a reconciler whose lock file another process holds makes no call to the hot tier
/// (no lease, no read of the journals) and copies nothing while it is held, through twelve
/// seconds of a retry every second and two intervals of the reconciler; and that when the lock is
/// free the next try takes it, the first pass takes the lease and copies the object, and a halt
/// gives the lease up. It would catch a reconciler of a second process that takes the lease of
/// the first while the first drains.
#[tokio::test(start_paused = true)]
async fn a_reconciler_under_a_held_lock_file_does_nothing_until_it_takes_the_lock() {
    let (tiers, reconciler) = Tiers::new().await;
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gate-held");
    let path = dir.path().join("instance.lock");
    let holder = try_take(&path).unwrap().unwrap();
    let gate = Gate::open(&path, Duration::from_secs(1)).unwrap();
    let halt = Arc::new(Notify::new());
    let every = Duration::from_secs(5);
    let working = reconcile_gated(reconciler, every, Arc::clone(&halt), gate);
    let watching = async {
        for _ in 0..12 {
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(tiers.hot_calls(), 0);
        assert!(tiers.cold_mem.get("k", None).await.is_err());
        drop(holder);
        tiers.lease(true, Duration::from_secs(1)).await;
        let mut copied = false;
        for _ in 0..100_000 {
            copied = tiers.cold_mem.get("k", None).await.is_ok();
            if copied {
                break;
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        assert!(copied, "the object was never copied to the cold tier");
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(tiers.hot.get(LEASE, None).await.is_err());
}

/// Asserts that a reconciler that holds its lock file works at once, and when the file is
/// replaced (removed and made again by another process, which holds the new one) it gives its
/// lease up at the next interval, takes none under the new file while the other holds it, and
/// works again when the other is gone. It would catch a reconciler that goes on under a lock on
/// a file nothing else looks at, beside a second reconciler that holds the new one.
#[tokio::test(start_paused = true)]
async fn a_reconciler_stops_when_its_lock_file_is_replaced_and_works_again_under_the_new_one() {
    let (tiers, reconciler) = Tiers::new().await;
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gate-replaced");
    let path = dir.path().join("instance.lock");
    let gate = Gate::open(&path, Duration::from_secs(1)).unwrap();
    let halt = Arc::new(Notify::new());
    let every = Duration::from_secs(5);
    let working = reconcile_gated(reconciler, every, Arc::clone(&halt), gate);
    let watching = async {
        tiers.lease(true, Duration::ZERO).await;
        std::fs::remove_file(&path).unwrap();
        let other = try_take(&path).unwrap().unwrap();
        tiers.lease(false, every).await;
        let calls = tiers.hot_calls();
        for _ in 0..12 {
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(tiers.hot_calls(), calls);
        assert!(tiers.hot.get(LEASE, None).await.is_err());
        drop(other);
        tiers.lease(true, Duration::from_secs(1)).await;
        halt.notify_one();
    };
    tokio::join!(working, watching);
    assert!(tiers.hot.get(LEASE, None).await.is_err());
}

/// Asserts that a lock file that cannot be used stops the start: a link is not followed, and a
/// directory that is not there is not made.
#[tokio::test]
async fn a_lock_file_that_cannot_be_used_is_an_error_at_the_start() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gate-refused");
    let target = dir.path().join("target");
    std::fs::write(&target, b"").unwrap();
    let link = dir.path().join("link.lock");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(Gate::open(&link, Duration::from_secs(1)).is_err());
    let missing = dir.path().join("none/instance.lock");
    assert!(Gate::open(&missing, Duration::from_secs(1)).is_err());
}
