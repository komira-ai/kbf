//! The row codec and the rows of the build server, behind their traits: the bytes are
//! the ones the rows always held, a value no row holds or a later binary wrote is told
//! from one that is malformed, and a store layer opened with other implementations writes
//! what they say.

use std::sync::atomic::AtomicUsize;

use kbf_caps::NodeCaps;
use kbf_state::ManualClock;
use kbf_state::fake::{Fake, Flaw};
use kbf_types::{DigestFunction, FencePolicy, LeaseKind, Qos, Resources, StartLease};

use super::codec::{KbfLog, RowCodec, Value, WorkerUp};
use super::*;

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
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

fn grant() -> LeaseGrant {
    LeaseGrant {
        lease: LeaseId::new(5, 0),
        operation: OperationId(1),
        worker: WorkerId::new("node-a"),
        cost: Default::default(),
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

fn store() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn subject() -> Subject {
    Subject::of(Described::default()).0
}

/// Asserts that each value [`KbfLog`] writes is the `kbf-log` encoding of the scheduler
/// input it stands for, byte for byte, and reads back as what it was made from.
///
/// Catches: a codec that changes a row's bytes (a store written before the change would no
/// longer read, and one written after it would not be read by an older binary), or one that
/// reads a value back as another kind.
#[test]
fn the_kbf_log_codec_writes_the_scheduler_inputs_it_always_wrote() {
    let submit = KbfLog.submit(&request(1)).expect("submit");
    let kept = encode(FarmInput::Submit(request(1))).expect("encode");
    assert_eq!(submit, kept);
    let lease = KbfLog.lease(&grant()).expect("lease");
    let kept = encode(FarmInput::Committed(ControlRecord::Lease(grant()))).expect("encode");
    assert_eq!(lease, kept);
    let session = KbfLog.session(&up()).expect("session");
    let kept = encode(FarmInput::WorkerUp {
        worker: WorkerId::new("node-a"),
        instance: DaemonInstance::new("daemon-1"),
        predecessor_stopped: true,
        capacity: Resources::new(2_000, 1 << 30),
        caps: up().caps,
    })
    .expect("encode");
    assert_eq!(session, kept);

    let Ok(Value::Submit(read)) = KbfLog.read("op/1", &submit) else {
        panic!("not a request");
    };
    assert_eq!(read, request(1));
    let Ok(Value::Lease(read)) = KbfLog.read("lease/1", &lease) else {
        panic!("not a lease");
    };
    assert_eq!(read, grant());
    let Ok(Value::Session(read)) = KbfLog.read("session/node-a", &session) else {
        panic!("not a registration");
    };
    assert_eq!(read.instance, DaemonInstance::new("daemon-1"));
    assert_eq!(read.capacity, Resources::new(2_000, 1 << 30));
    assert!(read.predecessor_stopped);
}

/// Asserts that a value the codec reads but no row holds is `Other`, one a later binary wrote
/// is `Unknown`, and one that is not a value at all is refused naming its row.
///
/// Catches: a later binary's row read as malformed (a start would refuse a store it should
/// skip a row of), or a malformed row skipped as a later binary's (work silently lost).
#[test]
fn a_value_no_row_holds_is_other_and_a_later_binarys_is_unknown() {
    let tick = encode(FarmInput::Tick).expect("encode");
    assert!(matches!(KbfLog.read("op/1", &tick), Ok(Value::Other)));
    let unknown = vec![0x0a, 0x03, 0xaa, 0x01, 0x00];
    assert!(matches!(KbfLog.read("op/1", &unknown), Ok(Value::Unknown)));
    let Err(StateError::Corrupt { key, .. }) = KbfLog.read("op/1", &[0xff, 0xff, 0xff]) else {
        panic!("not refused");
    };
    assert_eq!(key, "op/1");
}

/// Asserts that a row read as the wrong kind of value is refused with the words of the kind
/// wanted, and that a row of a later binary's is refused with its own, whichever kind is wanted.
///
/// Catches: a refusal that names the wrong kind of row, or one that calls a later binary's
/// row corrupt.
#[test]
fn a_row_of_the_wrong_kind_is_refused_saying_which() {
    let durable = Durable::open(store(), EpochPolicy::Keep, 1)
        .expect("open")
        .0;
    let row = |value: Vec<u8>| Versioned {
        value,
        version: Version(1),
        expires_at: None,
    };
    let tick = row(encode(FarmInput::Tick).expect("encode"));
    let lease = row(KbfLog.lease(&grant()).expect("lease"));
    let unknown = row(vec![0x0a, 0x03, 0xaa, 0x01, 0x00]);
    let why = |error: StateError| match error {
        StateError::Corrupt { why, .. } => why,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        why(durable.request_row("op/1", &lease).expect_err("lease")),
        "not a request"
    );
    assert_eq!(
        why(durable.request_row("op/1", &tick).expect_err("tick")),
        "not a request"
    );
    assert_eq!(
        why(durable.lease_row("lease/1", &tick).expect_err("tick")),
        "not a lease grant"
    );
    assert_eq!(
        why(durable.session_row("session/a", &lease).expect_err("lease")),
        "not a daemon registration"
    );
    let known = "a command this binary does not know";
    assert_eq!(
        why(durable.request_row("op/1", &unknown).expect_err("unknown")),
        known
    );
    assert_eq!(
        why(durable.lease_row("lease/1", &unknown).expect_err("unknown")),
        known
    );
    assert_eq!(
        why(durable
            .session_row("session/a", &unknown)
            .expect_err("unknown")),
        known
    );
}

/// A codec that counts what it is asked to write and writes it as [`KbfLog`] does.
#[derive(Default)]
struct Counting {
    submits: AtomicUsize,
    leases: AtomicUsize,
    sessions: AtomicUsize,
}

impl RowCodec for Counting {
    fn submit(&self, request: &Request) -> Result<Vec<u8>, StateError> {
        self.submits.fetch_add(1, Ordering::SeqCst);
        KbfLog.submit(request)
    }

    fn lease(&self, grant: &LeaseGrant) -> Result<Vec<u8>, StateError> {
        self.leases.fetch_add(1, Ordering::SeqCst);
        KbfLog.lease(grant)
    }

    fn session(&self, up: &WorkerUp) -> Result<Vec<u8>, StateError> {
        self.sessions.fetch_add(1, Ordering::SeqCst);
        KbfLog.session(up)
    }

    fn read(&self, key: &str, bytes: &[u8]) -> Result<Value, StateError> {
        KbfLog.read(key, bytes)
    }
}

/// Asserts that a store layer opened with a codec of its own asks it for every request,
/// lease and registration it writes, and writes the bytes it returns.
///
/// Catches: a write path that still encodes with the `kbf-log` codec directly, which a store
/// layer owned by another repository could not change.
#[test]
fn the_store_layer_writes_the_bytes_its_codec_returns() {
    let codec = Arc::new(Counting::default());
    let (durable, _) = Durable::open_with(
        store(),
        EpochPolicy::Keep,
        1,
        codec.clone(),
        Arc::new(KbfRows),
    )
    .expect("open");
    durable
        .submitted(OperationId(1), &request(1), &subject())
        .expect("submit");
    let start = StartLease {
        worker: WorkerId::new("node-a"),
        lease: LeaseId::new(5, 0),
        operation: OperationId(1),
        key: request(1).key,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        fence: FencePolicy::RunOn,
        device: None,
    };
    durable.granted(&start).expect("grant");
    let reported = Reported::serving(Resources::new(2_000, 1 << 30), up().caps);
    durable
        .session(
            &WorkerId::new("node-a"),
            &DaemonInstance::new("daemon-1"),
            false,
            &reported,
        )
        .expect("session");
    let counts = [&codec.submits, &codec.leases, &codec.sessions];
    let counts = counts.map(|count| count.load(Ordering::SeqCst));
    assert_eq!(counts, [1, 1, 1]);
}
