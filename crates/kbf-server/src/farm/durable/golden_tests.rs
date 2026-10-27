//! The bytes of every row the scheduler writes to the state store, as a file.
//!
//! `tests/golden/farm-rows/scheduler-rows.txt` holds, for one store that a handle of the
//! scheduler wrote through its own calls, every row: its key, when the store expires it and
//! its value as hex. The calls cover each kind of row the scheduler writes (`op/`, `queue/`,
//! `lease/`, `lease-by-op/`, `session/`, `owner/`, `cordon/`, `done/`, `doneof/`, `loss/`,
//! `fate/`, `floor/`, `keyfloor/`, `opkey/`, `subject/`, `inflight/`, `est/` and the `meta/`
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

use std::fmt::Write as _;

use kbf_front::ActionLabel;
use kbf_proto::reapi::ActionResult;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Versioned};
use kbf_types::{
    ActionKey, Answer, Digest, DigestFunction, Failure, FarmFault, FarmRun, FencePolicy, LeaseKind,
    LostWhy, MemoryKill, MemoryRun, Outcome, Qos, Resources, StartLease,
};

use super::owner::{pair_of, pair_row};
use super::*;
use crate::action_usage::{ShapeKey, Target, TargetKind};

const GOLDEN: &str = include_str!("../../../tests/golden/farm-rows/scheduler-rows.txt");

/// The kinds of row the scheduler writes, as the start of their keys.
const KINDS: [&str; 22] = [
    "op/",
    "queue/",
    "lease/",
    "lease-by-op/",
    "session/",
    "owner/",
    "cordon/",
    "done/",
    "doneof/",
    "loss/",
    "fate/",
    "floor/",
    "keyfloor/",
    "opkey/",
    "subject/",
    "inflight/",
    "est/",
    "meta/alive",
    "meta/epoch",
    "meta/generation",
    "meta/next_op",
    "meta/next_seq",
];

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
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
    let submitted = durable.submit_keyed(OperationId(operation), &request(tag), &subject(), size);
    submitted.expect("submit");
    let effects = [Effect::Start(start(operation, seq, tag))];
    durable.persist(&effects, None).expect("grant");
}

/// A store written through the calls of the scheduler's own handle, one call for each row it
/// writes, in the order the scheduler makes them.
fn written() -> Arc<Fake> {
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

    // Operation 8 holds its lease.
    granted(&durable, 8, 4, 9, Some(7));

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

/// The lines of the golden file: its rows, without the comment lines.
/// A row too long for a line goes on in lines that begin with two spaces, which continue the
/// value of the row before them.
fn golden_lines() -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for line in GOLDEN.lines().filter(|l| !l.starts_with('#')) {
        match (line.strip_prefix("  "), rows.last_mut()) {
            (Some(more), Some(last)) => last.push_str(more),
            _ => rows.push(line.to_owned()),
        }
    }
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
    let store = written();
    let found = rendered(&store);
    let want = golden_lines();
    let keys = |lines: &[String]| -> Vec<String> {
        let first = |line: &String| line.split(' ').next().unwrap_or_default().to_owned();
        lines.iter().map(first).collect()
    };
    assert_eq!(keys(&found), keys(&want), "the keys of the rows");
    for (found, want) in found.iter().zip(&want) {
        assert_eq!(found, want);
    }
}

/// Catches: a file that lost a kind of row (the test above would then pass on a scenario
/// that no longer writes it).
#[test]
fn the_file_holds_every_kind_of_row() {
    let lines = golden_lines();
    for kind in KINDS {
        assert!(
            lines.iter().any(|line| line.starts_with(kind)),
            "no {kind} row in the file"
        );
    }
    for line in lines {
        assert!(
            KINDS.iter().any(|kind| line.starts_with(kind)),
            "a row of no known kind: {line}"
        );
    }
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
            "op/" | "lease/" | "session/" => {
                let input = decode(key, &row).expect("a scheduler input");
                assert_eq!(encode(input).expect("encodes"), row, "{key}");
            }
            "queue/" | "meta/alive" => assert!(row.is_empty(), "{key}"),
            "lease-by-op/" | "owner/" => {
                let (high, low) = pair_of(&row).expect("two numbers");
                assert_eq!(pair_row(high, low), row[..], "{key}");
            }
            "cordon/" => {
                let cordon = StoredCordon::from_row(key, &row).expect("a cordon");
                assert_eq!(cordon.to_row(), row, "{key}");
            }
            "done/" => {
                let finished = Finished::from_row(&row).expect("a result");
                assert_eq!(finished.to_row(), row, "{key}");
            }
            "doneof/" => {
                let action = named::decode(&row).expect("an action");
                assert_eq!(named::encode(&action), row, "{key}");
            }
            "fate/" => {
                let fate = fate::decode(&row).expect("a fate");
                assert_eq!(fate::encode(&fate), Some(row), "{key}");
            }
            "floor/" => {
                let floor = floor::decode(&row).expect("a floor");
                assert_eq!(floor::row_key(&floor.key), key);
                assert_eq!(floor::encode(&floor), Some(row), "{key}");
            }
            "keyfloor/" => {
                let (floor, summary) = floor::decode_key(&row).expect("a key floor");
                assert_eq!(floor::key_row(floor.key), key);
                assert_eq!(floor::encode_key(&floor, summary.as_deref()), row, "{key}");
            }
            "opkey/" => {
                let size = opkey::decode(&row).expect("a size key");
                assert_eq!(opkey::encode(size), row, "{key}");
            }
            "subject/" => {
                let subject = subject::decode(&row).expect("a subject");
                assert_eq!(subject::encode(&subject), row, "{key}");
            }
            "est/" => {
                let (named, summary) = estimate::decode(key, &row).expect("a size summary");
                assert_eq!(estimate::row_key(&named), key);
                assert_eq!(estimate::encode(&summary), row, "{key}");
                let read = kbf_estimator::Summary::decode(&summary).expect("a summary");
                assert_eq!(read.encode(), summary, "{key}");
            }
            "loss/" | "inflight/" | "meta/next_op" | "meta/next_seq" => {
                assert_eq!(eight(key, &row).to_be_bytes(), row[..], "{key}");
            }
            "meta/epoch" => assert_eq!(row.len(), 8, "{key}"),
            "meta/generation" => assert_eq!(row, kbf_state::SHARED_WRITERS, "{key}"),
            other => panic!("{key}: no reader named for {other}"),
        }
    }
}
