//! The small parts: the pace, the clocks, the store adapter, the report and the errors.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use kbf_front::action_tree::Sleeper;
use kbf_front::large_backfill::LargeStore;
use kbf_l2_backfill::Error;
use kbf_l2_backfill::pace::{Clock, Pace, SystemClock, TokioSleeper};
use kbf_l2_backfill::report::{Backfill, MAX_PROBLEMS};
use kbf_l2_backfill::store::Bucket;
use kbf_objstore::{BodyStream, Capabilities, MemoryStore, ObjectKey, ObjectStoreError};

use crate::support::{Naps, StepClock};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

/// Asserts the sequence of waits a pace asks for: none before the first, the spacing after.
#[test]
fn a_pace_waits_the_spacing_after_the_first() {
    let clock = Arc::new(StepClock::default());
    let naps = Naps {
        waits: Default::default(),
        clock: clock.clone(),
    };
    let mut pace = Pace::new(NonZeroU32::new(10));

    runtime().block_on(async {
        for _ in 0..3 {
            pace.wait(clock.as_ref(), &naps).await;
        }
    });

    assert_eq!(naps.waits(), vec![Duration::from_millis(100); 2]);
}

/// Asserts that the system clock does not go back, and that the tokio sleeper completes.
#[test]
fn the_system_clock_is_monotonic_and_the_sleeper_completes() {
    let clock = SystemClock::default();
    let first = clock.now();
    let second = clock.now();
    assert!(second >= first);

    runtime().block_on(async { TokioSleeper.sleep(Duration::ZERO).await });
}

fn key(text: &str) -> ObjectKey {
    ObjectKey::new(text).expect("a key")
}

/// Asserts that the store adapter writes create-only, reads lengths and ranges, and refuses a
/// read of no bytes.
#[test]
fn the_store_adapter_puts_measures_and_reads_ranges() {
    let bucket = Bucket::new(Arc::new(MemoryStore::new(Capabilities::default())));
    let object = key("cas/l/x");

    runtime().block_on(async {
        assert_eq!(bucket.len(&object).await.expect("len"), None);
        let body: BodyStream = Box::pin(stream::iter([Ok(Bytes::from_static(b"hello world"))]));
        bucket.put(&object, 11, body).await.expect("a put");
        assert_eq!(bucket.len(&object).await.expect("len"), Some(11));
        let piece = bucket.read_range(&object, 6, 5).await.expect("a range");
        assert_eq!(&piece[..], b"world");
        let none = bucket.read_range(&object, 0, 0).await;
        assert!(matches!(none, Err(ObjectStoreError::Unsupported(_))));
        let missing = bucket.read_range(&key("cas/l/y"), 0, 1).await;
        assert!(matches!(missing, Err(ObjectStoreError::NotFound(_))));
    });
}

/// Asserts that a report lists at most the first `MAX_PROBLEMS` problems.
#[test]
fn a_report_lists_the_first_hundred_problems() {
    let mut report = Backfill::default();

    for at in 0..MAX_PROBLEMS + 5 {
        report.problem(&format!("{at}"), "failed", "x".to_owned());
    }

    assert_eq!(report.problems.len(), MAX_PROBLEMS);
    assert_eq!(report.problems[0].tree, "0");
}

/// Asserts the exit status of each kind of end.
#[test]
fn a_refusal_is_status_2_and_a_failure_status_1() {
    assert_eq!(Error::refused("x").code(), 2);
    assert_eq!(Error::failed("x").code(), 1);
    let io = Error::from(std::io::Error::other("disk"));
    assert_eq!(io.code(), 1);
    assert_eq!(io.to_string(), "failed: disk");
}

/// Asserts that a file written whole lands under its name with nothing left beside it, and that
/// a rename that fails (the name is a directory) leaves no temporary file behind.
#[test]
fn a_file_is_written_whole_and_a_failed_rename_leaves_no_temporary_file() {
    let world = crate::support::World::new("units-atomic");
    let path = world.path("report.json");

    kbf_l2_backfill::files::write_atomic(&path, b"{}").expect("written");

    assert_eq!(std::fs::read(&path).expect("read"), b"{}");
    assert!(!world.path("report.json.tmp").exists());
    let taken = world.path("taken");
    std::fs::create_dir(&taken).expect("a directory");
    std::fs::write(taken.join("inside"), b"x").expect("a file in it");

    let failed = kbf_l2_backfill::files::write_atomic(&taken, b"{}");

    assert!(failed.is_err());
    assert!(
        !world.path("taken.tmp").exists(),
        "the temporary file is removed"
    );
}
