//! `cargo xtask release verdict-write`: the Beta record a release tag carries (audit only:
//! nothing gates on it). Each field of it is checked on its own; it has no bake and no load.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_gate::{ResultDoc, SetHash, Stage, Status, Verdict};
use kbf_testkit::dir::TestDir;
use xtask::release::verdict_write::{STAGE, WriteArgs, build, run};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/release-verdict-write.json"
);
const SHA: &str = "357494ac6b0fecfa065d6c8fa038cfca9ae9c13f";
const SUMS: &str = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881  a\n";

struct Fx {
    root: PathBuf,
    /// Last field: the directory is removed with the fixture.
    _own: TestDir,
}

fn fx(name: &str) -> Fx {
    let own = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-release-verdict-write"),
        name,
    );
    let root = own.to_path_buf();
    std::fs::create_dir_all(root.join("dist")).unwrap();
    std::fs::write(root.join("dist/SHA256SUMS"), SUMS).unwrap();
    Fx { root, _own: own }
}

impl Fx {
    fn args(&self) -> WriteArgs {
        WriteArgs {
            dir: self.root.join("dist"),
            commit: SHA.to_owned(),
            tag: "v0.3.7".to_owned(),
            run_id: 9001,
            attempt: 2,
            pipeline_run: 111,
            not_reversible: vec![],
            escape: vec![],
            withdraws: vec![],
            at: Some(5000),
            out: self.root.join("verdict.json"),
        }
    }

    /// `release verdict-write` with the fixture's fields as flags, then `extra`.
    fn cli(&self, extra: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(XTASK)
            .args(["release", "verdict-write"])
            .arg(self.root.join("dist"))
            .args(["--commit", SHA, "--tag", "v0.3.7", "--run-id", "9001"])
            .args(["--attempt", "2", "--pipeline-run", "111"])
            .args(["--at", "5000", "--out"])
            .arg(self.root.join("verdict.json"))
            .args(extra)
            .output()
            .expect("run xtask");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    }

    fn written(&self) -> ResultDoc {
        let bytes = std::fs::read(self.root.join("verdict.json")).expect("the verdict");
        ResultDoc::from_bytes(&bytes).expect("a result document")
    }
}

#[test]
fn the_gate_stage_is_beta() {
    // Catches: the record's stage changed to Gamma, which would claim a bake nobody ran.
    assert_eq!(STAGE, Stage::Beta);
}

#[test]
fn the_verdict_is_a_green_beta_document_and_matches_its_golden() {
    // Catches: any field of the document changing shape or value unseen.
    let fx = fx("golden");
    let (code, out, err) = fx.cli(&[
        "--not-reversible",
        "meta-log format",
        "--escape",
        "e1",
        "--withdraws",
        "v0.3.6",
    ]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("Beta record of v0.3.7"), "{out}");
    let bytes = std::fs::read(fx.root.join("verdict.json")).unwrap();
    let golden = std::fs::read(GOLDEN).expect("golden file");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        String::from_utf8_lossy(&golden)
    );
}

#[test]
fn the_stage_is_beta_and_the_status_green() {
    // Catches: a Gamma or Prod document, or one that is not green.
    let fx = fx("stage");
    assert_eq!(fx.cli(&[]).0, Some(0));
    let doc = fx.written();
    assert_eq!(doc.stage, Stage::Beta);
    assert_eq!(doc.status, Status::Green);
    assert!(doc.passed());
}

#[test]
fn the_commit_tag_and_set_are_the_releases() {
    // Catches: a verdict for other files, another commit or another tag.
    let fx = fx("subject");
    assert_eq!(fx.cli(&[]).0, Some(0));
    let doc = fx.written();
    assert_eq!(doc.commit.as_str(), SHA);
    assert_eq!(doc.tag.as_deref(), Some("v0.3.7"));
    assert_eq!(doc.set_hash, SetHash::of_sums(SUMS.as_bytes()));
}

#[test]
fn the_set_follows_the_files() {
    // Catches: a set hash fixed or taken from something but SHA256SUMS.
    let fx = fx("set");
    std::fs::write(fx.root.join("dist/SHA256SUMS"), format!("{SUMS}{SUMS}")).unwrap();
    assert_eq!(fx.cli(&[]).0, Some(0));
    let doc = fx.written();
    assert_eq!(
        doc.set_hash,
        SetHash::of_sums(format!("{SUMS}{SUMS}").as_bytes())
    );
}

#[test]
fn the_run_and_attempt_are_this_release_run() {
    // Catches: run id and attempt swapped or dropped.
    let fx = fx("run");
    assert_eq!(fx.cli(&[]).0, Some(0));
    let doc = fx.written();
    assert_eq!(doc.run_id, "9001");
    assert_eq!(doc.attempt, 2);
}

#[test]
fn each_of_the_three_gates_is_a_green_gate_and_the_note_names_the_runs() {
    // Catches: a gate missing, a red one, or run ids in the wrong order.
    let fx = fx("gates");
    assert_eq!(fx.cli(&[]).0, Some(0));
    let doc = fx.written();
    let names: Vec<&str> = doc.gates.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, ["ci", "beta", "artifacts"]);
    assert!(doc.gates.iter().all(|g| g.verdict == Verdict::Green));
    let note = doc.note.expect("a note");
    assert_eq!(
        note,
        "gate run: pipeline 111 (ci, artifacts, beta); beta smoke on the attested kbf"
    );
}

#[test]
fn the_time_is_the_flag_or_the_clock() {
    // Catches: the `--at` ignored, or a document with no time.
    let fx = fx("time");
    assert_eq!(fx.cli(&[]).0, Some(0));
    let doc = fx.written();
    assert_eq!((doc.started, doc.finished), (5000, Some(5000)));
    assert!(
        doc.gates
            .iter()
            .all(|g| g.started == 5000 && g.finished == 5000)
    );
    let mut a = fx.args();
    a.at = None;
    run(&a).expect("write with the clock");
    assert!(fx.written().started > 1_700_000_000);
}

#[test]
fn the_not_reversible_escape_and_withdrawn_lines_are_carried_in_order() {
    // Catches: a list dropped, or one carried into another's field.
    let fx = fx("lists");
    let mut a = fx.args();
    a.not_reversible = vec!["one".to_owned(), "two".to_owned()];
    a.escape = vec!["e".to_owned()];
    a.withdraws = vec!["v0.3.5".to_owned(), "v0.3.6".to_owned()];
    run(&a).unwrap();
    let doc = fx.written();
    assert_eq!(doc.not_reversible, ["one", "two"]);
    assert_eq!(doc.escapes, ["e"]);
    assert_eq!(doc.withdrawn, ["v0.3.5", "v0.3.6"]);
    let plain = fx.args();
    run(&plain).unwrap();
    let doc = fx.written();
    assert!(doc.not_reversible.is_empty() && doc.escapes.is_empty() && doc.withdrawn.is_empty());
}

#[test]
fn a_beta_verdict_has_no_bake() {
    // Catches: a bake invented for a stage that has none.
    let fx = fx("no-bake");
    assert_eq!(fx.cli(&[]).0, Some(0));
    let doc = fx.written();
    assert!(doc.bake.is_none() && doc.load.is_none());
}

#[test]
fn a_bake_file_flag_is_gone() {
    // Catches: the Gamma bake path coming back (the verb once took `--bake-file` for a
    // Gamma record). clap refuses an argument it does not have, with the usage exit code.
    let fx = fx("no-bake-flag");
    let (code, _, err) = fx.cli(&["--bake-file", "bake.json"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--bake-file"), "{err}");
    assert!(!fx.root.join("verdict.json").exists());
}

#[test]
fn a_bad_commit_tag_or_missing_files_are_refused_and_write_nothing() {
    // Catches: a verdict for a branch name, a probe tag, or a directory without sums.
    let fx = fx("refusals");
    let mut a = fx.args();
    a.commit = "main".to_owned();
    assert!(build(&a, 0).unwrap_err().to_string().contains("--commit"));
    let mut a = fx.args();
    a.tag = "v0.3.7-rc1".to_owned();
    assert!(
        build(&a, 0)
            .unwrap_err()
            .to_string()
            .contains("not a release tag")
    );
    let mut a = fx.args();
    a.dir = fx.root.join("nowhere");
    assert!(build(&a, 0).is_err());
    let (code, _, _) = fx.cli(&["--commit", "main"]);
    assert_ne!(code, Some(0));
    assert!(!fx.root.join("verdict.json").exists());
}

#[test]
fn a_verdict_that_cannot_be_written_fails() {
    // Catches: a silent success with no file for the tag job to read.
    let fx = fx("unwritable");
    let mut a = fx.args();
    a.out = fx.root.join("no/such/dir/verdict.json");
    assert!(run(&a).is_err());
}
