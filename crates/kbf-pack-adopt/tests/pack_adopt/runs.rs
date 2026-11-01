//! Runs of the tool over an S3 service on loopback holding real segments and an in-memory pack
//! index: what a dry run and an apply do, the files they keep, and how they end.

use std::io;
use std::path::Path;

use kbf_pack::{AdoptMode, AdoptReport};
use kbf_pack_adopt::main_with_seams;
use kbf_pack_index::{NewPack, PackIndex};

use crate::support::{BUCKET, Farm, SOCKET_URL, says, write_file};

/// The flags of an apply.
const APPLY: [&str; 4] = ["--mode", "apply", "--pack-store-bucket", BUCKET];

fn text(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 path")
}

fn read_report(path: &Path) -> AdoptReport {
    let bytes = std::fs::read(path).expect("the file exists");
    serde_json::from_slice(&bytes).expect("a report")
}

/// A pack of the index under `key` that is not what the tool would record for it.
fn other_pack(key: &str) -> NewPack {
    NewPack {
        key: key.to_owned(),
        writer: "someone-else".to_owned(),
        size: 1 << 20,
        blobs: Vec::new(),
    }
}

/// Asserts that a dry run records nothing in the index, prints what an apply would add, reads a
/// sample of blobs back, writes the report as JSON, and writes nothing to the object store.
#[test]
fn a_dry_run_records_nothing_and_reports_the_packs_an_apply_would_add() {
    let farm = Farm::new("runs-dry");
    let report = farm.path("report.json");
    let mut flags = vec!["--report-json", text(&report)];
    flags.extend(["--segments-per-second", "100"]);
    flags.extend(["--footer-concurrency", "1"]);

    let ran = farm.run(&flags);

    assert_eq!(ran.code, 0, "{}", ran.err);
    assert!(ran.err.is_empty());
    says(&ran.out, "packs: new 3, already recorded 0");
    says(&ran.out, "rows: new 5");
    says(&ran.out, "sample: 3 blobs read back, 0 did not match");
    let source = format!("{}/{BUCKET}", farm.s3.endpoint());
    says(&ran.out, &format!("source {source}"));
    for key in &farm.keys {
        assert_eq!(farm.index.pack(key).unwrap(), None);
    }
    let written = read_report(&report);
    assert_eq!(written.source, source);
    assert_eq!(written.mode, AdoptMode::DryRun);
    assert!(written.finished);
    assert_eq!(written.segments, 3);
    assert_eq!(written.packs_new, 3);
    assert_eq!(written.rows_new, 5);
    // The three segments the farm put are the only puts.
    assert_eq!(farm.s3.stats().single_puts, 3);
}

/// Asserts that an apply records each segment under its own key, that the reader then finds
/// every blob through the S3 service, that the tool wrote nothing to the object store, and that
/// a second apply adds nothing.
#[test]
fn an_apply_records_each_segment_where_it_lies_and_a_second_adds_nothing() {
    let farm = Farm::new("runs-apply");

    let first = farm.run(&APPLY);

    assert_eq!(first.code, 0, "{}", first.err);
    says(&first.out, "packs: new 3, already recorded 0");
    says(&first.out, "rows: new 5");
    for key in &farm.keys {
        let pack = farm.index.pack(key).unwrap().expect("recorded");
        assert_eq!(&pack.key, key);
    }
    assert_eq!(farm.read_all(), farm.found_all());
    assert_eq!(farm.s3.stats().single_puts, 3);
    assert_eq!(farm.s3.keys(), farm.keys);
    let second = farm.run(&APPLY);
    assert_eq!(second.code, 0, "{}", second.err);
    says(&second.out, "packs: new 0, already recorded 3");
    says(&second.out, "rows: new 0");
}

/// Asserts that a run stopped at its limit leaves a checkpoint file, that the same command line
/// without the limit goes on after the last key handled, and that once it has finished the same
/// command line runs again from the start.
#[test]
fn a_run_stopped_at_its_limit_goes_on_from_its_checkpoint_file() {
    let farm = Farm::new("runs-resume");
    let checkpoint = farm.path("checkpoint.json");
    let mut apply = APPLY.to_vec();
    apply.extend(["--checkpoint-file", text(&checkpoint)]);
    let mut limited = apply.clone();
    limited.extend(["--limit-segments", "2"]);

    let first = farm.run(&limited);

    assert_eq!(first.code, 0, "{}", first.err);
    let saved = read_report(&checkpoint);
    assert!(!saved.finished);
    assert_eq!(saved.segments, 2);
    assert_eq!(saved.last_key.as_deref(), Some(farm.keys[1].as_str()));
    assert!(farm.index.pack(&farm.keys[1]).unwrap().is_some());
    assert_eq!(farm.index.pack(&farm.keys[2]).unwrap(), None);
    assert!(!farm.path("checkpoint.json.tmp").exists());
    let second = farm.run(&apply);
    assert_eq!(second.code, 0, "{}", second.err);
    let done = read_report(&checkpoint);
    assert!(done.finished);
    assert_eq!(done.resumed_after.as_deref(), Some(farm.keys[1].as_str()));
    assert_eq!(done.segments, 3);
    assert_eq!(done.packs_new, 3);
    assert_eq!(farm.read_all(), farm.found_all());
    let third = farm.run(&apply);
    assert_eq!(third.code, 0, "{}", third.err);
    let again = read_report(&checkpoint);
    assert_eq!(again.resumed_after, None);
    assert_eq!(again.packs_new, 0);
    assert_eq!(again.packs_known, 3);
}

/// Asserts that an apply given the checkpoint of a dry run is refused with status 2 and one
/// line, that its report is still printed, and that nothing is recorded.
#[test]
fn an_apply_given_the_checkpoint_of_a_dry_run_is_refused() {
    let farm = Farm::new("runs-mixed");
    let checkpoint = farm.path("checkpoint.json");
    let dry = [
        "--limit-segments",
        "1",
        "--checkpoint-file",
        text(&checkpoint),
    ];
    assert_eq!(farm.run(&dry).code, 0);
    let mut apply = APPLY.to_vec();
    apply.extend(["--checkpoint-file", text(&checkpoint)]);

    let ran = farm.run(&apply);

    assert_eq!(ran.code, 2);
    assert_eq!(ran.err.lines().count(), 1, "{}", ran.err);
    says(&ran.err, "kbf-pack-adopt: refused: ");
    says(&ran.err, "another mode, prefix or source store");
    says(&ran.out, "objects 0, segments 0");
    for key in &farm.keys {
        assert_eq!(farm.index.pack(key).unwrap(), None);
    }
}

/// Asserts that a run given the checkpoint of a run over another store is refused with status 2
/// and one line, and lists and records nothing.
#[test]
fn a_checkpoint_of_another_source_store_is_refused() {
    let farm = Farm::new("runs-other-source");
    let checkpoint = farm.path("checkpoint.json");
    let spelled = r#"{"mode": "apply", "prefix": "kbf/", "source": "http://elsewhere/kbf"}"#;
    write_file(&checkpoint, spelled, 0o600);
    let mut apply = APPLY.to_vec();
    apply.extend(["--checkpoint-file", text(&checkpoint)]);

    let ran = farm.run(&apply);

    assert_eq!(ran.code, 2);
    assert_eq!(ran.err.lines().count(), 1, "{}", ran.err);
    says(&ran.err, "another mode, prefix or source store");
    says(&ran.out, "objects 0, segments 0");
    for key in &farm.keys {
        assert_eq!(farm.index.pack(key).unwrap(), None);
    }
}

/// Asserts that a checkpoint file that is not a checkpoint, one whose last key is no object key
/// and one that cannot be read are each refused with status 2 and one line that names the file.
#[test]
fn a_checkpoint_file_that_is_no_checkpoint_is_refused() {
    let farm = Farm::new("runs-bad-checkpoint");
    let garbage = farm.path("garbage.json");
    write_file(&garbage, "not json", 0o600);
    let wrong_key = farm.path("key.json");
    let wrong = AdoptReport {
        prefix: "kbf/".to_owned(),
        source: format!("{}/{BUCKET}", farm.s3.endpoint()),
        last_key: Some("not a key".to_owned()),
        ..AdoptReport::default()
    };
    let spelled = serde_json::to_string(&wrong).expect("a report");
    write_file(&wrong_key, &spelled, 0o600);
    let folder = farm.path("folder");
    std::fs::create_dir(&folder).expect("made");

    for (file, why) in [
        (&garbage, "not a checkpoint"),
        (&wrong_key, "last key of the checkpoint"),
        (&folder, "Is a directory"),
    ] {
        let ran = farm.run(&["--checkpoint-file", text(file)]);

        assert_eq!(ran.code, 2, "{why}: {}", ran.err);
        assert_eq!(ran.err.lines().count(), 1, "{}", ran.err);
        says(&ran.err, why);
    }
}

/// Asserts that a pack the index holds under a segment's key with other contents is counted and
/// listed and does not fail the run, and that with the flag the run fails with status 1, names
/// the pack, and does not count the conflict that stopped it.
#[test]
fn a_conflict_is_reported_and_fails_the_run_only_with_the_flag() {
    let farm = Farm::new("runs-conflict");
    farm.index.record_pack(&other_pack(&farm.keys[1])).unwrap();

    let lenient = farm.run(&APPLY);

    assert_eq!(lenient.code, 0, "{}", lenient.err);
    says(&lenient.out, "conflicts 1");
    let listed = format!("  {}: Conflict", farm.keys[1]);
    says(&lenient.out, &listed);
    let mut strict = APPLY.to_vec();
    strict.push("--fail-on-conflict");
    let failed = farm.run(&strict);
    assert_eq!(failed.code, 1);
    says(&failed.err, "kbf-pack-adopt: failed: ");
    says(&failed.err, "is recorded already with other contents");
    says(&failed.err, &farm.keys[1]);
    says(&failed.out, "conflicts 0");
}

/// Asserts that the report is written to its file even when the run fails.
#[test]
fn the_report_file_is_written_when_the_run_fails() {
    let farm = Farm::new("runs-report-on-failure");
    farm.index.record_pack(&other_pack(&farm.keys[1])).unwrap();
    let report = farm.path("report.json");
    let mut flags = APPLY.to_vec();
    flags.extend(["--fail-on-conflict", "--report-json", text(&report)]);

    let ran = farm.run(&flags);

    assert_eq!(ran.code, 1);
    let written = read_report(&report);
    assert!(!written.finished);
    assert_eq!(written.packs_new, 1);
    assert_eq!(written.last_key.as_deref(), Some(farm.keys[0].as_str()));
}

/// A standard output that has been closed, as a pipe whose reader has gone.
struct Closed;

impl io::Write for Closed {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts that a report that cannot be printed because the standard output is closed is still
/// written to its file, and that the run then fails with status 1 and a line that says so.
#[test]
fn the_report_file_is_written_when_the_standard_output_is_closed() {
    let farm = Farm::new("runs-closed-output");
    let report = farm.path("report.json");
    let argv = farm.argv(SOCKET_URL, &["--report-json", text(&report)]);
    let mut err = Vec::new();

    let code = main_with_seams(argv, &farm.seams(), &mut Closed, &mut err);

    assert_eq!(code, 1);
    let said = String::from_utf8(err).expect("UTF-8");
    says(&said, "kbf-pack-adopt: failed: ");
    let written = read_report(&report);
    assert!(written.finished);
    assert_eq!(written.packs_new, 3);
}

/// Asserts that a report file and a checkpoint file that cannot be written each fail the run
/// with status 1 and a line that says so, after the report is printed.
#[test]
fn a_file_that_cannot_be_written_fails_the_run() {
    let farm = Farm::new("runs-unwritable");
    let report = farm.path("absent-directory/report.json");
    let checkpoint = farm.path("absent-directory/checkpoint.json");

    let reported = farm.run(&["--report-json", text(&report)]);
    let kept = farm.run(&["--checkpoint-file", text(&checkpoint)]);

    assert_eq!(reported.code, 1);
    says(&reported.out, "packs: new 3");
    says(&reported.err, "kbf-pack-adopt: failed: ");
    assert_eq!(kept.code, 1);
    says(&kept.err, "saving the checkpoint");
}

/// Asserts that a pack index that cannot be reached fails the run with status 1 and a line, and
/// prints no report.
#[test]
fn a_pack_index_that_cannot_be_reached_fails_the_run() {
    let farm = Farm::new("runs-unreachable");

    let ran = farm.run_for_real("postgres://nobody@127.0.0.1:1/none");

    assert_eq!(ran.code, 1);
    assert!(ran.out.is_empty(), "{}", ran.out);
    says(&ran.err, "kbf-pack-adopt: failed: ");
    assert_eq!(ran.err.lines().count(), 1, "{}", ran.err);
}
