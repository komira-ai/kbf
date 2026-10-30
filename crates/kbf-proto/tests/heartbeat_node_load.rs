//! The node load on the worker `Heartbeat` (field 6, `NodeLoad`): its numbers and wire types,
//! presence, and a peer on either side of the field. Also the peak window of the lease-end
//! usage (`CgroupUsage` field 17).

use kbf_proto::worker;
use prost::Message;

/// A heartbeat of a build before the field: sequence number, report hash, leases and the two
/// pack counts.
#[derive(Clone, PartialEq, Message)]
struct OldHeartbeat {
    #[prost(uint64, tag = "1")]
    seq: u64,
    #[prost(bytes = "vec", tag = "2")]
    report_hash: Vec<u8>,
    #[prost(message, repeated, tag = "3")]
    running: Vec<worker::LeaseId>,
    #[prost(message, optional, tag = "4")]
    pack_reads: Option<worker::PackReadCounts>,
    #[prost(message, optional, tag = "5")]
    pack_writes: Option<worker::PackWriteCounts>,
}

fn old() -> OldHeartbeat {
    OldHeartbeat {
        seq: 11,
        report_hash: vec![7; 32],
        running: vec![worker::LeaseId { term: 2, seq: 9 }],
        pack_reads: Some(worker::PackReadCounts {
            served: 3,
            ..worker::PackReadCounts::default()
        }),
        pack_writes: None,
    }
}

fn load() -> worker::NodeLoad {
    worker::NodeLoad {
        seq: 4,
        mono_usec: 5_000_000,
        cpu_usage_usec: 6_000_000,
        memory_working_set_bytes: 7_000,
        covered: vec![worker::LeaseId { term: 2, seq: 9 }],
        actions_cpu_limited: true,
        cpu_throttled_usec: Some(8),
        cpu_pressure_some_total_usec: Some(9),
        memory_active_file_bytes: 10,
        lease_clocks: Vec::new(),
        minutes: Vec::new(),
    }
}

fn new(node_load: Option<worker::NodeLoad>) -> worker::Heartbeat {
    let old = old();
    worker::Heartbeat {
        seq: old.seq,
        report_hash: old.report_hash,
        running: old.running,
        pack_reads: old.pack_reads,
        pack_writes: old.pack_writes,
        node_load,
        input_cache: None,
    }
}

/// Today's heartbeat bytes, with no reading, are the bytes a build before the field sent, and
/// decode to a heartbeat with none.
#[test]
fn a_heartbeat_without_a_reading_is_the_bytes_it_was_before() {
    assert_eq!(new(None).encode_to_vec(), old().encode_to_vec());
    let back = worker::Heartbeat::decode(&old().encode_to_vec()[..]).expect("decodes");
    assert_eq!(back, new(None));
}

/// A heartbeat with a reading decodes to what was sent, and the reading is field 6,
/// length-delimited, after the pack counts.
#[test]
fn a_heartbeat_with_a_reading_round_trips_on_field_six() {
    let beat = new(Some(load()));
    let bytes = beat.encode_to_vec();
    assert_eq!(
        worker::Heartbeat::decode(&bytes[..]).expect("decodes"),
        beat
    );
    let prefix = old().encode_to_vec();
    assert_eq!(&bytes[..prefix.len()], &prefix[..]);
    assert_eq!(bytes[prefix.len()], 6 << 3 | 2);
}

/// Each number and wire type of the reading, by hand: a number moved or retyped would be read
/// as another field by a peer of another build.
#[test]
fn each_field_is_on_its_number() {
    let mut want = vec![
        0x08, 4, // seq
        0x10, 0xC0, 0x96, 0xB1, 0x02, // mono_usec 5_000_000
        0x18, 0x80, 0x9B, 0xEE, 0x02, // cpu_usage_usec 6_000_000
        0x20, 0xD8, 0x36, // memory_working_set_bytes 7000
        0x2A, 4, 0x08, 2, 0x10, 9, // covered
        0x30, 1, // actions_cpu_limited
        0x38, 8, // cpu_throttled_usec
        0x40, 9, // cpu_pressure_some_total_usec
    ];
    want.extend_from_slice(&[0x58, 10]); // memory_active_file_bytes = 11
    assert_eq!(load().encode_to_vec(), want);
}

/// A telemetry counter that was not read is absent on the wire, and a 0 that was read is
/// present: the two are told apart.
#[test]
fn telemetry_absent_is_not_zero() {
    let none = worker::NodeLoad {
        cpu_throttled_usec: None,
        cpu_pressure_some_total_usec: None,
        ..load()
    };
    let zero = worker::NodeLoad {
        cpu_throttled_usec: Some(0),
        cpu_pressure_some_total_usec: Some(0),
        ..load()
    };
    let (none_len, zero_len) = (none.encode_to_vec().len(), zero.encode_to_vec().len());
    for reading in [none, zero] {
        let back = worker::NodeLoad::decode(&reading.encode_to_vec()[..]).expect("decodes");
        assert_eq!(back, reading);
    }
    // Two present zeros are two tags and two values.
    assert_eq!(zero_len - none_len, 4);
}

/// A server of a build before the field decodes a heartbeat that carries a reading, skipping
/// it, and reads the rest as sent.
#[test]
fn a_server_before_the_field_skips_it() {
    let beat = new(Some(load()));
    let seen = OldHeartbeat::decode(&beat.encode_to_vec()[..]).expect("an older peer decodes");
    assert_eq!(seen, old());
}

/// The lease-end usage says its peak window on field 17, and a peer before the field skips it.
#[test]
fn the_peak_window_is_cgroup_usage_field_seventeen() {
    #[derive(Clone, PartialEq, Message)]
    struct OldCgroup {
        #[prost(uint64, optional, tag = "1")]
        cpu_usage_micros: Option<u64>,
    }
    let usage = worker::CgroupUsage {
        cpu_usage_micros: Some(1),
        peak_window_millicores: Some(2500),
        ..worker::CgroupUsage::default()
    };
    let bytes = usage.encode_to_vec();
    assert_eq!(bytes, [0x08, 1, 0x88, 0x01, 0xC4, 0x13]);
    assert_eq!(
        worker::CgroupUsage::decode(&bytes[..]).expect("decodes"),
        usage
    );
    let old = OldCgroup::decode(&bytes[..]).expect("older peer decodes");
    assert_eq!(old.cpu_usage_micros, Some(1));
    let none = worker::CgroupUsage::decode(&[0x08, 1][..]).expect("decodes");
    assert_eq!(none.peak_window_millicores, None);
}
