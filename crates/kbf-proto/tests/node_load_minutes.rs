//! The lease clocks (`NodeLoad` field 9) and closed minutes (field 10) of the worker
//! heartbeat's node load: their numbers and wire types, a peer on either side of them, and the
//! size of a load with every list at its cap.

use kbf_proto::worker::{self, LeaseClock, LeaseId, LeaseMinute, NodeLoad};
use kbf_proto::{MAX_LOAD_LEASES, MAX_LOAD_MINUTES, MAX_MEMORY_BYTES, TAIL_MINUTE};
use prost::Message;

/// A load of a build before the two lists: fields 1 to 8 and 11.
#[derive(Clone, PartialEq, Message)]
struct OldNodeLoad {
    #[prost(uint64, tag = "1")]
    seq: u64,
    #[prost(uint64, tag = "2")]
    mono_usec: u64,
    #[prost(uint64, tag = "3")]
    cpu_usage_usec: u64,
    #[prost(uint64, tag = "4")]
    memory_working_set_bytes: u64,
    #[prost(message, repeated, tag = "5")]
    covered: Vec<LeaseId>,
    #[prost(bool, tag = "6")]
    actions_cpu_limited: bool,
    #[prost(uint64, optional, tag = "7")]
    cpu_throttled_usec: Option<u64>,
    #[prost(uint64, optional, tag = "8")]
    cpu_pressure_some_total_usec: Option<u64>,
    #[prost(uint64, tag = "11")]
    memory_active_file_bytes: u64,
}

fn lease(term: u64, seq: u64) -> LeaseId {
    LeaseId { term, seq }
}

/// The load of `tests/heartbeat_node_load.rs`, with no lists.
fn plain() -> NodeLoad {
    NodeLoad {
        seq: 4,
        mono_usec: 5_000_000,
        cpu_usage_usec: 6_000_000,
        memory_working_set_bytes: 7_000,
        covered: vec![lease(2, 9)],
        actions_cpu_limited: true,
        cpu_throttled_usec: Some(8),
        cpu_pressure_some_total_usec: Some(9),
        memory_active_file_bytes: 10,
        ..NodeLoad::default()
    }
}

fn clock() -> LeaseClock {
    LeaseClock {
        lease: Some(lease(2, 9)),
        started_mono_usec: 300,
    }
}

fn minute() -> LeaseMinute {
    LeaseMinute {
        lease: Some(lease(2, 9)),
        minute: 5,
        cpu_mean_millis: 400,
        cpu_peak_window_millis: 900,
        ws_max_bytes: 1_000_000,
    }
}

/// The lease `(2, 9)` as it is on the wire.
const LEASE: [u8; 6] = [0x0A, 4, 0x08, 2, 0x10, 9];

/// Each number and wire type of a clock, a minute and the two lists that carry them, by hand: a
/// number moved or a field retyped would be read as another field by a peer of another build.
#[test]
fn the_two_lists_are_on_fields_nine_and_ten() {
    let mut clock_bytes = LEASE.to_vec();
    clock_bytes.extend_from_slice(&[
        0x10, 0xAC, 0x02, // started_mono_usec 300
    ]);
    assert_eq!(clock().encode_to_vec(), clock_bytes);

    let mut minute_bytes = LEASE.to_vec();
    minute_bytes.extend_from_slice(&[
        0x10, 5, // minute
        0x18, 0x90, 0x03, // cpu_mean_millis 400
        0x20, 0x84, 0x07, // cpu_peak_window_millis 900
        0x28, 0xC0, 0x84, 0x3D, // ws_max_bytes 1_000_000
    ]);
    assert_eq!(minute().encode_to_vec(), minute_bytes);

    let load = NodeLoad {
        lease_clocks: vec![clock()],
        minutes: vec![minute()],
        ..NodeLoad::default()
    };
    let mut want = vec![0x4A, u8::try_from(clock_bytes.len()).expect("short")];
    want.extend_from_slice(&clock_bytes);
    want.extend_from_slice(&[0x52, u8::try_from(minute_bytes.len()).expect("short")]);
    want.extend_from_slice(&minute_bytes);
    assert_eq!(load.encode_to_vec(), want);
    assert_eq!(NodeLoad::decode(&want[..]).expect("decodes"), load);
}

/// A load with both lists empty is the bytes of a build before them: the numbers of fields 1 to
/// 8 and 11 did not move. (A list that is empty is not written at all, so this does not catch
/// "an empty list written"; it catches a renumbering of the old fields.)
#[test]
fn a_load_without_the_lists_is_the_bytes_it_was_before() {
    let new = plain();
    let old = OldNodeLoad {
        seq: new.seq,
        mono_usec: new.mono_usec,
        cpu_usage_usec: new.cpu_usage_usec,
        memory_working_set_bytes: new.memory_working_set_bytes,
        covered: new.covered.clone(),
        actions_cpu_limited: new.actions_cpu_limited,
        cpu_throttled_usec: new.cpu_throttled_usec,
        cpu_pressure_some_total_usec: new.cpu_pressure_some_total_usec,
        memory_active_file_bytes: new.memory_active_file_bytes,
    };
    assert_eq!(new.encode_to_vec(), old.encode_to_vec());
    assert_eq!(
        NodeLoad::decode(&old.encode_to_vec()[..]).expect("decodes"),
        new
    );
}

/// A server of a build before the lists decodes a load that carries both, skipping them, and
/// reads the rest as sent: the same load without the lists.
#[test]
fn a_peer_before_the_lists_skips_them() {
    let with = NodeLoad {
        lease_clocks: vec![clock()],
        minutes: vec![minute(), minute()],
        ..plain()
    };
    let seen = OldNodeLoad::decode(&with.encode_to_vec()[..]).expect("an older peer decodes");
    let without = OldNodeLoad::decode(&plain().encode_to_vec()[..]).expect("decodes");
    assert_eq!(seen, without);
    assert_eq!(seen.covered, vec![lease(2, 9)]);
}

/// The wire keeps the lists as sent: their order, and a record that repeats. A receiver's
/// merge by (lease, minute) is its own work.
#[test]
fn the_lists_keep_their_order_and_repeats() {
    let at = |term: u64, seq: u64, index: u32| LeaseMinute {
        lease: Some(lease(term, seq)),
        minute: index,
        ..minute()
    };
    let started = |term: u64, started_mono_usec: u64| LeaseClock {
        lease: Some(lease(term, 1)),
        started_mono_usec,
    };
    let load = NodeLoad {
        lease_clocks: vec![started(7, 30), started(3, 10), started(7, 30)],
        minutes: vec![at(2, 1, 3), at(1, 1, 1), at(2, 1, 3), at(2, 1, 2)],
        ..plain()
    };
    let back = NodeLoad::decode(&load.encode_to_vec()[..]).expect("decodes");
    assert_eq!(back, load);
}

/// A record sent without its lease decodes with none, and one sent with the lease `(0, 0)`
/// decodes with it: the two are told apart, and `NodeLoad::vet` drops the first.
#[test]
fn a_record_without_a_lease_decodes_without_one() {
    let bare = LeaseMinute {
        lease: None,
        ..minute()
    };
    let zero = LeaseMinute {
        lease: Some(lease(0, 0)),
        ..minute()
    };
    let load = NodeLoad {
        minutes: vec![bare, zero],
        ..NodeLoad::default()
    };
    let back = NodeLoad::decode(&load.encode_to_vec()[..]).expect("decodes");
    assert_eq!(back.minutes, vec![bare, zero]);
    assert_eq!(back.vet().minutes, vec![zero]);
}

/// A load whose every list is at its cap and whose every number has its most digits, in a
/// heartbeat with 96 running leases and both pack counts, is 9,001 and about 11.5 thousand
/// bytes: far under the 17 MiB a stream message may be, and it passes `vet` untouched.
#[test]
fn a_load_at_every_cap_is_9001_bytes_and_its_beat_under_12_kib() {
    // Ten-byte varints: distinct ids that still print as 20 digits.
    let id = |i: usize| lease(u64::MAX - i as u64, u64::MAX - i as u64);
    let load = NodeLoad {
        seq: u64::MAX,
        mono_usec: u64::MAX,
        cpu_usage_usec: u64::MAX,
        memory_working_set_bytes: MAX_MEMORY_BYTES,
        covered: (0..MAX_LOAD_LEASES).map(id).collect(),
        actions_cpu_limited: true,
        cpu_throttled_usec: Some(u64::MAX),
        cpu_pressure_some_total_usec: Some(u64::MAX),
        memory_active_file_bytes: MAX_MEMORY_BYTES,
        lease_clocks: (0..MAX_LOAD_LEASES)
            .map(|i| LeaseClock {
                lease: Some(id(i)),
                started_mono_usec: u64::MAX,
            })
            .collect(),
        minutes: (0..MAX_LOAD_MINUTES)
            .map(|i| LeaseMinute {
                lease: Some(id(i)),
                minute: TAIL_MINUTE,
                cpu_mean_millis: u32::MAX,
                cpu_peak_window_millis: u32::MAX,
                ws_max_bytes: MAX_MEMORY_BYTES,
            })
            .collect(),
    };
    // 96 clocks of 37 bytes, 96 covered of 24, 64 minutes of 48, and 73 of scalars.
    assert_eq!(load.encoded_len(), 96 * 37 + 96 * 24 + 64 * 48 + 73);
    assert_eq!(load.encoded_len(), 9_001);

    let vetted = load.vet();
    assert_eq!(vetted.covered, load.covered);
    assert_eq!(vetted.clocks, load.lease_clocks);
    assert_eq!(vetted.minutes, load.minutes);
    assert_eq!(vetted.drops.total(), 0);

    let beat = worker::Heartbeat {
        seq: u64::MAX,
        report_hash: vec![0xFF; 32],
        running: (0..MAX_LOAD_LEASES).map(id).collect(),
        pack_reads: Some(worker::PackReadCounts {
            served: u64::MAX,
            served_bytes: u64::MAX,
            elsewhere: u64::MAX,
            unasked: u64::MAX,
            lost: u64::MAX,
            damaged: u64::MAX,
        }),
        pack_writes: Some(worker::PackWriteCounts {
            packs: u64::MAX,
            packed: u64::MAX,
            packed_bytes: u64::MAX,
            fallback_off: u64::MAX,
            fallback_unfit: u64::MAX,
            fallback_unsupported: u64::MAX,
            fallback_refused: u64::MAX,
            fallback_store: u64::MAX,
            fallback_record: u64::MAX,
            fallback_index: u64::MAX,
            missing_indexed: u64::MAX,
            missing_unasked: u64::MAX,
        }),
        node_load: Some(load),
        input_cache: None,
    };
    let size = beat.encoded_len();
    assert!(size < 12 * 1024, "{size} bytes");
    // `kbf_front::MAX_MESSAGE_BYTES` is 17 MiB; kbf-proto cannot name it (kbf-front depends on
    // this crate). The beat is under a thousandth of it.
    assert!(size * 1000 < 17 * 1024 * 1024, "{size} bytes");
}
