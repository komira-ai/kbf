//! The row codec over `kbf-log`: the bytes of each row value, what a value reads back as, and
//! the lease row's cost.

use js_store::{RowCodec, StateError, Value, WorkerUp};
use kbf_caps::NodeCaps;
use kbf_log::{FarmBatch, FarmInput, Format, LogCommand};
use kbf_sched::{DaemonInstance, Request};
use kbf_types::sched::usage::LeaseCost;
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, FarmTime, LeaseGrant, LeaseId, LeaseKind,
    OperationId, Qos, Resources, WorkKey, WorkerId,
};

use super::codec::KbfRowCodec;
use super::testing::charged;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex digits"))
        .collect()
}

fn request() -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [1; 32], 1),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
        whole_worker: false,
    }
}

fn grant(cost: LeaseCost) -> LeaseGrant {
    LeaseGrant {
        lease: LeaseId::new(1, 2),
        operation: OperationId(3),
        worker: WorkerId::new("w"),
        cost,
    }
}

fn up() -> WorkerUp {
    WorkerUp {
        worker: WorkerId::new("node-a"),
        instance: DaemonInstance::new("daemon-1"),
        predecessor_stopped: true,
        capacity: Resources::new(2_000, 1 << 30),
        caps: NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps"),
    }
}

fn corrupt_key(error: StateError) -> String {
    match error {
        StateError::Corrupt { key, .. } => key,
        other => panic!("{other:?}"),
    }
}

/// Asserts that the lease row of a grant that carries no cost is the bytes the store layer
/// has always written for a lease, and that those bytes read back as a lease with no recorded
/// cost, `retry` false.
///
/// Catches: a codec that writes a cost field for a default cost (every lease row already in a
/// store would differ from the one a new binary writes), and one that reads a row written
/// before costs were kept as a lease charged nothing (a restart would book the lease at zero
/// instead of its request).
#[test]
fn a_lease_row_without_a_cost_is_the_old_row_and_reads_as_a_legacy_lease() {
    let old = hex(concat!("1a111a0f2a0d0a0b", "0a04080110021003", "1a0177"));
    let bytes = KbfRowCodec
        .lease(&grant(LeaseCost::default()))
        .expect("lease");
    assert_eq!(bytes, old);
    let Ok(Value::LegacyLease(read)) = KbfRowCodec.read("lease/1", &old) else {
        panic!("not a legacy lease");
    };
    assert_eq!(read, grant(LeaseCost::default()));
    assert!(!read.cost.retry);
}

/// Asserts that a lease row with a cost holds it, `retry` included, in the log's own bytes, and
/// reads back as a lease of that cost.
///
/// Catches: an encoder that drops one of the cost's fields (a recovered lease would be
/// charged less than it was granted, or a rerun up the memory ladder charged as a first
/// run), and a cost field number that moved.
#[test]
fn a_lease_row_with_a_cost_holds_it_and_its_retry_flag() {
    let want = hex(concat!(
        "1a1f1a1d2a1b0a19",
        "0a04080110021003",
        "1a0177",
        "220c080410051806200128013007"
    ));
    let bytes = KbfRowCodec.lease(&grant(charged())).expect("lease");
    assert_eq!(bytes, want);
    let Ok(Value::Lease(read)) = KbfRowCodec.read("lease/1", &bytes) else {
        panic!("not a lease");
    };
    assert_eq!(read.cost, charged());
    assert!(read.cost.retry);
    for retry in [false, true] {
        let cost = LeaseCost { retry, ..charged() };
        let bytes = KbfRowCodec.lease(&grant(cost)).expect("lease");
        let Ok(Value::Lease(read)) = KbfRowCodec.read("lease/1", &bytes) else {
            panic!("not a lease");
        };
        assert_eq!(read.cost.retry, retry);
    }
}

/// Asserts that a request and a registration read back as what they were made from, and that
/// their bytes are those of a one-input batch of the log.
///
/// Catches: a value read back as another kind, or a field lost on the way.
#[test]
fn a_request_and_a_registration_read_back() {
    let submit = KbfRowCodec.submit(&request()).expect("submit");
    let kept = kbf_log::encode(
        Format::V1,
        &LogCommand::Farm(FarmBatch {
            now: FarmTime::default(),
            wall: std::time::UNIX_EPOCH,
            inputs: vec![FarmInput::Submit(request())],
        }),
    )
    .expect("encode");
    assert_eq!(submit, kept);
    let Ok(Value::Submit(read)) = KbfRowCodec.read("op/1", &submit) else {
        panic!("not a request");
    };
    assert_eq!(read, request());

    let session = KbfRowCodec.session(&up()).expect("session");
    let Ok(Value::Session(read)) = KbfRowCodec.read("session/node-a", &session) else {
        panic!("not a registration");
    };
    assert_eq!(read, up());
}

/// Asserts that a value the log holds but no row does is `Other`, a command a later binary
/// wrote is `Unknown`, and bytes that are not a value, or a batch of several inputs, are
/// refused naming the row.
///
/// Catches: a later binary's row read as malformed (a start would refuse a store it should
/// skip a row of), a malformed row skipped as a later binary's (work silently lost), and a
/// row of two inputs read as the first.
#[test]
fn other_unknown_and_malformed_values_are_told_apart() {
    let tick = |inputs| {
        kbf_log::encode(
            Format::V1,
            &LogCommand::Farm(FarmBatch {
                now: FarmTime::default(),
                wall: std::time::UNIX_EPOCH,
                inputs,
            }),
        )
        .expect("encode")
    };
    assert!(matches!(
        KbfRowCodec.read("op/1", &tick(vec![FarmInput::Tick])),
        Ok(Value::Other)
    ));
    let unknown = vec![0x0a, 0x03, 0xaa, 0x01, 0x00];
    assert!(matches!(
        KbfRowCodec.read("op/1", &unknown),
        Ok(Value::Unknown)
    ));
    let garbage = KbfRowCodec.read("op/2", &[0xff, 0xff, 0xff]);
    assert_eq!(corrupt_key(garbage.expect_err("garbage")), "op/2");
    let two = tick(vec![FarmInput::Tick, FarmInput::Tick]);
    assert_eq!(
        corrupt_key(KbfRowCodec.read("op/3", &two).expect_err("two inputs")),
        "op/3"
    );
}

/// Asserts that a value the log cannot encode is refused as a codec error, not written.
///
/// Catches: an encoding failure swallowed into an empty row.
#[test]
fn a_request_the_log_cannot_encode_is_a_codec_error() {
    let request = Request {
        key: WorkKey::new("main", "not an action digest"),
        ..request()
    };
    let error = KbfRowCodec.submit(&request).expect_err("not encodable");
    assert!(matches!(error, StateError::Codec(_)), "{error:?}");
}

/// Asserts that a control record that is not a lease is not read as one.
///
/// Catches: a refusal row taken for a lease row (a grant would be recovered from it).
#[test]
fn a_control_record_that_is_not_a_lease_is_other() {
    let input = FarmInput::Committed(ControlRecord::Refusal(kbf_types::RefusalRecord {
        operation: OperationId(3),
        reason: "r".to_owned(),
    }));
    let batch = FarmBatch {
        now: FarmTime::default(),
        wall: std::time::UNIX_EPOCH,
        inputs: vec![input],
    };
    let refusal = kbf_log::encode(Format::V1, &LogCommand::Farm(batch)).expect("encode");
    assert!(matches!(
        KbfRowCodec.read("lease/1", &refusal),
        Ok(Value::Other)
    ));
}
