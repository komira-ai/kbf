//! The result document: its shape, and that a file is on disk, `RUNNING` or finished,
//! however the run ends.

use std::path::{Path, PathBuf};

use super::*;
use crate::gates::{Gate, Verdict};

fn file(name: &str) -> PathBuf {
    crate::scratch("result").join(name)
}

fn read(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("the file")).expect("JSON")
}

fn doc() -> Document {
    Document::running("farm_under_build_load", |key| match key {
        "GITHUB_SHA" => Some("0123abc".to_owned()),
        "GITHUB_RUN_ID" => Some("42".to_owned()),
        "GITHUB_RUN_ATTEMPT" => Some("2".to_owned()),
        _ => None,
    })
}

fn gate(name: &str, verdict: Verdict) -> Gate {
    Gate {
        name: name.to_owned(),
        measured: serde_json::json!(0),
        threshold: "0".to_owned(),
        verdict,
        started: "a".to_owned(),
        finished: "b".to_owned(),
        detail: None,
    }
}

#[test]
fn the_document_has_the_designs_fields() {
    // Catches: a renamed or dropped field that the offer to gamma later reads (`stage`,
    // `commit`, `set_hash`, `run_id`, `attempt`, `status`, `gates`, the harness commit,
    // the job's duration).
    let value = serde_json::to_value(doc()).expect("json");
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "attempt",
            "attested",
            "builds",
            "commit",
            "duration_secs",
            "finished",
            "gates",
            "harness_commit",
            "note",
            "processes",
            "run_id",
            "runtime",
            "scenario",
            "schema_version",
            "set_hash",
            "stage",
            "started",
            "status"
        ]
    );
    assert_eq!(value["stage"], "beta");
    assert_eq!(value["commit"], "0123abc");
    assert_eq!(value["run_id"], "42");
    assert_eq!(value["attempt"], "2");
    assert_eq!(value["set_hash"], serde_json::Value::Null);
    // Catches: a layout change a reader cannot tell from the old one.
    assert_eq!(value["schema_version"], SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 1);
}

#[test]
fn the_harness_commit_is_its_own_when_it_is_given_and_the_commit_under_test_otherwise() {
    // Catches: two fields that cannot differ (the attested beta runs released bytes with the
    // harness of another checkout, and the document must say which is which), and a harness
    // commit that is lost when it is given. Mutant: `harness_commit` always the commit.
    let both = Document::running("s", |key| match key {
        "GITHUB_SHA" => Some("aaa".to_owned()),
        "KBF_HARNESS_COMMIT" => Some("bbb".to_owned()),
        _ => None,
    });
    assert_eq!(
        (both.commit.as_str(), both.harness_commit.as_str()),
        ("aaa", "bbb")
    );
    assert_eq!(doc().harness_commit, "0123abc");
}

#[test]
fn the_document_says_it_is_unattested() {
    // Catches: a smoke taken for an attested run (the offer to gamma must not accept it).
    let value = serde_json::to_value(doc()).expect("json");
    assert_eq!(value["attested"], false);
    assert!(value["note"].as_str().expect("note").contains("unattested"));
}

#[test]
fn outside_ci_the_identity_is_local() {
    let value = serde_json::to_value(Document::running("s", |_| None)).expect("json");
    assert_eq!(value["commit"], "local");
    assert_eq!(value["run_id"], "local");
}

#[test]
fn the_file_is_running_at_the_start_and_finished_after() {
    // Catches: a document written only at the end (a cancelled run would leave no file
    // to read as "not run"), and one never stamped finished.
    let path = file("finished.json");
    let mut writer = Writer::start(&path, doc());
    assert_eq!(read(&path)["status"], "RUNNING");
    assert_eq!(read(&path)["finished"], serde_json::Value::Null);
    writer.doc.gates = vec![gate("builds_run", Verdict::Pass)];
    writer.finish(Status::Passed);
    let value = read(&path);
    assert_eq!(value["status"], "PASSED");
    assert!(value["finished"].is_string());
    assert_eq!(value["gates"][0]["name"], "builds_run");
    drop(writer);
    // Dropping a finished document changes nothing.
    assert_eq!(read(&path)["status"], "PASSED");
}

#[test]
fn a_run_that_ends_without_finishing_leaves_failed_not_running() {
    // Catches: a panic or early return leaving RUNNING (taken for "not run") or the
    // last good state (a PASSED written early): the guard must write FAILED.
    let path = file("dropped.json");
    {
        let writer = Writer::start(&path, doc());
        assert_eq!(read(&path)["status"], "RUNNING");
        drop(writer);
    }
    let value = read(&path);
    assert_eq!(value["status"], "FAILED");
    assert!(
        value["error"]
            .as_str()
            .expect("error")
            .contains("ended before")
    );
}

#[test]
fn a_panic_leaves_failed() {
    // Catches: the guard not running on unwinding.
    let path = file("panicked.json");
    let at = path.clone();
    let caught = std::panic::catch_unwind(move || {
        let _writer = Writer::start(&at, doc());
        panic!("the scenario panicked");
    });
    assert!(caught.is_err());
    assert_eq!(read(&path)["status"], "FAILED");
}

#[test]
fn a_timeout_is_recorded_as_timed_out() {
    let path = file("timed-out.json");
    let mut writer = Writer::start(&path, doc());
    writer.finish(Status::TimedOut);
    assert_eq!(read(&path)["status"], "TIMED_OUT");
}

#[test]
fn the_summary_is_a_table_of_the_gates() {
    // Catches: a job summary that hides a failed gate or the unattested note.
    let mut d = doc();
    d.status = Status::Failed;
    d.gates = vec![
        gate("builds_run", Verdict::Pass),
        gate("hang", Verdict::Fail),
    ];
    d.runtime.envoy = "absent: not set".to_owned();
    d.duration_secs = Some(412.4);
    let text = d.summary();
    assert!(text.contains("| builds_run | 0 | 0 | PASS |"), "{text}");
    assert!(text.contains("| hang | 0 | 0 | FAIL |"), "{text}");
    assert!(text.contains("unattested"), "{text}");
    assert!(text.contains("envoy: absent: not set"), "{text}");
    assert!(text.contains("duration: 412 s"), "{text}");
}

#[test]
fn the_summary_spells_the_status_as_the_document_does_and_keeps_a_pipe_in_its_cell() {
    // Catches: `Passed` in the summary where the JSON says `PASSED` (a reader greps one for
    // the other), and a `|` in a measured value or detail ending its cell, which shifts every
    // column after it. Mutants: `{:?}` for the status; no escape in `cell`.
    let mut d = doc();
    d.status = Status::Passed;
    let mut g = gate("lost_blobs_gauge", Verdict::Pass);
    g.measured = serde_json::json!("a|b");
    g.threshold = "x | y".to_owned();
    d.gates = vec![g];
    let text = d.summary();
    assert!(
        text.starts_with("## Beta `farm_under_build_load`: PASSED\n"),
        "{text}"
    );
    assert!(
        text.contains("| lost_blobs_gauge | \"a\\|b\" | x \\| y | PASS |"),
        "{text}"
    );
    for status in [
        Status::Running,
        Status::Passed,
        Status::Failed,
        Status::TimedOut,
    ] {
        let json = serde_json::to_value(status).expect("json");
        assert_eq!(json, status.as_str());
    }
}

#[test]
fn the_summary_names_an_error_and_leaves_out_a_duration_it_does_not_have() {
    // Catches: a run that failed before it could judge showing a table and no reason, and a
    // duration of nothing printed as zero seconds.
    let mut d = doc();
    d.status = Status::TimedOut;
    d.error = Some("timed out waiting for the frontend".to_owned());
    let text = d.summary();
    assert!(
        text.contains("error: timed out waiting for the frontend"),
        "{text}"
    );
    assert!(!text.contains("duration:"), "{text}");
}

#[test]
fn a_reason_given_before_the_drop_is_kept() {
    // Catches: the guard overwriting the reason the scenario gave with its own generic one.
    let path = file("reason.json");
    {
        let mut writer = Writer::start(&path, doc());
        writer.doc.error = Some("the farm never became ready".to_owned());
    }
    let value = read(&path);
    assert_eq!(value["status"], "FAILED");
    assert_eq!(value["error"], "the farm never became ready");
}
