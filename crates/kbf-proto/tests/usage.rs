//! `ResourceUsage` and `CgroupUsage` (`src/usage.rs`): finding the usage in a result,
//! the derived numbers and the fields of the log lines that print them.

use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::{CgroupUsage, NumaNodeBytes, ResourceUsage};
use prost::Message;
use prost_types::Any;

fn node(node: u32, bytes: u64) -> NumaNodeBytes {
    NumaNodeBytes { node, bytes }
}

fn cpu(usage_micros: u64) -> CgroupUsage {
    CgroupUsage {
        cpu_usage_micros: Some(usage_micros),
        ..CgroupUsage::default()
    }
}

/// Catches parallelism computed from the wrong operands (wall over CPU), not rounded to
/// the hundredth, a zero wall time dividing, and an absent CPU time read as zero CPUs.
#[test]
fn parallelism_is_cpu_time_over_wall_time_in_hundredths() {
    assert_eq!(cpu(3_000_000).parallelism_centi(2_000_000), Some(150));
    assert_eq!(cpu(500_000).parallelism_centi(2_000_000), Some(25));
    // 1/3 of a CPU, rounded: 0.33; 2/3: 0.67.
    assert_eq!(cpu(1_000_000).parallelism_centi(3_000_000), Some(33));
    assert_eq!(cpu(2_000_000).parallelism_centi(3_000_000), Some(67));
    // More than one CPU for a short run does not overflow.
    assert_eq!(cpu(u64::MAX / 2).parallelism_centi(1), None);
    assert_eq!(cpu(0).parallelism_centi(5), Some(0));
    assert_eq!(cpu(1).parallelism_centi(0), None);
    assert_eq!(CgroupUsage::default().parallelism_centi(1_000), None);
}

/// Catches the spread taken as the share on the biggest node (it is the share not on
/// it), a one-node host not at 0, and an empty or all-zero split given a number.
#[test]
fn numa_spread_is_the_share_off_the_biggest_node() {
    let spread = |nodes: Vec<NumaNodeBytes>| {
        CgroupUsage {
            numa: nodes,
            ..CgroupUsage::default()
        }
        .numa_spread_centi()
    };
    assert_eq!(spread(vec![node(0, 600), node(1, 400)]), Some(40));
    assert_eq!(spread(vec![node(0, 1000)]), Some(0));
    assert_eq!(spread(vec![node(0, 500), node(1, 500)]), Some(50));
    // The biggest need not be first; four nodes, a quarter each: three quarters off.
    assert_eq!(spread(vec![node(0, 1), node(1, 3)]), Some(25));
    assert_eq!(
        spread(vec![node(0, 5), node(1, 5), node(2, 5), node(3, 5)]),
        Some(75)
    );
    assert_eq!(spread(vec![]), None);
    assert_eq!(spread(vec![node(0, 0), node(1, 0)]), None);
}

/// Catches the line's fields reordered or renamed (a parser keys on them), an absent
/// field printed as 0, and the NUMA pairs not in `node:bytes,...` form.
#[test]
fn the_log_fields_are_fixed_and_dashes_stand_for_absent() {
    let full = CgroupUsage {
        cpu_usage_micros: Some(10),
        cpu_user_micros: Some(6),
        cpu_system_micros: Some(4),
        cpu_periods: Some(100),
        cpu_throttled_periods: Some(3),
        cpu_throttled_micros: Some(777),
        memory_peak_bytes: Some(4096),
        numa: vec![node(0, 30), node(2, 10)],
    };
    assert_eq!(
        full.log_fields(5),
        "cpu_usage_us=10 cpu_user_us=6 cpu_system_us=4 cpu_periods=100 cpu_throttled=3 \
         throttled_us=777 mem_peak=4096 parallelism=2.00 numa_bytes=0:30,2:10 numa_spread=0.25"
    );
    let dashes = "cpu_usage_us=- cpu_user_us=- cpu_system_us=- cpu_periods=- cpu_throttled=- \
                  throttled_us=- mem_peak=- parallelism=- numa_bytes=- numa_spread=-";
    assert_eq!(CgroupUsage::default().log_fields(5), dashes);
    assert_eq!(kbf_proto::no_cgroup_fields(), dashes);
    // Present zeros are zeros: a cgroup with no quota has throttled nothing.
    let zero = CgroupUsage {
        cpu_throttled_periods: Some(0),
        ..CgroupUsage::default()
    };
    assert!(zero.log_fields(5).contains(" cpu_throttled=0 "));
}

/// Catches usage not found by its type URL (and another entry's bytes decoded as
/// usage), and the new `cgroup` field lost on the wire: a ResourceUsage of the four
/// original fields (as an older daemon sends it) must decode with no cgroup.
#[test]
fn usage_is_found_in_a_result_and_the_old_four_fields_still_decode() {
    let usage = ResourceUsage {
        cpu_user_micros: 1,
        cpu_system_micros: 2,
        peak_memory_bytes: 3,
        wall_micros: 4,
        cgroup: Some(CgroupUsage {
            cpu_throttled_periods: Some(0),
            numa: vec![node(1, 9)],
            ..cpu(5)
        }),
    };
    let any = |type_url: &str, value: Vec<u8>| Any {
        type_url: type_url.to_owned(),
        value,
    };
    let result = |aux: Vec<Any>| ActionResult {
        execution_metadata: Some(ExecutedActionMetadata {
            auxiliary_metadata: aux,
            ..ExecutedActionMetadata::default()
        }),
        ..ActionResult::default()
    };
    assert_eq!(ResourceUsage::in_result(&ActionResult::default()), None);
    let other = any("type.googleapis.com/other", usage.encode_to_vec());
    assert_eq!(ResourceUsage::in_result(&result(vec![other.clone()])), None);
    let mine = usage.to_any();
    assert_eq!(mine.type_url, ResourceUsage::TYPE_URL);
    assert_eq!(
        ResourceUsage::in_result(&result(vec![other, mine])),
        Some(usage)
    );

    // The four fields as they were before `cgroup` (field numbers 1 to 4, varints).
    let old = [0x08, 1, 0x10, 2, 0x18, 3, 0x20, 4];
    let decoded = ResourceUsage::decode(old.as_slice()).expect("decodes");
    assert_eq!(decoded.wall_micros, 4);
    assert_eq!(decoded.cgroup, None);
    // A present cgroup with every field absent is not the same as no cgroup.
    let empty = ResourceUsage {
        cgroup: Some(CgroupUsage::default()),
        ..ResourceUsage::default()
    };
    let back = ResourceUsage::decode(empty.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(back.cgroup, Some(CgroupUsage::default()));
}
