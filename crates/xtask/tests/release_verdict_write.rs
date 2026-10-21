//! `cargo xtask release verdict-write`: the verdict document a release tag carries. Each
//! field of it is checked on its own, and the Gamma stage is refused without a bake.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_gate::{ResultDoc, SetHash, Stage, Status, Verdict};
use kbf_testkit::dir::TestDir;
use xtask::release::verdict::GATE_STAGE;
use xtask::release::verdict_write::{WriteArgs, build, run};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/release-verdict-write.json"
);
const SHA: &str = "357494ac6b0fecfa065d6c8fa038cfca9ae9c13f";
const SUMS: &str = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881  a\n";
const DAY: u64 = 24 * 60 * 60;

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
            ci_run: 111,
            beta_run: 222,
            artifacts_run: 333,
            not_reversible: vec![],
            escape: vec![],
            withdraws: vec![],
            bake_file: None,
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
            .args(["--attempt", "2", "--ci-run", "111", "--beta-run", "222"])
            .args(["--artifacts-run", "333", "--at", "5000", "--out"])
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

    fn bake_file(&self, text: &str) -> PathBuf {
        let file = self.root.join("bake.json");
        std::fs::write(&file, text).unwrap();
        file
    }
}

fn bake_json(start: u64, end: &str, actions: u64) -> String {
    format!(
        "{{\"bake\": {{\"start\": {start}, \"end\": {end}}}, \"load\": {{\
         \"actions\": {{\"measured\": {actions}, \"required\": 1000}}, \
         \"builds\": {{\"measured\": 10, \"required\": 10}}, \
         \"hours\": {{\"measured\": 3, \"required\": 3}}}}}}"
    )
}

#[test]
fn the_gate_stage_is_beta() {
    // Catches: the constant flipped to Gamma on its own (that is its own change).
    assert_eq!(GATE_STAGE, Stage::Beta);
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
    assert!(out.contains("Beta verdict of v0.3.7"), "{out}");
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
        "gate runs: ci 111, beta 222, artifacts 333; tested_bytes: rebuilt"
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
    run(&a, Stage::Beta).expect("write with the clock");
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
    run(&a, Stage::Beta).unwrap();
    let doc = fx.written();
    assert_eq!(doc.not_reversible, ["one", "two"]);
    assert_eq!(doc.escapes, ["e"]);
    assert_eq!(doc.withdrawn, ["v0.3.5", "v0.3.6"]);
    let plain = fx.args();
    run(&plain, Stage::Beta).unwrap();
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
fn a_bake_record_for_beta_is_refused() {
    // Catches: a bake carried on a Beta document unchecked.
    let fx = fx("beta-bake");
    let file = fx.bake_file(&bake_json(0, &DAY.to_string(), 1000));
    let (code, _, err) = fx.cli(&["--bake-file", file.to_str().unwrap()]);
    assert_eq!(code, Some(2));
    assert!(err.contains("belongs to a Gamma verdict"), "{err}");
}

#[test]
fn a_bad_commit_tag_or_missing_files_are_refused_and_write_nothing() {
    // Catches: a verdict for a branch name, a probe tag, or a directory without sums.
    let fx = fx("refusals");
    let mut a = fx.args();
    a.commit = "main".to_owned();
    assert!(
        build(&a, Stage::Beta, 0)
            .unwrap_err()
            .to_string()
            .contains("--commit")
    );
    let mut a = fx.args();
    a.tag = "v0.3.7-rc1".to_owned();
    assert!(
        build(&a, Stage::Beta, 0)
            .unwrap_err()
            .to_string()
            .contains("not a release tag")
    );
    let mut a = fx.args();
    a.dir = fx.root.join("nowhere");
    assert!(build(&a, Stage::Beta, 0).is_err());
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
    assert!(run(&a, Stage::Beta).is_err());
}

#[test]
fn gamma_without_a_bake_record_is_refused() {
    // Catches: GATE_STAGE flipped to Gamma without the bake rule: a "bake" nobody ran.
    let fx = fx("gamma-no-bake");
    let err = build(&fx.args(), Stage::Gamma, 0).unwrap_err();
    assert!(err.to_string().contains("--bake-file is required"), "{err}");
    assert!(run(&fx.args(), Stage::Gamma).is_err());
    assert!(!fx.root.join("verdict.json").exists());
}

#[test]
fn gamma_with_a_full_bake_is_written() {
    // Catches: the bake rule refusing a good bake, or the bake dropped from the document.
    let fx = fx("gamma-bake");
    let mut a = fx.args();
    a.bake_file = Some(fx.bake_file(&bake_json(1000, &(1000 + DAY).to_string(), 1000)));
    run(&a, Stage::Gamma).expect("a Gamma verdict");
    let doc = fx.written();
    assert_eq!(doc.stage, Stage::Gamma);
    let bake = doc.bake.expect("the bake");
    assert_eq!((bake.start, bake.end), (1000, Some(1000 + DAY)));
    assert!(doc.load.expect("the load").met());
}

#[test]
fn gamma_refuses_a_bake_that_is_short_open_backwards_or_under_load() {
    // Catches: a bake of 23 h 59 m 59 s, one with no end or end before start, or one
    // below the load floor passing.
    let fx = fx("gamma-bad-bake");
    let cases = [
        (bake_json(0, &(DAY - 1).to_string(), 1000), "less than"),
        (bake_json(0, "null", 1000), "no end"),
        (bake_json(DAY, "5", 1000), "ends before it starts"),
        (
            bake_json(0, &DAY.to_string(), 999),
            "minimum load was not met",
        ),
    ];
    for (json, why) in cases {
        let mut a = fx.args();
        a.bake_file = Some(fx.bake_file(&json));
        let err = build(&a, Stage::Gamma, 0).unwrap_err();
        assert!(err.to_string().contains(why), "{why}: {err}");
    }
}

#[test]
fn a_bake_file_that_is_not_a_bake_record_is_refused() {
    // Catches: a missing part of the record read as zero.
    let fx = fx("bake-file");
    let cases = [
        ("not json", "not JSON"),
        ("{}", "no `bake`"),
        ("{\"bake\": 3, \"load\": {}}", "`bake`"),
        ("{\"bake\": {\"start\": 0, \"end\": 1}}", "no `load`"),
        (
            "{\"bake\": {\"start\": 0, \"end\": 1}, \"load\": 3}",
            "`load`",
        ),
    ];
    for (text, why) in cases {
        let mut a = fx.args();
        a.bake_file = Some(fx.bake_file(text));
        let err = build(&a, Stage::Gamma, 0).unwrap_err();
        assert!(err.to_string().contains(why), "{why}: {err}");
    }
    let mut a = fx.args();
    a.bake_file = Some(fx.root.join("nowhere.json"));
    assert!(build(&a, Stage::Gamma, 0).is_err());
}
