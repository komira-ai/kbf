//! The golden corpus: entries of the replicated log as a build wrote them, kept as files in
//! `tests/golden/`. Every later build must read them as the values they were written from, so
//! a build that stops reading today's entries fails here before it ships (a release has to
//! start on the data the release before it wrote: docs/design/kbf-cicd.md, section 9).
//!
//! The corpus is append-only. A release that adds a kind of entry adds a file with an entry of
//! that kind and a row in [`corpus`]. A file that exists is never rewritten, not even to match
//! a new encoding, and never deleted: its bytes are what an earlier build wrote, and they stay
//! what every later build must read. Each file holds the bytes of one entry as hex text, after
//! comment lines (beginning with `#`) that say what the file pins; and each entry is read
//! under each format this build knows, 1 to 5 (`decode` reads an entry the same under all of
//! them).
//!
//! The corpus holds every kind of entry the scheduler writes to the log: each command that is
//! not a metadata command, and each input a batch of the farm carries (the worker's
//! registration and capacity, the heartbeat, a submission, a started lease, the committed
//! lease, result and refusal, and the placement of a node). The test of the kinds fails when
//! one is added without a row. The values the entries carry are held the same way: a
//! submission for each QoS level and each kind of lease, and a report for each `Failure`
//! (and so each `LostWhy`), each listed by a chain written as an exhaustive match that does
//! not compile when a variant is added to the type. Not entries of the log, so not here: the
//! reason a lease is given up (`RequeueReason`), the fence policy and the device of a start
//! (the grant the log holds names the lease, the operation and the worker, and nothing else). The metadata commands are pinned in part (the rows named
//! `meta-`) and in part by the inline bytes of `meta_codec.rs`, which are tests and not
//! files. That the corpus is append-only is held by review: the test of the directory fails
//! on a file added or deleted without its row, and not on a file and its row edited together.
//!
//! A row without a file is written by `cargo test -p kbf-log --test golden_corpus -- --ignored
//! write_missing_files`, which never replaces a file; commit the files it writes with the row.

mod control_support;

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use control_support::{batch, caps, d, lease, resources, w};
use kbf_log::{
    ActionEntry, Bootstrap, Checkpoint, FarmBatch, FarmInput, FarmLogId, Format, LogCommand,
    MachineVersion, Report, Takeover, decode, encode,
};
use kbf_meta::{
    ActionRecord, Closure, Command, Epoch, Generation, NodeWriter, ObjectId, ServerId, StagedPut,
    TierId,
};
use kbf_sched::{DaemonInstance, Request};
use kbf_types::{
    ActionKey, ControlRecord, Failure, FarmTime, LeaseGrant, LeaseId, LeaseKind, LostWhy,
    OperationId, Outcome, Qos, RefusalRecord, Resources, ResultRecord,
};

fn nz(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).expect("not zero")
}

fn object(epoch: u64, seq: u64) -> ObjectId {
    ObjectId::new(Epoch::new(epoch), seq)
}

fn staged(seq: u64) -> StagedPut {
    StagedPut {
        object: object(2, seq),
        end: 300,
        seal: true,
        blobs: Vec::new(),
    }
}

/// An execution request for instance `i`, action `d(3)`, of CPU 1 and memory 2, needing
/// what `needs` parses from.
fn request<const N: usize>(qos: Qos, kind: LeaseKind, needs: [(&str, &str); N]) -> Request {
    Request {
        key: ActionKey {
            instance: "i".to_owned(),
            action: d(3),
        }
        .into(),
        qos,
        kind,
        resources: Resources::new(1, 2),
        needs: kbf_caps::Request::parse(needs).expect("a request"),
        hermetic: true,
        do_not_cache: true,
        whole_worker: false,
    }
}

/// The rows of the metadata commands of the corpus.
fn meta_rows() -> Vec<(&'static str, LogCommand)> {
    let meta = |name, command| (name, LogCommand::Meta(command));
    let tick = Command::Tick(FarmTime::from_millis(1500));
    let reachable = Command::ObjectReachable {
        object: object(2, 9),
        generation: Generation::new(300),
    };
    let add_tier = Command::AddTier {
        server: ServerId::new(3),
        tier: TierId::new(7),
    };
    let evict = Command::EvictObjects {
        objects: [object(2, 9), object(3, 1)].into(),
    };
    let set_writer = Command::SetWriter(NodeWriter::new(nz(7), nz(2)));
    let reseat = Command::ReseatStaged {
        tier: TierId::new(2),
        from: object(2, 9),
        to: staged(11),
        stored: false,
    };
    vec![
        meta("meta-tick", tick),
        meta("meta-alloc-epoch", Command::AllocEpoch),
        meta("meta-collect", Command::Collect),
        meta("meta-object-reachable", reachable),
        meta("meta-add-tier", add_tier),
        meta("meta-evict-objects", evict),
        meta("meta-set-writer", set_writer),
        meta("meta-rebuild-done", Command::RebuildDone),
        meta("meta-reseat-staged", reseat),
    ]
}

/// The rows of the farm's inputs: one batch of one input each, at farm time 1 and wall time
/// 2 ns.
fn input_rows() -> Vec<(&'static str, LogCommand)> {
    let granted = LeaseGrant {
        lease: lease(1, 2),
        operation: OperationId(3),
        worker: w("w"),
        cost: Default::default(),
    };
    let result = ResultRecord {
        lease: lease(1, 2),
        operation: OperationId(3),
        outcome: Outcome::Failed(Failure::Timeout),
    };
    let refusal = RefusalRecord {
        operation: OperationId(3),
        reason: "r".to_owned(),
    };
    let up = |predecessor_stopped| FarmInput::WorkerUp {
        worker: w("w"),
        instance: DaemonInstance::new("i"),
        predecessor_stopped,
        capacity: resources(),
        caps: caps(),
    };
    let device = request(
        Qos::Batch,
        LeaseKind::Action,
        [
            ("arch", "arm64"),
            ("ios.device", "1"),
            ("ios.device.class", "c"),
        ],
    );
    let rows = [
        ("farm-worker-up", up(false)),
        ("farm-worker-up-predecessor-stopped", up(true)),
        (
            "farm-capacity",
            FarmInput::Capacity {
                worker: w("w"),
                capacity: resources(),
                caps: caps(),
            },
        ),
        (
            "farm-heartbeat",
            FarmInput::Heartbeat {
                worker: w("w"),
                running: vec![lease(1, 2), lease(1, 3)],
            },
        ),
        (
            "farm-submit",
            FarmInput::Submit(request(
                Qos::Batch,
                LeaseKind::WholeMachine,
                [("arch", "arm64")],
            )),
        ),
        ("farm-submit-device", FarmInput::Submit(device)),
        (
            "farm-started",
            FarmInput::Started {
                operation: OperationId(3),
                lease: lease(1, 2),
            },
        ),
        (
            "farm-committed-lease",
            FarmInput::Committed(ControlRecord::Lease(granted)),
        ),
        (
            "farm-committed-result",
            FarmInput::Committed(ControlRecord::Result(result)),
        ),
        (
            "farm-committed-refusal",
            FarmInput::Committed(ControlRecord::Refusal(refusal)),
        ),
        ("farm-cordon", FarmInput::Cordon(w("w"))),
        ("farm-uncordon", FarmInput::Uncordon(w("w"))),
        (
            "farm-drain",
            FarmInput::Drain {
                worker: w("w"),
                deadline: FarmTime::from_millis(5),
            },
        ),
        ("farm-disconnected", FarmInput::Disconnected(w("w"))),
    ];
    let mut rows: Vec<_> = rows
        .into_iter()
        .map(|(name, input)| (name, batch(input)))
        .collect();
    rows.extend(qos_rows());
    rows
}

/// The values of an enum: `first`, then what `next` makes of the last one, until it makes
/// none. `next` is a `match` with an arm for every variant, so a variant added to the enum
/// does not compile until it has a place in the chain, and then it needs a row.
fn chain<T>(first: T, next: impl Fn(&T) -> Option<T>) -> Vec<T> {
    let mut all = vec![first];
    while let Some(after) = next(&all[all.len() - 1]) {
        all.push(after);
    }
    all
}

/// A submission for each QoS level the rows above do not hold (`Batch` is in `farm-submit`
/// and `farm-submit-device`), with the kinds of lease they leave out (`Vm`).
fn qos_rows() -> Vec<(&'static str, LogCommand)> {
    let levels = chain(Qos::Interactive, |qos| match qos {
        Qos::Interactive => Some(Qos::Ci),
        Qos::Ci => Some(Qos::Batch),
        Qos::Batch => Some(Qos::custom("tenant-a", 150).expect("a level")),
        Qos::Custom(_) => None,
    });
    levels
        .into_iter()
        .filter_map(|qos| {
            let (name, kind) = match &qos {
                Qos::Interactive => ("farm-submit-interactive", LeaseKind::Action),
                Qos::Ci => ("farm-submit-ci", LeaseKind::Vm),
                Qos::Batch => return None,
                Qos::Custom(_) => ("farm-submit-custom", LeaseKind::Action),
            };
            let submit = FarmInput::Submit(request(qos, kind, []));
            Some((name, batch(submit)))
        })
        .collect()
}

/// A report for each `Failure`: the outcome of a lease that ended without a result.
fn report_rows() -> Vec<(&'static str, LogCommand)> {
    let failures = chain(Failure::Timeout, |failure| match failure {
        Failure::Timeout => Some(Failure::Infra),
        Failure::Infra => Some(Failure::Invalid),
        Failure::Invalid => Some(Failure::OutOfMemory),
        Failure::OutOfMemory => Some(Failure::NodeMemoryPressure),
        Failure::NodeMemoryPressure => Some(Failure::MissingInput),
        Failure::MissingInput => Some(Failure::Lost(LostWhy::DaemonRestarted)),
        Failure::Lost(LostWhy::DaemonRestarted) => Some(Failure::Lost(LostWhy::Fenced)),
        Failure::Lost(LostWhy::Fenced) => None,
    });
    failures
        .into_iter()
        .map(|failure| {
            let name = match failure {
                Failure::Timeout => "report-failed",
                Failure::Infra => "report-failed-infra",
                Failure::Invalid => "report-failed-invalid",
                Failure::OutOfMemory => "report-failed-out-of-memory",
                Failure::NodeMemoryPressure => "report-failed-node-memory-pressure",
                Failure::MissingInput => "report-failed-missing-input",
                Failure::Lost(LostWhy::DaemonRestarted) => "report-failed-lost-daemon-restarted",
                Failure::Lost(LostWhy::Fenced) => "report-failed-lost-fenced",
            };
            let report = LogCommand::Report(Report {
                now: FarmTime::from_millis(1),
                operation: OperationId(3),
                lease: LeaseId::new(1, 2),
                outcome: Outcome::Failed(failure),
                action_entry: None,
            });
            (name, report)
        })
        .collect()
}

/// The rows of the corpus: the name of a file (without `.hex`) and the value its bytes decode
/// to.
fn corpus() -> Vec<(&'static str, LogCommand)> {
    let bootstrap = LogCommand::Bootstrap(Bootstrap {
        log_id: FarmLogId::new(nz(0x0123_4567_89ab_cdef)),
        store_prefix: "kbf/0123456789abcdef/".to_owned(),
    });
    let version = LogCommand::MachineVersion(MachineVersion::V2);
    let takeover = LogCommand::Takeover(Takeover {
        term: nz(7),
        now: FarmTime::from_millis(9),
    });
    let farm = LogCommand::Farm(FarmBatch {
        now: FarmTime::from_millis(1),
        wall: UNIX_EPOCH + Duration::from_nanos(2),
        inputs: vec![FarmInput::Tick],
    });
    let completed = LogCommand::Report(Report {
        now: FarmTime::from_millis(1),
        operation: OperationId(3),
        lease: LeaseId::new(1, 2),
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
    });
    let checkpoint = LogCommand::Checkpoint(Checkpoint {
        index: 5,
        digest: [0xab; 32],
    });
    let mut rows = meta_rows();
    rows.extend([
        ("bootstrap", bootstrap),
        ("machine-version", version),
        ("takeover", takeover),
        ("farm-tick", farm),
        ("report-completed", completed),
        ("checkpoint", checkpoint),
    ]);
    rows.extend(report_rows());
    rows.extend(input_rows());
    rows
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// The bytes in the file `name`.hex of the golden directory, without its comment lines.
fn entry_bytes(name: &str) -> Vec<u8> {
    let path = golden_dir().join(format!("{name}.hex"));
    let text = std::fs::read_to_string(&path);
    let text = text.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let digits: String = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<String>();
    hex::decode(digits.trim()).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Asserts that every entry of the corpus decodes, under each format this build knows (1 to
/// 5), to the value it was written from. The files are append-only: a release that stops
/// reading one of them, or reads it as another value, fails here, and the way to fix it is
/// never to rewrite the file.
#[test]
fn every_golden_entry_decodes_to_the_value_it_was_written_from() {
    for (name, want) in corpus() {
        let bytes = entry_bytes(name);
        for n in 1..=5 {
            let read = decode(Format::new(n), &bytes);
            assert_eq!(read.as_ref(), Ok(&want), "{name} under format {n}");
        }
    }
}

/// Asserts that this build writes each entry of the corpus as the bytes of its file under
/// format 1, the format the files were written under: an encoder that changes the bytes of an
/// entry fails here, so a log written by this build is read by the build before it.
#[test]
fn every_golden_entry_is_written_as_the_bytes_of_its_file() {
    for (name, value) in corpus() {
        let written = encode(Format::V1, &value).expect("an encoding");
        assert_eq!(
            hex::encode(written),
            hex::encode(entry_bytes(name)),
            "{name}: the encoding changed"
        );
    }
}

/// Asserts that the golden directory holds a file for every row of the corpus and no other
/// file: a file added without the value it must decode to, and a file deleted, each fail here.
#[test]
fn the_golden_directory_holds_exactly_the_files_the_corpus_names() {
    let mut found: Vec<String> = std::fs::read_dir(golden_dir())
        .expect("the golden directory")
        .map(|entry| entry.expect("an entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    found.sort();
    let mut named: Vec<String> = corpus()
        .into_iter()
        .map(|(name, _)| format!("{name}.hex"))
        .collect();
    named.sort();
    assert_eq!(found, named);
}

/// Asserts that the corpus holds every kind of entry the scheduler writes: each command that
/// is not a metadata command, and each input of a farm batch. The matches are exhaustive, so
/// a kind added without a row fails to compile here or reds the counts.
#[test]
fn the_corpus_holds_every_kind_of_scheduler_entry() {
    let mut commands = [false; 6];
    let mut inputs = [false; 11];
    for (name, command) in corpus() {
        let command = match &command {
            LogCommand::Meta(_) => continue,
            LogCommand::Unknown => unreachable!("{name}: an unknown command has no bytes"),
            LogCommand::Bootstrap(_) => 0,
            LogCommand::Farm(batch) => {
                for input in &batch.inputs {
                    inputs[match input {
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
                    }] = true;
                }
                1
            }
            LogCommand::Report(_) => 2,
            LogCommand::Takeover(_) => 3,
            LogCommand::Checkpoint(_) => 4,
            LogCommand::MachineVersion(_) => 5,
        };
        commands[command] = true;
    }
    assert!(commands.iter().all(|seen| *seen), "{commands:?}");
    assert!(inputs.iter().all(|seen| *seen), "{inputs:?}");
}

/// The comment lines at the head of a file this test writes.
fn header(name: &str) -> String {
    format!(
        "# Log entry `{name}`: the bytes of one entry of the replicated log, in format 1.\n\
         # Pins: this value decodes from these bytes under every format, and encodes to them.\n\
         # A change to this file needs a reason in the pull request that makes it, and the\n\
         # file is never rewritten to match a new encoding: see tests/golden_corpus.rs.\n"
    )
}

/// Writes the file of each row that has none, with a header and the bytes of the entry in
/// format 1. A release-time step: it never replaces a file.
#[test]
#[ignore = "writes the files of rows that have none; run by a person with the row"]
fn write_missing_files() {
    for (name, value) in corpus() {
        let path = golden_dir().join(format!("{name}.hex"));
        if path.exists() {
            continue;
        }
        let bytes = encode(Format::V1, &value).expect("an encoding");
        let text = format!("{}{}\n", header(name), hex::encode(bytes));
        std::fs::write(&path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}
