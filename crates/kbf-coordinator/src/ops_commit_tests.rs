//! A change commits the ops file and its halts file together ([`crate::ops_file`]): the file
//! system is made to fail at each step of a clear, and what memory holds and what a restart
//! reads are always the halt standing with its old generation, or the clear with its
//! acknowledgement and its new generation, never one with the other. The halts file's reader
//! passes over a record it cannot read, counted, and reads the rest.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_testkit::dir::TestDir;
use kbf_updater::wire::{Outcome, Phase, Token, UpdateResult};

use crate::ops::{AuditEntry, Halt, HaltAck, OpsError, OpsState, OpsStore, ServiceOps, Write};
use crate::ops_file::{self, Step, halts_path, staged_path};
use crate::testutil::version;

fn halted_update(target: &str) -> UpdateResult {
    let mut report = UpdateResult::new(version(target), Outcome::Failed);
    report.phase = Some(Phase::Halt);
    report.reason = Token::parse("tainted");
    report
}

/// The kbf service halted by `s1` on 2.0, with one clear counted.
fn halted() -> ServiceOps {
    ServiceOps {
        halt: Some(Halt {
            node_id: "s1".to_owned(),
            check: "tainted".to_owned(),
            target: version("2.0"),
            report: Some(halted_update("2.0")),
        }),
        halt_generation: 1,
        ..ServiceOps::default()
    }
}

/// [`halted`], cleared: `s1`'s report acknowledged and two clears counted.
fn cleared() -> ServiceOps {
    let mut marks = halted();
    let halt = marks.halt.take().expect("halted");
    marks.halt_acked.push(HaltAck::from(halt));
    marks.halt_generation = 2;
    marks
}

/// Which steps of a write fail.
#[derive(Default)]
struct Faults {
    replaced: AtomicBool,
    read_back: AtomicBool,
    discard: AtomicBool,
    publish: AtomicBool,
}

impl Faults {
    fn at(&self, step: Step) -> io::Result<()> {
        let flag = match step {
            Step::OpsReplaced => &self.replaced,
            Step::ReadBack => &self.read_back,
            Step::Discard => &self.discard,
            Step::Publish => &self.publish,
        };
        if flag.load(Ordering::SeqCst) {
            Err(io::Error::other(format!("{step:?} fails")))
        } else {
            Ok(())
        }
    }
}

/// A directory whose ops file holds [`halted`], and a store over it whose writes fail where
/// the returned faults say.
fn halted_store(name: &str) -> (TestDir, PathBuf, OpsStore, Arc<Faults>) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().join("ops.json");
    OpsStore::open(&path)
        .expect("a store")
        .change(
            Write::BeforeMemory,
            AuditEntry::new("operator", "seed"),
            |state| {
                state.services.insert("kbf".to_owned(), halted());
            },
        )
        .expect("seeded");
    let faults = Arc::new(Faults::default());
    let (at, fail) = (path.clone(), Arc::clone(&faults));
    let state = OpsStore::open(&path).expect("read").state();
    let store = OpsStore::with_writer(
        state,
        Box::new(move |state| ops_file::write_with(&at, state, &|step| fail.at(step))),
    );
    (dir, path, store, faults)
}

/// The clear of [`halted`].
fn clear(store: &OpsStore) -> Result<AuditEntry, OpsError> {
    store.change(
        Write::BeforeMemory,
        AuditEntry::new("operator", "clear_halt"),
        |state| *state.services.get_mut("kbf").expect("kbf") = cleared(),
    )
}

/// The kbf marks a restart reads.
fn restarted(path: &Path) -> ServiceOps {
    OpsStore::open(path).expect("a restart reads").marks("kbf")
}

/// Makes replacing the ops file fail before its rename.
fn block_ops(path: &Path) {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".new");
    fs::create_dir(temp).expect("blocked");
}

/// Catches: a clear whose ops file was replaced, but whose replacing answered an error (the
/// directory's fsync after the rename), answered as failed while the disk holds it, or with its
/// halts file left behind: the clear stands, in memory and after a restart, with its
/// acknowledgement's report and its generation.
#[test]
fn a_clear_whose_ops_file_answered_an_error_after_holding_it_stands() {
    let (_dir, path, store, faults) = halted_store("ops-commit-after-rename");
    faults.replaced.store(true, Ordering::SeqCst);
    clear(&store).expect("the clear stands");
    assert_eq!(store.marks("kbf"), cleared());
    assert_eq!(restarted(&path), cleared());
    assert!(!staged_path(&path).exists());
}

/// Catches: a clear whose ops file was not replaced lifting the halt on disk all the same, or
/// publishing its generation: the clear fails, memory and a restart keep the halt and the old
/// generation, and the staged halts file is removed.
#[test]
fn a_clear_whose_ops_file_was_not_replaced_leaves_the_halt_and_its_generation() {
    let (_dir, path, store, _) = halted_store("ops-commit-not-replaced");
    let before = fs::read(halts_path(&path)).expect("the halts file");
    block_ops(&path);
    assert!(matches!(clear(&store), Err(OpsError::Io(_))));
    assert_eq!(store.marks("kbf"), halted());
    assert!(!staged_path(&path).exists());
    assert_eq!(fs::read(halts_path(&path)).expect("the halts file"), before);
    assert_eq!(restarted(&path), halted());
}

/// Catches: the staged halts file of a failed clear, when it cannot be removed, renamed in at
/// the next start (its generation published with no clear): the start removes it, and reads
/// the halt and the old generation.
#[test]
fn a_failed_clear_whose_staged_file_stays_is_removed_at_the_next_start() {
    let (_dir, path, store, faults) = halted_store("ops-commit-discard");
    block_ops(&path);
    faults.discard.store(true, Ordering::SeqCst);
    assert!(clear(&store).is_err());
    assert!(staged_path(&path).exists());
    assert_eq!(restarted(&path), halted());
    assert!(!staged_path(&path).exists());
}

/// Catches: a committed clear whose halts file could not be renamed in read at the next start
/// with the halts file before it (the acknowledgement without its report, the old
/// generation): the start renames the staged file in.
#[test]
fn a_committed_clear_whose_halts_file_was_not_renamed_in_is_renamed_in_at_the_next_start() {
    let (_dir, path, store, faults) = halted_store("ops-commit-publish");
    faults.publish.store(true, Ordering::SeqCst);
    clear(&store).expect("committed");
    assert!(staged_path(&path).exists());
    assert_eq!(restarted(&path), cleared());
    assert!(!staged_path(&path).exists());
}

/// Catches: the staged halts file of a committed clear thrown away by the next change when that
/// change fails before its commit, which leaves the clear in the ops file with the halts file
/// before it: a change first renames a committed staged file in.
#[test]
fn a_committed_clears_staged_file_is_renamed_in_before_the_next_change() {
    let (_dir, path, store, faults) = halted_store("ops-commit-settle-first");
    faults.publish.store(true, Ordering::SeqCst);
    clear(&store).expect("committed");
    faults.publish.store(false, Ordering::SeqCst);
    block_ops(&path);
    let next = store.change(
        Write::BeforeMemory,
        AuditEntry::new("operator", "exclude"),
        |_| {},
    );
    assert!(next.is_err());
    assert_eq!(restarted(&path), cleared());
}

/// Catches: a clear guessed made or not made when the ops file can be read back neither
/// after an error replacing it: the clear is answered as failed, the staged file is kept, and
/// the next start settles it by what the ops file holds (here the clear, whole).
#[test]
fn a_clear_whose_ops_file_cannot_be_read_back_is_settled_by_the_next_start() {
    let (_dir, path, store, faults) = halted_store("ops-commit-unknown");
    faults.replaced.store(true, Ordering::SeqCst);
    faults.read_back.store(true, Ordering::SeqCst);
    assert!(matches!(clear(&store), Err(OpsError::Io(_))));
    assert_eq!(store.marks("kbf"), halted());
    assert!(staged_path(&path).exists());
    assert_eq!(restarted(&path), cleared());
}

/// Catches: a start that cannot rename a committed staged file in reading the halts file
/// before it: the start is refused, naming the halts file.
#[test]
fn a_start_that_cannot_rename_a_committed_staged_file_in_is_refused() {
    let (_dir, path, store, faults) = halted_store("ops-commit-refused");
    faults.publish.store(true, Ordering::SeqCst);
    clear(&store).expect("committed");
    let halts = halts_path(&path);
    fs::remove_file(&halts).expect("removed");
    fs::create_dir(&halts).expect("a directory");
    fs::write(halts.join("in-the-way"), "").expect("not empty");
    match OpsStore::open(&path) {
        Err(why @ OpsError::Io(_)) => assert!(why.to_string().contains("ops.json.halts"), "{why}"),
        other => panic!("expected an I/O error, got {:?}", other.map(|s| s.state())),
    }
}

/// Catches: a staged file that cannot be read (here a directory at its name) taken for none:
/// the start is refused, naming it.
#[test]
fn a_staged_file_that_cannot_be_read_refuses_the_start() {
    let dir = TestDir::under(std::env::temp_dir(), "ops-commit-unreadable");
    let path = dir.path().join("ops.json");
    fs::create_dir(staged_path(&path)).expect("a directory");
    match OpsStore::open(&path) {
        Err(why @ OpsError::Io(_)) => assert!(why.to_string().contains(".halts.next"), "{why}"),
        other => panic!("expected an I/O error, got {:?}", other.map(|s| s.state())),
    }
}

/// Catches: a halts file a later release wrote with a report this release cannot read (an
/// outcome it does not know, a version it cannot parse) refusing the start (B8: an unknown
/// record is passed over and counted): those records are passed over and counted, and the
/// rest is read: here the acknowledgement reads as one from before reports, one report kept
/// while halted of two stays, and the halt's report and the generation stand.
#[test]
fn a_kept_report_this_release_cannot_read_is_passed_over_and_counted() {
    let dir = TestDir::under(std::env::temp_dir(), "ops-commit-unknown-outcome");
    let path = dir.path().join("ops.json");
    fs::write(
        &path,
        r#"{"schema":1,"services":{"kbf":{
            "halt":{"node_id":"s1","check":"tainted","target":"2.0"},
            "halt_acked":[{"node_id":"s4","target":"1.5"}]}}}"#,
    )
    .expect("written");
    let report = |target: &str, outcome: &str| serde_json::json!({"target": target, "outcome": outcome, "phase": "halt"});
    let halts = serde_json::json!({"services": {"kbf": {
        "halt": {"node_id": "s1", "target": "2.0", "report": report("2.0", "failed")},
        "halt_acked": [{"node_id": "s4", "target": "1.5", "report": report("1.5", "rolled_forward")}],
        "halt_seen": [
            {"node_id": "s2", "target": "2.0", "report": report("2.0", "failed")},
            {"node_id": "s3", "target": "not a version", "report": report("2.0", "failed")},
        ],
        "halt_generation": 3,
    }}});
    fs::write(halts_path(&path), halts.to_string()).expect("written");
    let read = OpsStore::open(&path).expect("read").state();
    let marks = &read.services["kbf"];
    assert!(marks.halt.as_ref().is_some_and(|h| h.report.is_some()));
    assert_eq!(marks.halt_acked[0].report, None);
    let seen: Vec<&str> = marks.halt_seen.iter().map(|a| a.node_id.as_str()).collect();
    assert_eq!(seen, ["s2"]);
    assert_eq!(marks.halt_generation, 3);
    let (state, skipped) = ops_file::joined(&path, read.clone()).expect("joined");
    assert_eq!(skipped, 2);
    assert_eq!(state, read);
}

/// Pins: a halts file with nothing this release cannot read counts nothing passed over.
#[test]
fn a_halts_file_this_release_reads_whole_counts_nothing_passed_over() {
    let (_dir, path, store, _) = halted_store("ops-commit-none-skipped");
    let (_, skipped) = ops_file::joined(&path, OpsState::default()).expect("joined");
    assert_eq!(skipped, 0);
    assert_eq!(store.marks("kbf"), halted());
}

/// Catches: the first change of a store, when its ops file cannot be written, read as made or
/// as not known because there is no ops file to read back: it fails, and leaves no ops file
/// and no staged halts file.
#[test]
fn a_first_change_whose_ops_file_cannot_be_written_leaves_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "ops-commit-first");
    let path = dir.path().join("ops.json");
    let store = OpsStore::open(&path).expect("opened");
    block_ops(&path);
    let raised = store.change(
        Write::BeforeMemory,
        AuditEntry::new("operator", "seed"),
        |state| {
            state.services.insert("kbf".to_owned(), halted());
        },
    );
    assert!(matches!(raised, Err(OpsError::Io(_))), "{raised:?}");
    assert!(!path.exists());
    assert!(!staged_path(&path).exists());
    assert!(!halts_path(&path).exists());
}

/// Catches: the reports kept while a halt stood dropped with the halt's own report when only
/// that report cannot be read (they joined to the halt through it): the halt reads with no
/// report, one record passed over, and the reports kept while it stood are kept for the clear.
#[test]
fn reports_kept_while_halted_join_to_a_halt_whose_report_cannot_be_read() {
    let dir = TestDir::under(std::env::temp_dir(), "ops-commit-unknown-halt");
    let path = dir.path().join("ops.json");
    fs::write(
        &path,
        r#"{"schema":1,"services":{"kbf":{
            "halt":{"node_id":"s1","check":"tainted","target":"2.0"}}}}"#,
    )
    .expect("written");
    let report =
        |outcome: &str| serde_json::json!({"target": "2.0", "outcome": outcome, "phase": "halt"});
    let halts = serde_json::json!({"services": {"kbf": {
        "halt": {"node_id": "s1", "target": "2.0", "report": report("rolled_forward")},
        "halt_seen": [{"node_id": "s2", "target": "2.0", "report": report("failed")}],
    }}});
    fs::write(halts_path(&path), halts.to_string()).expect("written");
    let read = OpsStore::open(&path).expect("read").state();
    let (_, skipped) = ops_file::joined(&path, OpsState::default()).expect("joined");
    assert_eq!(skipped, 1);
    let marks = &read.services["kbf"];
    assert_eq!(marks.halt.as_ref().map(|h| h.report.clone()), Some(None));
    let seen: Vec<&str> = marks.halt_seen.iter().map(|a| a.node_id.as_str()).collect();
    assert_eq!(seen, ["s2"]);
}

/// Catches: a kept report joined by host alone, or by version alone: when the ops file's halt
/// and acknowledgement are of the same hosts as the halts file's records but of other versions
/// (or of the same version and another host), nothing joins: the
/// halt and the acknowledgement read with no report, and the reports kept while the other halt
/// stood are dropped.
#[test]
fn a_kept_report_of_the_same_host_and_another_version_joins_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "ops-commit-other-version");
    let path = dir.path().join("ops.json");
    fs::write(
        &path,
        r#"{"schema":1,"services":{"kbf":{
            "halt":{"node_id":"s1","check":"tainted","target":"2.0"},
            "halt_acked":[{"node_id":"s4","target":"1.5"}]}}}"#,
    )
    .expect("written");
    let report =
        |target: &str| serde_json::json!({"target": target, "outcome": "failed", "phase": "halt"});
    let halts = serde_json::json!({"services": {"kbf": {
        "halt": {"node_id": "s1", "target": "2.1", "report": report("2.1")},
        "halt_acked": [
            {"node_id": "s5", "target": "1.5", "report": report("1.5")},
            {"node_id": "s4", "target": "1.6", "report": report("1.6")},
        ],
        "halt_seen": [{"node_id": "s2", "target": "2.1", "report": report("2.1")}],
    }}});
    fs::write(halts_path(&path), halts.to_string()).expect("written");
    let marks = OpsStore::open(&path).expect("read").marks("kbf");
    assert_eq!(marks.halt.map(|h| h.report), Some(None));
    assert_eq!(marks.halt_acked[0].report, None);
    assert_eq!(marks.halt_seen, []);
}
