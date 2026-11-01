//! The bytes of every row the scheduler writes to the state store, as a file.
//!
//! `tests/golden/farm-rows/scheduler-rows.txt` holds, for one store that a handle of the
//! scheduler wrote through its own calls, every row: its key, when the store expires it and
//! its value as hex. The calls cover each kind of row the scheduler writes (`op/`, `queue/`,
//! `lease/`, `lease-by-op/`, `session/`, `owner/`, `cordon/`, `done/`, `doneof/`, `loss/`,
//! `fate/`, `floor/`, `keyfloor/`, `opkey/`, `opwall/`, `subject/`, `inflight/`, `est/` and the `meta/`
//! rows), so a change to the key of a row, to its value or to how long it lives fails here,
//! and a row this release adds is missing from the file until a person adds it with the
//! reason. This test requires:
//!
//! - the rows the calls write are the rows of the file, byte for byte;
//! - every kind above is in the file (the file cannot lose a kind and still pass);
//! - each row decodes with the reader of its kind and encodes back to the same bytes, so a
//!   reader and a writer that drift apart are caught as well as a writer that moves.
//!
//! The expiry of a row is on the store's clock, which starts at zero, is never advanced by
//! the test and ticks once for each transaction, so the expiry is the same on every run.
//! The file is append-only in spirit: rows an earlier release wrote are not edited to match a
//! new encoding, because a store written by that release holds them as they were. A change to
//! the file needs a reason in the pull request that makes it.
//!
//! The size summary of the `est/` row is synthetic: one made-up quiet run fed to the
//! estimator's own update path (`Summaries::observe`) and then encoded. No farm data is read
//! to make it.
//!
//! `tests/golden/farm-rows/scheduler-variants.txt` holds the rows of a second store, written
//! the same way, for the values the first leaves out: one `op/` row for every `Qos` and one
//! for every `LeaseKind` (the same rows: the kinds take turns across the levels, so it is not
//! a row for each pair), the `done/` row of an answer for every `Failure` (the way a failure
//! reaches the store is the result its callers are told), the `done/` row of a refusal, and a
//! cordon on a node that has a session. Each enumeration below is a chain written as an
//! exhaustive match,
//! so a variant added to one of these types does not compile until it is placed in its chain,
//! and then it has rows in the file or fails the comparison.
//!
//! Not rows, by what the writer reads:
//!
//! - `RequeueReason`: for the same loss count and floors, the give-up writes the same rows
//!   whatever the reason (`a_requeue_reason_is_not_a_row` shows it). The caller derives the
//!   `loss/` and `floor/` rows from the reason, so those rows are in the first file as the
//!   caller's parameters, not as a value of the reason.
//! - `FencePolicy` and the `device` of a `StartLease`: the grant writes a `LeaseGrant` (lease,
//!   operation, worker) and nothing of the start (`a_start_writes_the_grant_and_nothing_of_it`).
//! - `Effect::Commit` and `Effect::Waiting`: persisting them writes nothing
//!   (`an_effect_that_carries_no_row_writes_none`). `Start`, `Answer` and `Refuse` are in the
//!   rows.

use std::fmt::Write as _;

use kbf_front::ActionLabel;
use kbf_proto::reapi::ActionResult;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{GENERATION, ManualClock, Versioned};
use kbf_types::{
    ActionKey, Answer, ControlRecord, Digest, DigestFunction, Failure, FarmFault, FarmRun,
    FencePolicy, LeaseKind, LostWhy, MemoryKill, MemoryRun, Outcome, Qos, Refusal, RefusalRecord,
    Resources, StartLease, Waiting,
};

use super::owner::{pair_of, pair_row};
use super::*;
use crate::action_usage::{ShapeKey, Target, TargetKind};

const GOLDEN: &str = include_str!("../../../tests/golden/farm-rows/scheduler-rows.txt");
const VARIANTS: &str = include_str!("../../../tests/golden/farm-rows/scheduler-variants.txt");

/// The kinds of row the scheduler writes, as the start of their keys: the constants that
/// the writers build their keys from.
const KINDS: [&str; 23] = [
    OPERATION,
    QUEUE,
    LEASE,
    leases::LEASE_BY_OP,
    SESSION,
    owner::OWNER,
    CORDON,
    DONE,
    named::DONE_OF,
    LOSS,
    FATE,
    FLOOR,
    KEY_FLOOR,
    OPKEY,
    OPWALL,
    SUBJECT,
    inflight::INFLIGHT,
    ESTIMATE,
    ALIVE,
    EPOCH,
    GENERATION,
    NEXT_OP,
    NEXT_SEQ,
];

/// The wall time operation 8 of the first store is queued with.
pub(super) const WALL: Duration = Duration::new(3_600, 250_000_000);

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

fn subject() -> Subject {
    Subject {
        key: ShapeKey::from_bytes([1, 2, 3, 4, 5, 6, 7, 8]),
        cfg: Some("0123456789abcdef-fedcba9876543210".to_owned()),
        target: Some(Target {
            kind: TargetKind::Anon,
            label: "root//pkg:t".to_owned(),
        }),
        test: true,
        label: ActionLabel {
            mnemonic: Some("CxxCompile".to_owned()),
            target: Some("root//pkg:hint".to_owned()),
        },
    }
}

fn start(operation: u64, seq: u64, tag: u8) -> StartLease {
    StartLease {
        worker: WorkerId::new("node-a"),
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        key: request(tag).key,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        fence: FencePolicy::RunOn,
        device: None,
    }
}

fn answer(operation: u64, seq: u64) -> Answer {
    Answer {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        waiters: Vec::new(),
        outcome: Outcome::Failed(Failure::Invalid),
        memory_runs: Vec::new(),
        farm_runs: Vec::new(),
    }
}

fn status(code: i32, message: &str) -> kbf_proto::google::rpc::Status {
    kbf_proto::google::rpc::Status {
        code,
        message: message.to_owned(),
        details: Vec::new(),
    }
}

fn ending(finished: Finished) -> Ending {
    Ending {
        finished,
        retention: Duration::from_secs(1),
    }
}

fn requeue(operation: u64, seq: u64) -> Requeue {
    Requeue {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        worker: WorkerId::new("node-a"),
        reason: RequeueReason::OutOfMemory {
            booked: 4 << 30,
            raised: 16 << 30,
        },
    }
}

fn fate() -> Fate {
    Fate {
        booked_bytes: 16 << 30,
        memory_runs: vec![
            MemoryRun {
                worker: WorkerId::new("node-a"),
                booked: 4 << 30,
                kill: MemoryKill::OwnLimit { cap: 64 << 30 },
            },
            MemoryRun {
                worker: WorkerId::new("node-b"),
                booked: 16 << 30,
                kill: MemoryKill::NodePressure,
            },
        ],
        farm_runs: vec![
            FarmRun {
                worker: WorkerId::new("node-a"),
                fault: FarmFault::NodeMemoryPressure,
            },
            FarmRun {
                worker: WorkerId::new("node-b"),
                fault: FarmFault::Lost(LostWhy::DaemonRestarted),
            },
            FarmRun {
                worker: WorkerId::new("node-c"),
                fault: FarmFault::Lost(LostWhy::Fenced),
            },
            FarmRun {
                worker: WorkerId::new("node-c"),
                fault: FarmFault::Infra,
            },
        ],
        avoid: ["node-a", "node-b"]
            .map(WorkerId::new)
            .into_iter()
            .collect(),
        not_started: 3,
    }
}

fn size_key() -> kbf_estimator::Key {
    use kbf_estimator::{Basis, Cfg, Instance, Key, PlatformClass, Shape};
    Key::new(
        Instance::new("main"),
        PlatformClass::new("linux/x86_64"),
        Basis::Cgroup,
        Shape(0x0123_4567_89ab_cdef),
        Some(Cfg(7)),
    )
}

/// A size summary that has seen one quiet run, as its stored bytes.
fn size_summary() -> Vec<u8> {
    use kbf_estimator::{Bounds, Day, Outcome as RunOutcome, Run, Summaries};
    let run = Run {
        outcome: RunOutcome::Success,
        wall_us: 2_000_000,
        peak_memory_bytes: Some(1 << 28),
        booked_memory_bytes: 4 << 30,
        raised_memory_bytes: None,
        limit_hits: 0,
        swap_peak_bytes: 0,
        cpu_usage_us: 0,
        node_cpu_pressure_permille: None,
    };
    let mut summaries = Summaries::default();
    summaries.observe(&size_key(), &run, Day(100), &Bounds::default());
    summaries.get(&size_key()).expect("a summary").encode()
}

/// Submits operation `operation` of action `tag` (with the size key `size`, if any) and
/// grants it lease `seq`.
fn granted(durable: &Durable, operation: u64, seq: u64, tag: u8, size: Option<u64>) {
    granted_request(durable, operation, seq, &request(tag), size, None);
}

/// [`granted`] for an operation of `request`.
fn granted_request(
    durable: &Durable,
    operation: u64,
    seq: u64,
    request: &Request,
    size: Option<u64>,
    wall: Option<Duration>,
) {
    let submitted = durable.submit_keyed(OperationId(operation), request, &subject(), size, wall);
    submitted.expect("submit");
    let start = StartLease {
        key: request.key.clone(),
        kind: request.kind,
        ..start(operation, seq, 0)
    };
    durable
        .persist(&[Effect::Start(start)], None)
        .expect("grant");
}

/// A fresh store with the shared marker, and a handle of the scheduler on it that has
/// reserved ids and seen `node-a` arrive and register a session.
fn opened() -> (Arc<Fake>, Durable) {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    kbf_state::share(&*store).expect("the shared marker");
    let handle: Arc<dyn Store> = store.clone();
    let (durable, _) =
        Durable::open(handle, EpochPolicy::Keep, 0x0102_0304_0506_0708).expect("open");
    durable
        .reserve_operation(OperationId(4))
        .expect("operation ids");
    durable.reserve_seqs(0).expect("lease sequences");
    let node = WorkerId::new("node-a");
    durable
        .arrive(&node, FarmTime::from_millis(0))
        .expect("a Hello");
    let caps =
        kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    let reported = Reported::serving(Resources::new(2_000, 1 << 30), caps);
    let instance = DaemonInstance::new("daemon-1");
    durable
        .session(&node, &instance, true, &reported)
        .expect("a session");
    (store, durable)
}

/// A store written through the calls of the scheduler's own handle, one call for each row it
/// writes, in the order the scheduler makes them.
fn written() -> Arc<Fake> {
    let (store, durable) = opened();
    let drain = StoredCordon::Draining {
        deadline_unix_ms: 1_234_567,
    };
    durable
        .cordoned(&WorkerId::new("node-b"), Some(drain))
        .expect("a drain");
    durable
        .cordoned(&WorkerId::new("node-c"), Some(StoredCordon::Cordoned))
        .expect("a cordon");

    // Operation 4 was killed for memory: queued again, with its fate, the floors and the
    // loss count that the requeue writes, and the summary the key floor names.
    granted(&durable, 4, 0, 1, Some(7));
    let floors = FloorChange {
        raised: Some(Floor {
            key: request(1).key,
            ask: 16 << 30,
            stamp: 2,
        }),
        key_raised: Some(KeyFloor {
            key: 7,
            ask: 16 << 30,
            stamp: 2,
        }),
        ..FloorChange::default()
    };
    let name = size_key();
    let queued =
        durable.requeued_named(&requeue(4, 0), Some(2), Some(&fate()), &floors, Some(&name));
    queued.expect("a kill");

    // Operations 5 to 7 finished, one in each way a result is kept.
    let result = ActionResult {
        exit_code: 0,
        stdout_raw: b"out".to_vec(),
        ..ActionResult::default()
    };
    let partial = ActionResult {
        stderr_raw: b"cut".to_vec(),
        ..ActionResult::default()
    };
    let finished = [
        Finished::Failed(status(3, "ended")),
        Finished::Ran(Box::new(result)),
        Finished::TimedOut {
            status: status(4, "timed out"),
            partial: Box::new(partial),
        },
    ];
    for (i, finished) in (0..).zip(finished) {
        let (operation, seq, tag) = (5 + i, 1 + i, 2 + u8::try_from(i).expect("small"));
        granted(&durable, operation, seq, tag, None);
        let effects = [Effect::Answer(answer(operation, seq))];
        durable
            .persist(&effects, Some(&ending(finished)))
            .expect("a result");
    }

    // Operation 8 holds its lease, and has a size key and a wall time.
    granted_request(&durable, 8, 4, &request(9), Some(7), Some(WALL));

    durable
        .put_estimates(&[(size_key(), size_summary())])
        .expect("a size summary");
    durable.ping().expect("a ping");
    store
}

fn render_row(key: &str, row: &Versioned) -> String {
    let expiry = row.expires_at.map_or_else(
        || "-".to_owned(),
        |at| at.as_duration().as_nanos().to_string(),
    );
    let mut line = format!("{key} expires_ns={expiry} value=");
    for byte in &row.value {
        write!(line, "{byte:02x}").expect("a string takes text");
    }
    line
}

/// The rows of `store`, a line each.
fn rendered(store: &Fake) -> Vec<String> {
    let rows = store.scan_prefix("").expect("a scan");
    rows.iter().map(|(key, row)| render_row(key, row)).collect()
}

/// The lines of the golden file `text`: its rows, without the comment lines.
/// A row too long for a line goes on in lines that begin with two spaces, which continue the
/// value of the row before them.
fn lines_of(text: &str) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        match (line.strip_prefix("  "), rows.last_mut()) {
            (Some(more), Some(last)) => last.push_str(more),
            _ => rows.push(line.to_owned()),
        }
    }
    rows
}

/// The rows of both golden files.
fn golden_lines() -> Vec<String> {
    let mut rows = lines_of(GOLDEN);
    rows.extend(lines_of(VARIANTS));
    rows
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex digits"))
        .collect()
}

/// The key and value of a line of the golden file.
fn split(line: &str) -> (&str, Vec<u8>) {
    let mut parts = line.split(' ');
    let key = parts.next().expect("a key");
    let _expiry = parts.next().expect("an expiry");
    let value = parts.next().and_then(|part| part.strip_prefix("value="));
    (key, unhex(value.expect("a value")))
}

/// Catches: a row whose key, value or lifetime changed (a field renumbered, a key spelled
/// another way, a time to live changed), and a store whose rows an earlier release reads as
/// something else. The store here is written through the scheduler's own calls, so a row the
/// scheduler starts to write, or stops writing, shows as a line added or removed.
#[test]
fn the_rows_the_scheduler_writes_are_the_rows_of_the_file() {
    assert_rows(&written(), GOLDEN);
}

/// Catches: the same for the values the first store leaves out (every `Qos`, `LeaseKind`
/// and `Failure`, a refusal, a cordon on a node with a session).
#[test]
fn the_rows_written_for_every_variant_are_the_rows_of_its_file() {
    assert_rows(&written_variants(), VARIANTS);
}

fn assert_rows(store: &Fake, golden: &str) {
    let found = rendered(store);
    let want = lines_of(golden);
    let keys = |lines: &[String]| -> Vec<String> {
        let first = |line: &String| line.split(' ').next().unwrap_or_default().to_owned();
        lines.iter().map(first).collect()
    };
    assert_eq!(keys(&found), keys(&want), "the keys of the rows");
    for (found, want) in found.iter().zip(&want) {
        assert_eq!(found, want);
    }
}

/// Catches: a key constant of the scheduler that `KINDS` lacks (a new kind of row the file
/// is never asked to hold), a kind listed twice, and a list that holds a kind no constant
/// names. The constants of `durable.rs` are read from its source; the five others live in
/// the modules and `kbf-state` that own them and are named in `KINDS`.
#[test]
fn the_kinds_list_is_the_key_constants() {
    const ELSEWHERE: usize = 5;
    let mut sorted = KINDS.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), KINDS.len(), "a kind listed twice");
    let mut found = 0;
    for line in include_str!("../durable.rs").lines() {
        // A constant other store code reuses is `pub(crate)`; it is a key constant all the same.
        let line = line.strip_prefix("pub(crate) ").unwrap_or(line);
        let Some(rest) = line.strip_prefix("const ") else {
            continue;
        };
        let Some((_, value)) = rest.split_once(": &str = \"") else {
            continue;
        };
        let value = value.trim_end_matches("\";");
        assert!(
            KINDS.contains(&value),
            "{value} is a key constant not in KINDS"
        );
        found += 1;
    }
    assert_eq!(
        found + ELSEWHERE,
        KINDS.len(),
        "KINDS names a constant that is gone"
    );
}

/// Catches: a file that lost a kind of row (the test above would then pass on a scenario
/// that no longer writes it), and a row of a kind no key constant names.
#[test]
fn the_file_holds_every_kind_of_row() {
    let lines = lines_of(GOLDEN);
    for kind in KINDS {
        assert!(
            lines.iter().any(|line| line.starts_with(kind)),
            "no {kind} row in the file"
        );
    }
    for line in golden_lines() {
        assert!(
            KINDS.iter().any(|kind| line.starts_with(kind)),
            "a row of no known kind: {line}"
        );
    }
}

/// The values of an enum: `first`, then what `next` makes of the last one, until it makes
/// none. `next` is a `match` with an arm for every variant, so a variant added to the enum
/// does not compile until it has a place in the chain, and every list below is complete.
fn chain<T>(first: T, next: impl Fn(&T) -> Option<T>) -> Vec<T> {
    let mut all = vec![first];
    while let Some(after) = next(&all[all.len() - 1]) {
        all.push(after);
    }
    all
}

fn every_qos() -> Vec<Qos> {
    chain(Qos::Interactive, |qos| match qos {
        Qos::Interactive => Some(Qos::Ci),
        Qos::Ci => Some(Qos::Batch),
        Qos::Batch => Some(Qos::custom("tenant-a", 150).expect("a level")),
        Qos::Custom(_) => None,
    })
}

fn every_lease_kind() -> Vec<LeaseKind> {
    chain(LeaseKind::Action, |kind| match kind {
        LeaseKind::Action => Some(LeaseKind::WholeMachine),
        LeaseKind::WholeMachine => Some(LeaseKind::Vm),
        LeaseKind::Vm => None,
    })
}

fn every_failure() -> Vec<Failure> {
    chain(Failure::Timeout, |failure| match failure {
        Failure::Timeout => Some(Failure::Infra),
        Failure::Infra => Some(Failure::Invalid),
        Failure::Invalid => Some(Failure::OutOfMemory),
        Failure::OutOfMemory => Some(Failure::NodeMemoryPressure),
        Failure::NodeMemoryPressure => Some(Failure::MissingInput),
        Failure::MissingInput => Some(Failure::Lost(LostWhy::DaemonRestarted)),
        Failure::Lost(LostWhy::DaemonRestarted) => Some(Failure::Lost(LostWhy::Fenced)),
        Failure::Lost(LostWhy::Fenced) => None,
    })
}

fn every_fence() -> Vec<FencePolicy> {
    chain(FencePolicy::RunOn, |fence| match fence {
        FencePolicy::RunOn => Some(FencePolicy::SelfFence),
        FencePolicy::SelfFence => None,
    })
}

fn every_requeue_reason() -> Vec<RequeueReason> {
    let oom = RequeueReason::OutOfMemory {
        booked: 4 << 30,
        raised: 16 << 30,
    };
    chain(RequeueReason::Silent, |reason| match reason {
        RequeueReason::Silent => Some(RequeueReason::Replaced),
        RequeueReason::Replaced => Some(RequeueReason::Reconnected),
        RequeueReason::Reconnected => Some(RequeueReason::NotStarted),
        RequeueReason::NotStarted => Some(RequeueReason::SessionSwitch),
        RequeueReason::SessionSwitch => Some(oom),
        RequeueReason::OutOfMemory { .. } => Some(RequeueReason::NodeMemoryPressure),
        RequeueReason::NodeMemoryPressure => Some(RequeueReason::Restarted),
        RequeueReason::Restarted => Some(RequeueReason::Lost(LostWhy::DaemonRestarted)),
        RequeueReason::Lost(LostWhy::DaemonRestarted) => Some(RequeueReason::Lost(LostWhy::Fenced)),
        RequeueReason::Lost(LostWhy::Fenced) => Some(RequeueReason::Infra),
        RequeueReason::Infra => None,
    })
}

/// Whether the store takes rows for `effect`. A match with an arm for every effect.
fn writes_rows(effect: &Effect) -> bool {
    match effect {
        Effect::Start(_) | Effect::Answer(_) | Effect::Refuse(_) => true,
        Effect::Commit(_) | Effect::Waiting(_) => false,
    }
}

/// A second store, for the values the first leaves out: an operation for each `Qos`, the
/// kinds of lease taking turns (so each kind has one, not each pair); an answer for each `Failure`; a refusal; and a cordon on `node-a`,
/// which has a session.
fn written_variants() -> Arc<Fake> {
    let (store, durable) = opened();
    let cordon = durable.cordoned(&WorkerId::new("node-a"), Some(StoredCordon::Cordoned));
    cordon.expect("a cordon");
    let mut count = 0_u64;
    let mut next = || {
        let n = count;
        count += 1;
        (4 + n, n, 20 + u8::try_from(n).expect("small"))
    };

    // Held on their leases: the `op/` and `lease/` rows stay.
    let kinds = every_lease_kind().into_iter().cycle();
    for (qos, kind) in every_qos().into_iter().zip(kinds) {
        let (operation, seq, tag) = next();
        let held = Request {
            qos,
            kind,
            ..request(tag)
        };
        granted_request(&durable, operation, seq, &held, None, None);
    }

    // Finished with each failure: the `done/` row is what the callers are told.
    let fate = fate();
    for failure in every_failure() {
        let (operation, seq, tag) = next();
        granted(&durable, operation, seq, tag, None);
        let answer = Answer {
            outcome: Outcome::Failed(failure),
            memory_runs: fate.memory_runs.clone(),
            farm_runs: fate.farm_runs.clone(),
            ..answer(operation, seq)
        };
        let done = ending(finished_of(&answer, None));
        let persisted = durable.persist(&[Effect::Answer(answer)], Some(&done));
        persisted.expect("a failure");
    }

    // Refused while queued.
    let (operation, _, tag) = next();
    let submitted = durable.submit_keyed(
        OperationId(operation),
        &request(tag),
        &subject(),
        None,
        None,
    );
    submitted.expect("submit");
    let refusal = Refusal {
        operation: OperationId(operation),
        waiters: Vec::new(),
        reason: "no node can run it".to_owned(),
    };
    let done = ending(refused_as(&refusal));
    let persisted = durable.persist(&[Effect::Refuse(refusal)], Some(&done));
    persisted.expect("a refusal");
    durable.ping().expect("a ping");
    store
}

/// Catches: a variant of `Qos`, `LeaseKind` or `Failure`, or a refusal, that the store
/// writes differently from another, or stops writing; and a cordon row that depends on the
/// node having no session.
#[test]
fn the_variants_store_holds_a_row_for_each_variant() {
    let store = written_variants();
    let rows = rendered(&store);
    let count = |prefix: &str| rows.iter().filter(|row| row.starts_with(prefix)).count();
    let held = every_qos().len();
    assert_eq!(count("op/"), held, "an op/ row for each Qos");
    assert_eq!(
        count("lease/"),
        held,
        "a lease/ row for each held operation"
    );
    let done = every_failure().len() + 1;
    assert_eq!(
        count("done/"),
        done,
        "a done/ row for each failure and the refusal"
    );
    assert_eq!(count("cordon/node-a"), 1);
    assert_eq!(count("session/node-a"), 1);
}

/// Catches: a give-up that writes something of its reason, so that two reasons leave
/// different rows. The reason decides what the caller passes (the loss count, the floors);
/// given the same loss count and floors (none here), the rows are the same for every reason.
/// The golden files hold the rows those parameters make.
#[test]
fn a_requeue_reason_is_not_a_row() {
    let rows = |reason| {
        let (store, durable) = opened();
        granted(&durable, 4, 0, 1, None);
        let requeue = Requeue {
            reason,
            ..requeue(4, 0)
        };
        let floors = FloorChange::default();
        let requeued = durable.requeued_named(&requeue, None, None, &floors, None);
        requeued.expect("a give-up");
        rendered(&store)
    };
    let reasons = every_requeue_reason();
    let first = rows(reasons[0]);
    assert!(first.iter().any(|row| row.starts_with("queue/")));
    for reason in reasons {
        assert_eq!(rows(reason), first, "{reason}");
    }
}

/// Catches: a grant that writes the fence policy or the device of the start into the store,
/// which would be a row the golden files do not pin.
#[test]
fn a_start_writes_the_grant_and_nothing_of_it() {
    let rows = |fence, device: Option<&str>| {
        let (store, durable) = opened();
        let submitted = durable.submit_keyed(OperationId(4), &request(1), &subject(), None, None);
        submitted.expect("submit");
        let start = StartLease {
            fence,
            device: device.map(str::to_owned),
            ..start(4, 0, 1)
        };
        durable
            .persist(&[Effect::Start(start)], None)
            .expect("grant");
        rendered(&store)
    };
    let first = rows(FencePolicy::RunOn, None);
    assert!(first.iter().any(|row| row.starts_with("lease/")));
    for fence in every_fence() {
        for device in [None, Some("00008030-001C195A3A38802E")] {
            assert_eq!(rows(fence, device), first, "{fence:?} {device:?}");
        }
    }
}

/// Catches: an effect that carries no row but writes one anyway, or that needs an ending.
#[test]
fn an_effect_that_carries_no_row_writes_none() {
    let (store, durable) = opened();
    let before = rendered(&store);
    let effects = [
        Effect::Commit(ControlRecord::Refusal(RefusalRecord {
            operation: OperationId(4),
            reason: "r".to_owned(),
        })),
        Effect::Waiting(Waiting {
            operation: OperationId(4),
            reason: None,
        }),
    ];
    assert!(effects.iter().all(|effect| !writes_rows(effect)));
    durable.persist(&effects, None).expect("nothing to write");
    assert_eq!(rendered(&store), before);
    let carried = [
        Effect::Start(start(4, 0, 1)),
        Effect::Answer(answer(4, 0)),
        Effect::Refuse(Refusal {
            operation: OperationId(4),
            waiters: Vec::new(),
            reason: "r".to_owned(),
        }),
    ];
    assert!(carried.iter().all(writes_rows));
}

/// Fills `scheduler-variants.txt` with the rows of the variants store when the file holds
/// only its comment lines. A release-time step: it never replaces a row.
#[test]
#[ignore = "writes the rows of a file that has none; run by a person with the scenario"]
fn write_variant_rows() {
    if !lines_of(VARIANTS).is_empty() {
        return;
    }
    let mut text: String = VARIANTS
        .lines()
        .filter(|line| line.starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    for line in rendered(&written_variants()) {
        text.push_str(&line);
        text.push('\n');
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/farm-rows/scheduler-variants.txt");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

fn eight(key: &str, row: &[u8]) -> u64 {
    <[u8; 8]>::try_from(row)
        .map(u64::from_be_bytes)
        .unwrap_or_else(|_| panic!("{key}: not 8 bytes"))
}

/// Catches: a reader and a writer of a row that no longer agree: each row of the file is read
/// by the reader of its kind and written again, and the bytes must be the same.
#[test]
fn every_row_of_the_file_reads_and_writes_back_the_same() {
    for line in golden_lines() {
        let (key, row) = split(&line);
        let kind = KINDS
            .iter()
            .find(|kind| key.starts_with(**kind))
            .expect("a known kind");
        match *kind {
            OPERATION | LEASE | SESSION => {
                let input = decode(key, &row).expect("a scheduler input");
                assert_eq!(encode(input).expect("encodes"), row, "{key}");
            }
            QUEUE | ALIVE => assert!(row.is_empty(), "{key}"),
            leases::LEASE_BY_OP | owner::OWNER => {
                let (high, low) = pair_of(&row).expect("two numbers");
                assert_eq!(pair_row(high, low), row[..], "{key}");
            }
            CORDON => {
                let cordon = StoredCordon::from_row(key, &row).expect("a cordon");
                assert_eq!(cordon.to_row(), row, "{key}");
            }
            DONE => {
                let finished = Finished::from_row(&row).expect("a result");
                assert_eq!(finished.to_row(), row, "{key}");
            }
            named::DONE_OF => {
                let action = named::decode(&row).expect("an action");
                assert_eq!(named::encode(&action), row, "{key}");
            }
            FATE => {
                let fate = fate::decode(&row).expect("a fate");
                assert_eq!(fate::encode(&fate), Some(row), "{key}");
            }
            FLOOR => {
                let floor = floor::decode(&row).expect("a floor");
                assert_eq!(floor::row_key(&floor.key), key);
                assert_eq!(floor::encode(&floor), Some(row), "{key}");
            }
            KEY_FLOOR => {
                let (floor, summary) = floor::decode_key(&row).expect("a key floor");
                assert_eq!(floor::key_row(floor.key), key);
                assert_eq!(floor::encode_key(&floor, summary.as_deref()), row, "{key}");
            }
            OPKEY => {
                let size = opkey::decode(&row).expect("a size key");
                assert_eq!(opkey::encode(size), row, "{key}");
            }
            OPWALL => {
                let wall = opwall::decode(&row).expect("a wall time");
                assert_eq!(opwall::encode(wall), row, "{key}");
            }
            SUBJECT => {
                let subject = subject::decode(&row).expect("a subject");
                assert_eq!(subject::encode(&subject), row, "{key}");
            }
            ESTIMATE => {
                let (named, summary) = estimate::decode(key, &row).expect("a size summary");
                assert_eq!(estimate::row_key(&named), key);
                assert_eq!(estimate::encode(&summary), row, "{key}");
                let read = kbf_estimator::Summary::decode(&summary).expect("a summary");
                assert_eq!(read.encode(), summary, "{key}");
            }
            LOSS | inflight::INFLIGHT | NEXT_OP | NEXT_SEQ => {
                assert_eq!(eight(key, &row).to_be_bytes(), row[..], "{key}");
            }
            EPOCH => assert_eq!(row.len(), 8, "{key}"),
            GENERATION => assert_eq!(row, kbf_state::SHARED_WRITERS, "{key}"),
            other => panic!("{key}: no reader named for {other}"),
        }
    }
}

const SUMMARY_V1: &str = include_str!("../../../tests/golden/farm-rows/size-summary-v1.txt");

/// Catches: a release that cannot read the `est/` row the release before it wrote (the
/// estimator's summary changed its stored form to version 2, with a minute ring more), and a
/// row read but written back as something that does not read: the row of `size-summary-v1.txt`
/// is read as the summary it holds, and what it is written back as reads as the same summary.
#[test]
fn a_version_1_size_summary_row_still_reads() {
    let lines = lines_of(SUMMARY_V1);
    assert_eq!(lines.len(), 1);
    let (key, row) = split(&lines[0]);
    let (named, summary) = estimate::decode(key, &row).expect("a version 1 row");
    assert_eq!(named, size_key());
    let read = kbf_estimator::Summary::decode(&summary).expect("a version 1 summary");
    let again = read.encode();
    assert_eq!(again.len(), summary.len() + 32, "the minute ring is added");
    assert_eq!(kbf_estimator::Summary::decode(&again), Some(read));
}

/// The same scenarios through `js-store`.
mod via_js_store;
