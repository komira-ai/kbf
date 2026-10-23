//! The golden corpus: entries of the replicated log as a build wrote them, kept as files in
//! `tests/golden/`. Every later build must read them as the values they were written from, so
//! a build that stops reading today's entries fails here before it ships (a release has to
//! start on the data the release before it wrote: docs/design/kbf-cicd.md, section 9).
//!
//! The corpus is append-only. A release that adds a kind of entry adds a file with an entry of
//! that kind and a row in [`corpus`]. A file that exists is never rewritten, not even to match
//! a new encoding, and never deleted: its bytes are what an earlier build wrote, and they stay
//! what every later build must read. Each file holds the bytes of one entry as hex text, and
//! each entry is read under each format this build knows, 1 to 5 (`decode` reads an entry the
//! same under all of them).
//!
//! The corpus holds the kinds of entry the rows of [`corpus`] name, not every kind: the
//! encoding of the others is pinned by the inline bytes of `meta_codec.rs` and
//! `control_golden.rs`, which are tests and not files. That the corpus is append-only is held
//! by review: the test of the directory fails on a file added or deleted without its row, and
//! not on a file and its row edited together.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use kbf_log::{
    Bootstrap, FarmBatch, FarmInput, FarmLogId, Format, LogCommand, MachineVersion, Report,
    Takeover, decode,
};
use kbf_meta::{Command, Epoch, Generation, NodeWriter, ObjectId, ServerId, StagedPut, TierId};
use kbf_types::{Failure, FarmTime, LeaseId, OperationId, Outcome};

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

/// The rows of the corpus: the name of a file (without `.hex`) and the value its bytes decode
/// to.
fn corpus() -> Vec<(&'static str, LogCommand)> {
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
    let report = LogCommand::Report(Report {
        now: FarmTime::from_millis(1),
        operation: OperationId(3),
        lease: LeaseId::new(1, 2),
        outcome: Outcome::Failed(Failure::Timeout),
        action_entry: None,
    });
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
        ("bootstrap", bootstrap),
        ("machine-version", version),
        ("takeover", takeover),
        ("farm-tick", farm),
        ("report-failed", report),
    ]
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// The bytes in the file `name`.hex of the golden directory.
fn entry_bytes(name: &str) -> Vec<u8> {
    let path = golden_dir().join(format!("{name}.hex"));
    let text = std::fs::read_to_string(&path);
    let text = text.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    hex::decode(text.trim()).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
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
