//! `ingest` and the timing a task claims: the manifest's `fetch_ms`, `run_ms` and `pack_ms`
//! become the result's execution metadata (the input fetch, the execution and the output
//! upload), laid back to back and ending at the time the frontend read the pack, as the claim of
//! the task. The clock of the packs is fixed (`now()`), so every timestamp is asserted as a value.
//! Every test says the behaviour it asserts.

use std::time::Duration;

use kbf_front::Ingested;
use kbf_pack::Timing;
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use prost_types::Timestamp;

use crate::untrusted_task::{Farm, now};

/// The timestamp `ms` before the frontend read the pack.
fn before(ms: u128) -> Option<Timestamp> {
    let (secs, millis) = (ms / 1_000, ms % 1_000);
    let back = Duration::new(secs as u64, millis as u32 * 1_000_000);
    Some(Timestamp::from(now() - back))
}

/// The metadata with nothing but the mark that says its phase times are a task's claim.
fn marked() -> ExecutedActionMetadata {
    let mut metadata = ExecutedActionMetadata::default();
    kbf_proto::task_claim::mark(&mut metadata);
    metadata
}

/// What the frontend rebuilt from a pack whose manifest claims `timing`, and the action cache's
/// copy of it.
async fn ingested(timing: Timing) -> (ActionResult, ActionResult) {
    let farm = Farm::new().await;
    let mut manifest = farm.manifest(0);
    manifest.timing = timing;
    farm.put(&farm.pack_of(&manifest)).await;
    let Ingested::Ran { result, cached } = farm.ingest().await else {
        panic!("the pack was not ingested");
    };
    assert!(cached);
    let stored = farm.cached().await.expect("a hit");
    (*result, stored)
}

/// Asserts that the three durations of the manifest become the three phases of the metadata, each
/// as long as the task said to the millisecond, one after the other, the last ending when the
/// pack was read; that nothing else of the metadata is set (the worker, the queue time and the
/// worker's own times are the server's to add); that the rest of the result is the honest one;
/// and that the action cache holds the same result. Catches a duration dropped, put in another
/// phase, scaled, or laid with a gap or an overlap.
#[tokio::test]
async fn rebuilt_carries_the_manifests_durations_as_a_claim() {
    let timing = Timing {
        fetch_ms: 120,
        run_ms: 3_400,
        pack_ms: 56,
    };
    let (got, stored) = ingested(timing).await;

    let want = ExecutedActionMetadata {
        input_fetch_start_timestamp: before(56 + 3_400 + 120),
        input_fetch_completed_timestamp: before(56 + 3_400),
        execution_start_timestamp: before(56 + 3_400),
        execution_completed_timestamp: before(56),
        output_upload_start_timestamp: before(56),
        output_upload_completed_timestamp: before(0),
        ..marked()
    };
    assert_eq!(got.execution_metadata, Some(want));
    let honest = Farm::new().await.result(0);
    assert_eq!(
        ActionResult {
            execution_metadata: None,
            ..got.clone()
        },
        honest
    );
    assert_eq!(stored, got);
}

/// Asserts that a manifest that times nothing (every duration 0, as the tool writes for a step it
/// did not time) leaves the metadata absent, not a message of instants at one time. Catches a
/// missing timing recorded as zero-width phases.
#[tokio::test]
async fn rebuilt_without_a_manifest_timing_leaves_metadata_empty() {
    let (got, stored) = ingested(Timing::default()).await;

    assert_eq!(got.execution_metadata, None);
    assert_eq!(got, Farm::new().await.result(0));
    assert_eq!(stored.execution_metadata, None);
}

/// Asserts that a phase the task did not time is absent while the others keep their values:
/// with only the run timed, the execution ends where the frontend read the pack and the input
/// fetch and the output upload have no timestamps at all. Catches an absent phase written as zero
/// width, and the others moved by it.
#[tokio::test]
async fn a_phase_the_task_did_not_time_is_absent_not_zero_width() {
    let timing = Timing {
        fetch_ms: 0,
        run_ms: 500,
        pack_ms: 0,
    };
    let (got, _) = ingested(timing).await;

    let want = ExecutedActionMetadata {
        execution_start_timestamp: before(500),
        execution_completed_timestamp: before(0),
        ..marked()
    };
    assert_eq!(got.execution_metadata, Some(want));
}

/// Asserts that the longest durations a manifest can hold (it refuses a number beyond an i64) are
/// laid without a panic and the phases are still one after the other. Catches a subtraction that
/// overflows on a task's claim, which is not trusted.
#[tokio::test]
async fn the_largest_claim_a_manifest_holds_does_not_overflow() {
    let most = i64::MAX as u64;
    let (one, two, three) = (u128::from(most), 2 * u128::from(most), 3 * u128::from(most));
    let timing = Timing {
        fetch_ms: most,
        run_ms: most,
        pack_ms: most,
    };
    let (got, _) = ingested(timing).await;

    let metadata = got.execution_metadata.expect("a claim");
    assert!(kbf_proto::task_claim::is_marked(&metadata));
    assert_eq!(metadata.output_upload_completed_timestamp, before(0));
    assert_eq!(metadata.output_upload_start_timestamp, before(one));
    assert_eq!(metadata.execution_completed_timestamp, before(one));
    assert_eq!(metadata.execution_start_timestamp, before(two));
    assert_eq!(metadata.input_fetch_completed_timestamp, before(two));
    assert_eq!(metadata.input_fetch_start_timestamp, before(three));
}
