//! `ResourceUsage` and `CgroupUsage` (`src/usage.rs`): finding the usage in a result,
//! the derived numbers and the fields of the log lines that print them.

use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::{CgroupUsage, FootprintUsage, NumaNodeBytes, ResourceUsage, UsageBasis};
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
        ..CgroupUsage::default()
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

/// Catches a macOS lease's measurements dropped from the line (every field `-` beside a
/// footprint block that read its processes), a cgroup's fields replaced by the
/// footprint's where both are present, CPU time summed from one half only, a usage that
/// measured nothing given numbers, and the Linux line changed.
#[test]
fn a_usage_prints_its_cgroup_else_its_footprint_else_dashes() {
    let dashes = kbf_proto::no_cgroup_fields();
    assert_eq!(ResourceUsage::default().log_fields(), dashes);
    // Only the wall time measured, as a whole-machine lease reports: all dashes.
    let wall_only = ResourceUsage {
        wall_micros: 9,
        ..ResourceUsage::default()
    };
    assert_eq!(wall_only.log_fields(), dashes);

    let mac = |footprint| ResourceUsage {
        wall_micros: 4_000_000,
        footprint: Some(footprint),
        basis: UsageBasis::MacFootprint.into(),
        ..ResourceUsage::default()
    };
    let read = mac(FootprintUsage {
        peak_footprint_bytes: Some(8192),
        cpu_user_micros: Some(3_000_000),
        cpu_system_micros: Some(1_000_000),
        wall_micros: Some(4_000_000),
    });
    assert_eq!(
        read.log_fields(),
        "cpu_usage_us=4000000 cpu_user_us=3000000 cpu_system_us=1000000 cpu_periods=- \
         cpu_throttled=- throttled_us=- mem_peak=8192 parallelism=1.00 numa_bytes=- numa_spread=-"
    );
    // A footprint block of a lease whose processes were never read.
    let unread = mac(FootprintUsage {
        wall_micros: Some(4_000_000),
        ..FootprintUsage::default()
    });
    assert_eq!(unread.log_fields(), dashes);
    // Half a CPU time is no CPU time.
    let half = mac(FootprintUsage {
        cpu_user_micros: Some(5),
        ..FootprintUsage::default()
    });
    assert!(
        half.log_fields()
            .starts_with("cpu_usage_us=- cpu_user_us=5 cpu_system_us=- ")
    );

    // A cgroup wins over a footprint, and prints as it always has.
    let both = ResourceUsage {
        cgroup: Some(cpu(10)),
        ..read
    };
    assert_eq!(both.log_fields(), cpu(10).log_fields(4_000_000));
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
        ..ResourceUsage::default()
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

/// Catches: a renumbered or dropped estimator field (a daemon and a server built from
/// different revisions would read the footprint, the basis, the pressure or the booking
/// from another field), a zero read as absent (`Some(0)` must survive), and a usage of
/// the nine older fields failing to decode or reading any of the new ones as set.
#[test]
fn the_footprint_basis_pressure_and_booking_have_their_field_numbers() {
    let footprint = ResourceUsage {
        footprint: Some(FootprintUsage {
            peak_footprint_bytes: Some(7),
            cpu_user_micros: Some(8),
            cpu_system_micros: Some(0),
            wall_micros: None,
        }),
        ..ResourceUsage::default()
    };
    // Field 6, length-delimited, holding fields 1, 2 and 3 as varints (the zero is on the
    // wire, the absent wall is not).
    assert_eq!(
        footprint.encode_to_vec(),
        [0x32, 6, 0x08, 7, 0x10, 8, 0x18, 0]
    );
    let basis = |b: UsageBasis| ResourceUsage {
        basis: b.into(),
        ..ResourceUsage::default()
    };
    // Field 7, varint.
    assert_eq!(basis(UsageBasis::MacFootprint).encode_to_vec(), [0x38, 2]);
    assert_eq!(basis(UsageBasis::Cgroup).encode_to_vec(), [0x38, 1]);
    assert!(basis(UsageBasis::Unspecified).encode_to_vec().is_empty());
    let load = ResourceUsage {
        node_cpu_pressure_centi: Some(0),
        booked_millicpus: Some(1500),
        ..ResourceUsage::default()
    };
    // Fields 8 and 9, varints; 1500 is 0xDC 0x0B.
    assert_eq!(load.encode_to_vec(), [0x40, 0, 0x48, 0xDC, 0x0B]);
    let back = ResourceUsage::decode(load.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(back.node_cpu_pressure_centi, Some(0), "zero is not absent");

    // A peer that predates the fields: nothing is set, and the basis reads unspecified.
    let old = ResourceUsage::decode([0x08, 1, 0x20, 4].as_slice()).expect("decodes");
    assert_eq!(old.footprint, None);
    assert_eq!(old.basis(), UsageBasis::Unspecified);
    assert_eq!(old.node_cpu_pressure_centi, None);
    assert_eq!(old.booked_millicpus, None);
}

/// Catches: a reader that fails on a field it does not know (an N-1 peer reading what
/// N sends), and an unknown basis number turning into a known one.
#[test]
fn an_unknown_field_is_skipped_and_an_unknown_basis_is_not_a_known_one() {
    let usage = ResourceUsage {
        wall_micros: 4,
        footprint: Some(FootprintUsage {
            peak_footprint_bytes: Some(1),
            ..FootprintUsage::default()
        }),
        basis: UsageBasis::MacFootprint.into(),
        ..ResourceUsage::default()
    };
    // Field 99, varint, then field 100, length-delimited: an N+1 peer's additions.
    let mut wire = usage.encode_to_vec();
    wire.extend_from_slice(&[0x98, 0x06, 5, 0xA2, 0x06, 2, 0xAA, 0xBB]);
    assert_eq!(
        ResourceUsage::decode(wire.as_slice()).expect("decodes"),
        usage
    );
    // The same inside the footprint block.
    let mut block = FootprintUsage::default().encode_to_vec();
    block.extend_from_slice(&[0x98, 0x06, 5]);
    assert_eq!(
        FootprintUsage::decode(block.as_slice()).expect("decodes"),
        FootprintUsage::default()
    );
    // Basis 9 is on the wire but is no variant: reading it gives unspecified.
    let future = ResourceUsage::decode([0x38, 9].as_slice()).expect("decodes");
    assert_eq!(future.basis(), UsageBasis::Unspecified);
}

/// Catches: a renumbered or dropped lease counter (events, swap peak, anon + swap peak,
/// pressure totals, CPUs), and a zero count read as absent (a lease held at its limit
/// zero times must read `Some(0)`, a kernel without the file `None`).
#[test]
fn the_lease_counters_have_their_field_numbers() {
    let all = CgroupUsage {
        memory_events_max: Some(1),
        memory_events_oom_kill: Some(0),
        memory_swap_peak_bytes: Some(3),
        memory_anon_swap_peak_bytes: Some(4),
        cpu_pressure_some_micros: Some(5),
        memory_pressure_some_micros: Some(6),
        memory_pressure_full_micros: Some(7),
        cpus_effective: Some(8),
        ..CgroupUsage::default()
    };
    assert_eq!(
        all.encode_to_vec(),
        [
            0x48, 1, 0x50, 0, 0x58, 3, 0x60, 4, 0x68, 5, 0x70, 6, 0x78, 7, 0x80, 1, 8
        ]
    );
    let back = CgroupUsage::decode(all.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(back, all);
    let old = CgroupUsage::decode([0x38, 9].as_slice()).expect("decodes");
    assert_eq!(old.memory_peak_bytes, Some(9));
    assert_eq!(old.memory_events_max, None);
    assert_eq!(old.cpus_effective, None);
}
