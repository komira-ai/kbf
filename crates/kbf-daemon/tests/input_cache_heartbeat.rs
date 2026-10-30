//! The input cache counts a daemon sends on its heartbeats (`Heartbeat.input_cache`), from a
//! runtime that has an input cache. The runtime here counts what the test tells it.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{InputCacheCounts, TreeCounts, daemon_message};
use kbf_types::LeaseId;
use std::sync::Arc;
use support::{Harness, PROMPT};

/// A runtime whose cache has filled `fills` entries of 10 bytes each so far.
#[derive(Default)]
struct Cached {
    fills: AtomicU64,
}

impl Runtime for Cached {
    fn driver(&self) -> &'static str {
        "cached"
    }

    fn serves(&self, kind: &str) -> bool {
        kind == "action"
    }

    async fn run(&self, _work: Work) -> Result<ActionResult, RuntimeError> {
        Ok(ActionResult::default())
    }

    fn input_cache(&self) -> Option<InputCacheCounts> {
        let fills = self.fills.load(Ordering::SeqCst);
        Some(InputCacheCounts {
            fills,
            fill_bytes: fills * 10,
            trees: Some(TreeCounts {
                builds: 1,
                ..TreeCounts::default()
            }),
            ..InputCacheCounts::default()
        })
    }

    async fn kill(&self, _lease_id: LeaseId) {}
}

/// Asserts that each heartbeat carries the counts the runtime gives as they stand when it is
/// sent, field for field, trees included: the first the counts of a cache that has done
/// nothing (present, all zero), a later one the counts after more fills.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_heartbeat_carries_the_input_cache_counts_as_they_stand() {
    let runtime = Arc::new(Cached::default());
    let mut h = Harness::with_runtime(
        "input-cache-heartbeat",
        Arc::clone(&runtime),
        Duration::from_secs(60),
    )
    .await;
    let mut peer = h.welcomed().await;
    let first = peer.heartbeat().await.input_cache.expect("counts");
    assert_eq!((first.fills, first.fill_bytes), (0, 0), "{first:?}");
    assert_eq!(first.trees.map(|t| t.builds), Some(1));

    runtime.fills.store(3, Ordering::SeqCst);
    let (_, later) = peer
        .expect(PROMPT, |m| match m {
            daemon_message::Message::Heartbeat(beat) => {
                beat.input_cache.filter(|counts| counts.fills == 3)
            }
            _ => None,
        })
        .await
        .expect("a heartbeat after the fills");
    assert_eq!(later.fill_bytes, 30);
}

/// Asserts that a runtime with no input cache (the default) sends none: absent, not zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_without_a_cache_sends_no_counts() {
    let runtime = Arc::new(kbf_daemon::FakeRuntime::new(Duration::ZERO));
    let mut h = Harness::with_runtime("input-cache-none", runtime, Duration::from_secs(60)).await;
    let mut peer = h.welcomed().await;
    assert_eq!(peer.heartbeat().await.input_cache, None);
    assert_eq!(peer.heartbeat().await.input_cache, None);
}
