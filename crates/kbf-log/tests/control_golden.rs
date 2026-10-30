//! The format-1 bytes of every control command and of every farm input.
//!
//! Catches an encoding that changes under a committed log: a field tag swapped or
//! renumbered, a field's type changed, a command or input moved to another `oneof`
//! number, an enum value renumbered. The bytes below were written out by hand from
//! `log.proto`, each with its reading beside it, and then compared with the encoder;
//! a log an older binary wrote must decode to the same value on a newer one.

mod control_support;

use std::num::{NonZeroU32, NonZeroU64};

use control_support::{
    CAPS_HEX, RESOURCES_HEX, batch, caps, d, dec, digest_hex, enc, lease, resources, w,
};
use kbf_caps::{Arch, Consumable, CpuCaps, NodeCaps};
use kbf_log::{ActionEntry, Checkpoint, FarmInput, LogCommand, MachineVersion, Report, Takeover};
use kbf_meta::{ActionRecord, Closure};
use kbf_sched::{DaemonInstance, Request};
use kbf_types::sched::usage::LeaseCost;
use kbf_types::{
    ActionKey, ControlRecord, Failure, FarmTime, LeaseGrant, LeaseKind, LostWhy, OperationId,
    Outcome, Qos, RefusalRecord, Resources, ResultRecord,
};

/// A batch (`1a`, `LogCommand` field 3) of `len` bytes: farm time 1 (`08 01`), wall
/// time 2 ns (`10 02`), one input (`1a`) of `input_len` bytes.
fn batch_hex(len: u8, input_len: u8, input: &str) -> String {
    format!("1a{len:02x}080110021a{input_len:02x}{input}")
}

/// A request for instance `i`, action `d(3)`, at `qos`, of `kind`, booking CPU 1 and
/// memory 2, needing `arch=arm64`, hermetic and not cached.
fn request(qos: Qos, kind: LeaseKind) -> Request {
    Request {
        key: ActionKey {
            instance: "i".to_owned(),
            action: d(3),
        }
        .into(),
        qos,
        kind,
        resources: Resources::new(1, 2),
        needs: kbf_caps::Request::parse([("arch", "arm64")]).expect("request"),
        hermetic: true,
        do_not_cache: true,
        whole_worker: false,
    }
}

/// `request(qos, kind)` inside a batch, where `qos_hex` is the `Qos` message's bytes
/// (2) and `kind` the `LeaseKind` value.
fn request_hex(qos_hex: &str, kind: u8) -> String {
    let request = [
        // key (0a, 45) { instance "i", action (12, 38) }
        format!("0a2b0a01691226{}", digest_hex(3)),
        // qos (12, 2), kind (18), resources (22, 4) { cpu 1, memory 2 }
        format!("1202{qos_hex}18{kind:02x}220408011002"),
        // needs (2a, 13) { key "arch", value "arm64" }, hermetic (30), do_not_cache (38)
        "2a0d0a046172636812056172 6d3634".replace(' ', ""),
        "30013801".to_owned(),
    ]
    .concat();
    // Request: 45 + 4 + 2 + 6 + 15 + 4 = 76 (0x4c); Submit (0a): 78; input (22): 80.
    batch_hex(0x56, 0x50, &format!("224e0a4c{request}"))
}

/// `request(Qos::Batch, LeaseKind::Action)` that also needs an iOS device of class `c`.
fn device_request() -> Request {
    let mut r = request(Qos::Batch, LeaseKind::Action);
    r.needs = kbf_caps::Request::parse([
        ("arch", "arm64"),
        ("ios.device", "1"),
        ("ios.device.class", "c"),
    ])
    .expect("device request");
    r
}

/// `device_request()` inside a batch. Its needs are written in `properties()` order:
/// `arch`, then `ios.device`, then each device attribute.
fn device_request_hex() -> String {
    let request = [
        // key (0a, 45) { instance "i", action (12, 38) }
        format!("0a2b0a01691226{}", digest_hex(3)),
        // qos (12, 2) { builtin (08) 3 }, kind (18) 1, resources (22, 4) { cpu 1,
        // memory 2 }
        "120208031801220408011002".to_owned(),
        // needs (2a, 13) { key "arch", value "arm64" }
        "2a0d0a046172636812056172 6d3634".replace(' ', ""),
        // needs (2a, 15) { key (0a, 10) "ios.device", value (12) "1" }
        "2a0f0a0a696f732e646576696365120131".to_owned(),
        // needs (2a, 21) { key (0a, 16) "ios.device.class", value (12) "c" }
        "2a150a10696f732e6465766963652e636c617373120163".to_owned(),
        // hermetic (30), do_not_cache (38)
        "30013801".to_owned(),
    ]
    .concat();
    // Request: 45 + 4 + 2 + 6 + 15 + 17 + 23 + 4 = 116 (0x74); Submit (0a): 118; input
    // (22): 120; batch: 126.
    batch_hex(0x7e, 0x78, &format!("22760a74{request}"))
}

/// A failed report: farm time 1, operation 3, lease 1.2, failure value `failure`.
fn failed_report_hex(failure: u8) -> String {
    // Report (22, 14): farm time (08), operation (10), lease (1a, 4), outcome (22, 2)
    // { failed (10) }.
    format!("220e080110031a04080110022202 10{failure:02x}").replace(' ', "")
}

fn failed_report(failure: Failure) -> LogCommand {
    LogCommand::Report(Report {
        now: FarmTime::from_millis(1),
        operation: OperationId(3),
        lease: lease(1, 2),
        outcome: Outcome::Failed(failure),
        action_entry: None,
    })
}

fn golden() -> Vec<(&'static str, LogCommand, String)> {
    vec![
        // input (3a): tick, empty
        ("Tick", batch(FarmInput::Tick), batch_hex(8, 2, "3a00")),
        // input (42): cordon { worker "w" }
        (
            "Cordon",
            batch(FarmInput::Cordon(w("w"))),
            batch_hex(11, 5, "42030a0177"),
        ),
        (
            "Uncordon",
            batch(FarmInput::Uncordon(w("w"))),
            batch_hex(11, 5, "52030a0177"),
        ),
        (
            "Disconnected",
            batch(FarmInput::Disconnected(w("w"))),
            batch_hex(11, 5, "5a030a0177"),
        ),
        (
            // input (4a): drain { worker "w", deadline 5 }
            "Drain",
            batch(FarmInput::Drain {
                worker: w("w"),
                deadline: FarmTime::from_millis(5),
            }),
            batch_hex(13, 7, "4a050a01771005"),
        ),
        (
            // input (32): started { operation 3, lease (12) { term 1, seq 2 } }
            "Started",
            batch(FarmInput::Started {
                operation: OperationId(3),
                lease: lease(1, 2),
            }),
            batch_hex(16, 10, "3208080312040801 1002".replace(' ', "").as_str()),
        ),
        (
            // input (1a): heartbeat { worker "w", running (12) 1.2, running (12) 1.3 }
            "Heartbeat",
            batch(FarmInput::Heartbeat {
                worker: w("w"),
                running: vec![lease(1, 2), lease(1, 3)],
            }),
            batch_hex(
                23,
                17,
                "1a0f0a017712040801100212040801 1003"
                    .replace(' ', "")
                    .as_str(),
            ),
        ),
        (
            // input (2a): committed { lease (0a) { lease (0a) 1.2, operation 3,
            // worker (1a) "w" } }
            "Committed lease",
            batch(FarmInput::Committed(ControlRecord::Lease(LeaseGrant {
                lease: lease(1, 2),
                operation: OperationId(3),
                worker: w("w"),
                cost: LeaseCost::default(),
            }))),
            batch_hex(21, 15, "2a0d0a0b0a040801100210031a0177"),
        ),
        (
            // The same grant with a cost: input (2a) { lease (0a) { lease, operation,
            // worker as above, then cost (22) { cpu_millis (08) 4, memory_bytes (10) 5,
            // limit_bytes (18) 6, cold (20) true, retry (28) true, key_hash (30) 7 } } }.
            // Field 4 of the grant is new: a reader without it skips the 14 bytes it adds.
            "Committed lease with a cost",
            batch(FarmInput::Committed(ControlRecord::Lease(LeaseGrant {
                lease: lease(1, 2),
                operation: OperationId(3),
                worker: w("w"),
                cost: LeaseCost {
                    cpu_millis: 4,
                    memory_bytes: 5,
                    limit_bytes: 6,
                    cold: true,
                    retry: true,
                    key_hash: 7,
                },
            }))),
            batch_hex(
                35,
                29,
                "2a1b0a190a040801100210031a0177220c080410051806200128013007",
            ),
        ),
        (
            // input (2a): committed { result (12) { lease 1.2, operation 3, outcome
            // (1a) { failed (10) 1 } } }
            "Committed result",
            batch(FarmInput::Committed(ControlRecord::Result(ResultRecord {
                lease: lease(1, 2),
                operation: OperationId(3),
                outcome: Outcome::Failed(Failure::Timeout),
            }))),
            batch_hex(
                22,
                16,
                "2a0e120c0a0408011002 10031a021001"
                    .replace(' ', "")
                    .as_str(),
            ),
        ),
        (
            // input (2a): committed { refusal (1a) { operation 3, reason (12) "r" } }
            "Committed refusal",
            batch(FarmInput::Committed(ControlRecord::Refusal(
                RefusalRecord {
                    operation: OperationId(3),
                    reason: "r".to_owned(),
                },
            ))),
            batch_hex(15, 9, "2a071a050803120172"),
        ),
        (
            // input (12, 64): capacity { worker "w" (3), capacity (12, 8) (10), caps
            // (1a, 49) (51) }
            "Capacity",
            batch(FarmInput::Capacity {
                worker: w("w"),
                capacity: resources(),
                caps: caps(),
            }),
            batch_hex(
                0x48,
                0x42,
                &format!("12400a01771208{RESOURCES_HEX}1a31{CAPS_HEX}"),
            ),
        ),
        (
            // input (0a, 67): worker up { worker "w" (3), instance (12) "i" (3),
            // capacity (1a, 8) (10), caps (22, 49) (51) }
            "WorkerUp",
            batch(FarmInput::WorkerUp {
                worker: w("w"),
                instance: DaemonInstance::new("i"),
                predecessor_stopped: false,
                capacity: resources(),
                caps: caps(),
            }),
            batch_hex(
                0x4b,
                0x45,
                &format!("0a430a01771201691a08{RESOURCES_HEX}2231{CAPS_HEX}"),
            ),
        ),
        (
            // As above, with the predecessor proven stopped: predecessor_stopped (28) 1.
            "WorkerUp, predecessor stopped",
            batch(FarmInput::WorkerUp {
                worker: w("w"),
                instance: DaemonInstance::new("i"),
                predecessor_stopped: true,
                capacity: resources(),
                caps: caps(),
            }),
            batch_hex(
                0x4d,
                0x47,
                &format!("0a450a01771201691a08{RESOURCES_HEX}2231{CAPS_HEX}2801"),
            ),
        ),
        (
            // Qos { builtin (08) 3 }, kind 2.
            "Submit",
            batch(FarmInput::Submit(request(
                Qos::Batch,
                LeaseKind::WholeMachine,
            ))),
            request_hex("0803", 2),
        ),
        (
            "Submit device",
            batch(FarmInput::Submit(device_request())),
            device_request_hex(),
        ),
        (
            // Report (22, 176 as the varint b0 01): farm time 1, operation 3, lease
            // (1a) 1.2, outcome (22, 40) { completed (0a) d(3) }, action entry (2a,
            // 122) { action (0a) d(4), record (12, 80) { result (0a) d(3), closure
            // (12) d(5) } }
            "Report",
            LogCommand::Report(Report {
                now: FarmTime::from_millis(1),
                operation: OperationId(3),
                lease: lease(1, 2),
                outcome: Outcome::Completed {
                    action_result: d(3).into(),
                },
                action_entry: Some(ActionEntry {
                    action: d(4),
                    record: ActionRecord {
                        result: d(3),
                        closure: [d(5)].into_iter().collect::<Closure>(),
                    },
                }),
            }),
            [
                "22b001080110031a0408011002".to_owned(),
                format!("22280a26{}", digest_hex(3)),
                format!("2a7a0a26{}", digest_hex(4)),
                format!("12500a26{}1226{}", digest_hex(3), digest_hex(5)),
            ]
            .concat(),
        ),
        // Report without an action entry; failure 1 (timeout).
        (
            "Report failed",
            failed_report(Failure::Timeout),
            failed_report_hex(1),
        ),
        (
            // Takeover (2a): term 7, farm time 9
            "Takeover",
            LogCommand::Takeover(Takeover {
                term: NonZeroU64::new(7).expect("non-zero"),
                now: FarmTime::from_millis(9),
            }),
            "2a0408071009".to_owned(),
        ),
        (
            // Checkpoint (32, 36): index 5, digest (12, 32)
            "Checkpoint",
            LogCommand::Checkpoint(Checkpoint {
                index: 5,
                digest: [0xab; 32],
            }),
            format!("322408051220{}", "ab".repeat(32)),
        ),
        (
            // MachineVersion (3a): version 2
            "MachineVersion",
            LogCommand::MachineVersion(MachineVersion::new(NonZeroU32::new(2).expect("2"))),
            "3a020802".to_owned(),
        ),
    ]
}

fn check(name: &str, command: &LogCommand, hex: &str) {
    let bytes = hex::decode(hex).unwrap_or_else(|e| panic!("{name}: bad hex: {e}"));
    assert_eq!(hex::encode(enc(command)), hex, "{name}: encoding changed");
    assert_eq!(
        dec(&bytes).as_ref(),
        Ok(command),
        "{name}: golden bytes decode differently"
    );
}

/// Catches: a field tag swapped or renumbered, a type changed, a command or input
/// moved; and a decoder that disagrees with the bytes an older binary wrote.
#[test]
fn every_control_command_has_its_golden_bytes() {
    for (name, command, hex) in golden() {
        check(name, &command, &hex);
    }
}

/// Every control command and every farm input is in the golden set, so one added
/// without golden bytes fails here (the matches are exhaustive).
#[test]
fn the_golden_set_names_every_command_and_input() {
    let mut commands = [false; 5];
    let mut inputs = [false; 11];
    for (_, command, _) in golden() {
        let i = match &command {
            LogCommand::Meta(_) => unreachable!("metadata commands are meta_codec's"),
            LogCommand::Bootstrap(_) => unreachable!("Bootstrap is bootstrap_codec's"),
            LogCommand::Unknown => unreachable!("an unknown command has no golden bytes"),
            LogCommand::Farm(b) => {
                for input in &b.inputs {
                    let j = match input {
                        FarmInput::WorkerUp { .. } => 0,
                        FarmInput::Capacity { .. } => 1,
                        FarmInput::Heartbeat { .. } => 2,
                        FarmInput::Submit(_) => 3,
                        FarmInput::Committed(_) => 4,
                        FarmInput::Started { .. } => 5,
                        FarmInput::Tick => 6,
                        FarmInput::Cordon(_) => 7,
                        FarmInput::Drain { .. } => 8,
                        FarmInput::Uncordon(_) => 9,
                        FarmInput::Disconnected(_) => 10,
                    };
                    inputs[j] = true;
                }
                0
            }
            LogCommand::Report(_) => 1,
            LogCommand::Takeover(_) => 2,
            LogCommand::Checkpoint(_) => 3,
            LogCommand::MachineVersion(_) => 4,
        };
        commands[i] = true;
    }
    assert!(commands.iter().all(|s| *s), "{commands:?}");
    assert!(inputs.iter().all(|s| *s), "{inputs:?}");
}

/// Catches an enum value renumbered: each value of every enum the control commands
/// carry, with the number `log.proto` gives it.
#[test]
fn every_enum_value_has_its_number() {
    let failures = [
        (Failure::Timeout, 1),
        (Failure::Infra, 2),
        (Failure::Invalid, 3),
        (Failure::OutOfMemory, 4),
        (Failure::NodeMemoryPressure, 5),
        (Failure::MissingInput, 6),
        (Failure::Lost(LostWhy::DaemonRestarted), 7),
        (Failure::Lost(LostWhy::Fenced), 8),
    ];
    for (failure, n) in failures {
        check(
            &format!("{failure:?}"),
            &failed_report(failure),
            &failed_report_hex(n),
        );
    }
    let kinds = [
        (LeaseKind::Action, 1),
        (LeaseKind::WholeMachine, 2),
        (LeaseKind::Vm, 3),
    ];
    for (kind, n) in kinds {
        let submit = batch(FarmInput::Submit(request(Qos::Batch, kind)));
        check(&format!("{kind:?}"), &submit, &request_hex("0803", n));
    }
    // Qos { builtin (08) n }, and a custom level: { custom (12) { name "c", urgency 150
    // } } in a Qos of its own length, so its request is written out separately below.
    for (qos, n) in [(Qos::Interactive, 1), (Qos::Ci, 2), (Qos::Batch, 3)] {
        let submit = batch(FarmInput::Submit(request(qos.clone(), LeaseKind::Action)));
        check(
            &format!("{qos:?}"),
            &submit,
            &request_hex(&format!("08{n:02x}"), 1),
        );
    }
    let custom = Qos::custom("c", 150).expect("custom");
    let mut r = request(custom, LeaseKind::Action);
    r.needs = kbf_caps::Request::default();
    r.hermetic = false;
    r.do_not_cache = false;
    // Request: key (0a, 45), qos (12, 10) { custom (12, 6) { name (0a) "c", urgency (10)
    // 150 as 96 01 } }, kind 1 (2), resources (22, 4) (6) = 63 (0x3f); Submit 65; input
    // 67; batch 73 (0x49).
    let hex = batch_hex(
        0x49,
        0x43,
        &format!(
            "22410a3f0a2b0a01691226{}120812060a0163109601180122040801 1002",
            digest_hex(3)
        )
        .replace(' ', ""),
    );
    check("custom QoS", &batch(FarmInput::Submit(r)), &hex);
    // A node's architecture (CpuCaps.arch) and each countable (ConsumableAmount): an
    // x86-64 node with 7 of each, in a Capacity input. NodeCaps (1a, 28):
    // cpu (0a, 2) { arch 1 }, consumables (22, 4) { n, 7 } for n = 1 to 4.
    let mut node = NodeCaps::new(CpuCaps::new(Arch::X86_64, Default::default()));
    let all = [
        Consumable::Cpus,
        Consumable::MemGib,
        Consumable::NvmeGib,
        Consumable::Gpus,
    ];
    for c in all {
        node.consumables.insert(c, 7);
    }
    let capacity = batch(FarmInput::Capacity {
        worker: w("w"),
        capacity: Resources::default(),
        caps: node,
    });
    let consumables: String = (1..=4).map(|n| format!("220408{n:02x}1007")).collect();
    // Capacity: worker (3), capacity (12, 0) (2), caps (1a, 28) (30) = 35; input 37;
    // batch 43.
    let hex = batch_hex(
        0x2b,
        0x25,
        &format!("12230a01771200 1a1c0a020801{consumables}").replace(' ', ""),
    );
    check("arch and consumables", &capacity, &hex);
}

/// The `Capacity` input of the golden set, with `disk_bytes` 7: the resources message gains
/// `disk_bytes` (field 6, `30 07`) at its end and nothing else moves, so the
/// capacity (12) is 0x42 bytes, the input (1a) 0x44, the batch 0x4a.
///
/// Catches: a disk field with another number or type (a second binary would read the
/// disk as another dimension), and a disk the encoder drops (a restart would book none).
#[test]
fn a_capacity_with_disk_bytes_writes_field_6_of_resources() {
    let capacity = batch(FarmInput::Capacity {
        worker: w("w"),
        capacity: resources().with_disk_bytes(7),
        caps: caps(),
    });
    let hex = batch_hex(
        0x4a,
        0x44,
        &format!("12420a0177120a{RESOURCES_HEX}3007 1a31{CAPS_HEX}").replace(' ', ""),
    );
    check("capacity with disk", &capacity, &hex);
}

/// The bytes of the `Capacity` golden, written before `disk_bytes` existed, decode to a
/// capacity of disk 0; and a capacity of disk 0 encodes to exactly those bytes.
///
/// Catches: a reader that refuses a resources message without field 6, one that takes a
/// default other than 0, and an encoder that writes a zero disk (every committed row and
/// log entry would change under a new binary).
#[test]
fn bytes_without_a_disk_field_read_as_disk_0_and_disk_0_writes_none() {
    let old = batch_hex(
        0x48,
        0x42,
        &format!("12400a01771208{RESOURCES_HEX}1a31{CAPS_HEX}"),
    );
    let capacity = batch(FarmInput::Capacity {
        worker: w("w"),
        capacity: resources(),
        caps: caps(),
    });
    assert_eq!(resources().disk_bytes, 0);
    check("capacity without disk", &capacity, &old);
}

/// A request of disk 7 (resources `08 01 10 02 30 07`, 6 bytes) in a batch.
#[test]
fn a_request_with_disk_bytes_writes_field_6_of_its_resources() {
    let mut r = request(Qos::Ci, LeaseKind::Action);
    r.resources = r.resources.with_disk_bytes(7);
    let with = enc(&batch(FarmInput::Submit(r.clone())));
    let without = enc(&batch(FarmInput::Submit(request(
        Qos::Ci,
        LeaseKind::Action,
    ))));
    // Two bytes longer, in the request (+2), its input (+2) and the batch (+2): the old
    // resources bytes `220408011002` become `2206080110023007`.
    let (with_hex, without_hex) = (hex::encode(&with), hex::encode(&without));
    assert!(without_hex.contains("220408011002"), "{without_hex}");
    assert!(with_hex.contains("2206080110023007"), "{with_hex}");
    assert_eq!(with.len(), without.len() + 2);
    assert_eq!(dec(&with), Ok(batch(FarmInput::Submit(r))));
}
