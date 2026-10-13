use kbf_proto::reapi::ExecutedActionMetadata;

use super::*;

/// Catches the most-in-flight count taken after a guard is given back (it would read
/// the level after the peak), an upload counted as stored before `done` (a failed
/// upload would count), an upload's guard that never lowers the level, and a call
/// outside any lease counted into one.
#[tokio::test]
async fn uploads_in_flight_and_stored_are_counted_per_lease() {
    let transfers = Arc::new(Transfers::default());
    // Outside any lease: counted nowhere, and harmless.
    read(5);
    upload().done(5);
    scoped(Arc::clone(&transfers), async {
        let a = upload();
        let b = upload();
        let failed = upload();
        drop(failed);
        a.done(10);
        let c = upload();
        b.done(20);
        c.done(30);
        read(7);
        read(8);
    })
    .await;
    assert_eq!(
        transfers.counts(),
        TransferCounts {
            blobs_read: 2,
            bytes_read: 15,
            blobs_uploaded: 3,
            bytes_uploaded: 60,
            most_uploads_in_flight: 3,
        }
    );
    assert_eq!(transfers.uploads_in_flight.load(Ordering::Relaxed), 0);
}

/// Catches a phase taken from the wrong pair of stamps, a phase whose end comes
/// before its start reported (as a huge number) instead of left out, and a result's
/// output files and directories not counted.
#[test]
fn each_phase_is_read_from_its_own_stamps() {
    let at = |ms: u64| {
        Some(prost_types::Timestamp::from(
            SystemTime::UNIX_EPOCH + Duration::from_millis(ms),
        ))
    };
    let result = ActionResult {
        output_files: vec![Default::default(); 2],
        output_directories: vec![Default::default(); 3],
        execution_metadata: Some(ExecutedActionMetadata {
            input_fetch_start_timestamp: at(1_000),
            input_fetch_completed_timestamp: at(1_010),
            execution_start_timestamp: at(1_100),
            execution_completed_timestamp: at(1_400),
            output_upload_start_timestamp: at(2_000),
            // Before its start: a clock stepped back.
            output_upload_completed_timestamp: at(1_999),
            ..ExecutedActionMetadata::default()
        }),
        ..ActionResult::default()
    };
    let lease = LeaseId::new(3, 4);
    let elapsed = Duration::from_millis(1_234);
    let phases = LeasePhases::new(
        lease,
        "fake",
        elapsed,
        Some(&result),
        TransferCounts::default(),
    );
    assert_eq!(
        (phases.inputs_ms, phases.run_ms, phases.outputs_ms),
        (Some(10), Some(300), None)
    );
    assert_eq!((phases.output_files, phases.output_dirs), (2, 3));
    assert!(phases.ok);
    assert_eq!(phases.lease_ms, 1_234);
    let line = phases.to_string();
    assert!(
        line.starts_with(
            "lease=3.4 driver=fake ok=true lease_ms=1234 inputs_ms=10 run_ms=300 outputs_ms=- "
        ),
        "{line}"
    );
}
