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
//! The rows of [`corpus`] are the entries this build reads as a value: the metadata commands
//! (`meta-`), the bootstrap and the machine version. The files in [`FARM_FILES`] are the
//! farm commands (a batch of the farm's inputs, a report, a takeover, a checkpoint) that
//! earlier builds wrote: this build does not read them as values, and each must decode as
//! [`LogCommand::Unknown`], so a log that holds them still replays. That the corpus is
//! append-only is held by review: the test of the directory fails on a file added or deleted
//! without its row, and not on a file and its row edited together.
//!
//! A row without a file is written by `cargo test -p kbf-log --test golden_corpus -- --ignored
//! write_missing_files`, which never replaces a file; commit the files it writes with the row.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use kbf_log::{Bootstrap, FarmLogId, Format, LogCommand, MachineVersion, decode, encode};
use kbf_meta::{Command, Epoch, Generation, NodeWriter, ObjectId, ServerId, StagedPut, TierId};
use kbf_types::FarmTime;

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
/// The files of the farm commands earlier builds wrote, each of which decodes as
/// [`LogCommand::Unknown`].
const FARM_FILES: [&str; 29] = [
    "checkpoint",
    "farm-capacity",
    "farm-committed-lease",
    "farm-committed-refusal",
    "farm-committed-result",
    "farm-cordon",
    "farm-disconnected",
    "farm-drain",
    "farm-heartbeat",
    "farm-started",
    "farm-submit-ci",
    "farm-submit-custom",
    "farm-submit-device",
    "farm-submit-interactive",
    "farm-submit",
    "farm-tick",
    "farm-uncordon",
    "farm-worker-up-predecessor-stopped",
    "farm-worker-up",
    "report-completed",
    "report-failed-infra",
    "report-failed-invalid",
    "report-failed-lost-daemon-restarted",
    "report-failed-lost-fenced",
    "report-failed-missing-input",
    "report-failed-node-memory-pressure",
    "report-failed-out-of-memory",
    "report-failed",
    "takeover",
];

/// The rows of the corpus: the name of a file (without `.hex`) and the value its bytes decode
/// to.
fn corpus() -> Vec<(&'static str, LogCommand)> {
    let bootstrap = LogCommand::Bootstrap(Bootstrap {
        log_id: FarmLogId::new(nz(0x0123_4567_89ab_cdef)),
        store_prefix: "kbf/0123456789abcdef/".to_owned(),
    });
    let version = LogCommand::MachineVersion(MachineVersion::V2);
    let mut rows = meta_rows();
    rows.extend([("bootstrap", bootstrap), ("machine-version", version)]);
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

/// Asserts that every file of a farm command still decodes, under each format this build
/// knows, as [`LogCommand::Unknown`]: the entry is skipped as a whole and not refused.
#[test]
fn every_farm_golden_entry_decodes_as_unknown() {
    for name in FARM_FILES {
        let bytes = entry_bytes(name);
        for n in 1..=5 {
            let read = decode(Format::new(n), &bytes);
            assert_eq!(read, Ok(LogCommand::Unknown), "{name} under format {n}");
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

/// Asserts that the golden directory holds a file for every row of the corpus and every name
/// of [`FARM_FILES`] and no other file: a file added without its row, and a file deleted, each
/// fail here.
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
        .chain(FARM_FILES.iter().map(|name| format!("{name}.hex")))
        .collect();
    named.sort();
    assert_eq!(found, named);
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
