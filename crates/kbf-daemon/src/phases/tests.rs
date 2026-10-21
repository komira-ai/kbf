use kbf_proto::reapi::ExecutedActionMetadata;
use kbf_proto::worker::{CgroupUsage, NumaNodeBytes};

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

/// A result whose metadata carries `usage`.
fn result_with(usage: &ResourceUsage) -> ActionResult {
    ActionResult {
        execution_metadata: Some(ExecutedActionMetadata {
            auxiliary_metadata: vec![usage.to_any()],
            ..ExecutedActionMetadata::default()
        }),
        ..ActionResult::default()
    }
}

/// Catches the cgroup's counters not reaching the line, in the order and under the
/// names the line promises (appended after `output_dirs`, so a parser of the old keys
/// reads the same), the parallelism taken over the wrong wall time (the driver's run
/// time, not the lease's) and the NUMA split printed in a shape a `key=value` parser
/// cannot read.
#[test]
fn the_cgroup_counters_follow_the_existing_keys() {
    let usage = ResourceUsage {
        wall_micros: 2_000_000,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(3_000_000),
            cpu_user_micros: Some(2_500_000),
            cpu_system_micros: Some(500_000),
            cpu_periods: Some(0),
            cpu_throttled_periods: Some(0),
            cpu_throttled_micros: Some(0),
            memory_peak_bytes: Some(1 << 30),
            numa: vec![
                NumaNodeBytes {
                    node: 0,
                    bytes: 600,
                },
                NumaNodeBytes {
                    node: 1,
                    bytes: 400,
                },
            ],
        }),
        ..ResourceUsage::default()
    };
    let result = result_with(&usage);
    // The lease's own time is longer than the run's: the line must not use it.
    let phases = LeasePhases::new(
        LeaseId::new(3, 4),
        "fake",
        Duration::from_secs(9),
        Some(&result),
    );
    assert_eq!(phases.parallelism_centi, Some(150));
    assert_eq!(phases.numa_spread_centi, Some(40));
    let line = phases.to_string();
    assert!(
        line.starts_with("lease=3.4 driver=fake ok=true lease_ms=9000 inputs_ms=- run_ms=- outputs_ms=- output_files=0 output_dirs=0 "),
        "{line}"
    );
    assert!(
        line.ends_with(
            " cpu_usage_us=3000000 cpu_user_us=2500000 cpu_system_us=500000 cpu_periods=0 \
             cpu_throttled=0 throttled_us=0 mem_peak=1073741824 parallelism=1.50 \
             numa_bytes=0:600,1:400 numa_spread=0.40"
        ),
        "{line}"
    );
}

/// Catches a result that carries no cgroup counters (another runtime, an old daemon's
/// usage, no usage at all, a lease without a result) printed with zeros: every
/// cgroup field must say `-`.
#[test]
fn a_result_without_cgroup_counters_prints_dashes() {
    let dashes = " cpu_usage_us=- cpu_user_us=- cpu_system_us=- cpu_periods=- cpu_throttled=- \
                  throttled_us=- mem_peak=- parallelism=- numa_bytes=- numa_spread=-";
    let rusage_only = result_with(&ResourceUsage {
        cpu_user_micros: 7,
        wall_micros: 9,
        ..ResourceUsage::default()
    });
    for result in [Some(&rusage_only), Some(&ActionResult::default()), None] {
        let phases = LeasePhases::new(LeaseId::new(1, 1), "fake", Duration::ZERO, result);
        assert_eq!(
            (phases.parallelism_centi, phases.numa_spread_centi),
            (None, None)
        );
        assert!(phases.to_string().ends_with(dashes), "{phases}");
    }
}
