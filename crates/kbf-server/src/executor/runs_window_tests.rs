//! The window of a `Start` judged after its task is prepared: the prepare's own reads end at
//! the instant they end, and the submit goes out only before the window's end.

use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::JobState;
use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::{Stage, counted, step, submitted_specs, tell_start};
use crate::executor::Secret;
use crate::executor::test_support::{GIB, Harness, start};
use crate::executor::traits::{ActionFacts, Inputs, InputsError, ReadGrant};

/// An action read that ends at `until`, on the test's held clock: the prepare of a task
/// takes exactly as long as the test says. It says when the read has begun (`begun`), so the
/// test moves the clock only then.
struct ReadUntil {
    until: Instant,
    begun: Arc<Notify>,
}

#[tonic::async_trait]
impl Inputs for ReadUntil {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        self.begun.notify_one();
        tokio::time::sleep_until(self.until).await;
        Ok(ActionFacts {
            image: format!("registry.test/app@sha256:{}", "ab".repeat(32)),
            timeout: Some(Duration::from_secs(20)),
        })
    }

    async fn read_grant(
        &self,
        _: LeaseId,
        _: &Digest,
        _: Duration,
    ) -> Result<ReadGrant, InputsError> {
        Ok(ReadGrant {
            url: "https://grant.test:9400".to_owned(),
            token: Secret::new("token"),
            input_bytes: Some(3 * GIB),
        })
    }
}

/// A `Start` of lease 1.1 whose action read ends `before` the end of its window (counted
/// from the worker's first heartbeat, at its start, for the `Start`'s `valid_for_ms`), and
/// what came of it after one step: the harness and whether the task is running. The harness
/// holds the clock, so the test moves it: once the read has begun, to the read's end.
async fn prepared_before_the_end(before: Duration) -> (Harness, bool) {
    let mut harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let start = start(1, 1);
    let until = Instant::now() + Duration::from_millis(start.valid_for_ms) - before;
    let begun = Arc::new(Notify::new());
    harness.deps.inputs = Arc::new(ReadUntil {
        until,
        begun: Arc::clone(&begun),
    });
    tell_start(&messages, start);
    let read_ends = async {
        begun.notified().await;
        tokio::time::advance(until - Instant::now()).await;
    };
    tokio::join!(step(&harness, &mut worker), read_ends);
    let running = matches!(
        worker
            .tasks
            .get(&LeaseId::new(1, 1))
            .map(|task| &task.stage),
        Some(Stage::Running(_))
    );
    (harness, running)
}

/// Catches a window judged only by the timeout around the prepare: a prepare that ends at
/// the very instant the window ends finishes inside that timeout, and its job must still
/// not be submitted, as the `Start` is no longer valid. It is dropped and counted late, and
/// the service is asked for nothing. Control: a prepare that ends a millisecond before the
/// end is submitted and its job runs.
#[tokio::test(start_paused = true)]
async fn a_task_prepared_at_the_end_of_its_window_is_not_submitted() {
    let (harness, running) = prepared_before_the_end(Duration::ZERO).await;
    assert!(!running, "a job was submitted for a Start past its window");
    assert_eq!(submitted_specs(&harness).len(), 0);
    assert!(harness.fake.jobs().is_empty());
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 1);

    let (harness, running) = prepared_before_the_end(Duration::from_millis(1)).await;
    assert!(
        running,
        "control: a Start prepared inside its window is not running"
    );
    assert_eq!(submitted_specs(&harness).len(), 1);
    assert_eq!(harness.fake.jobs()[0].state(), JobState::Queued);
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 0);
}
