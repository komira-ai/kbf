use kbf_proto::reapi::ExecutedActionMetadata;

use super::*;

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
    let phases = LeasePhases::new(lease, "fake", elapsed, Some(&result));
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
