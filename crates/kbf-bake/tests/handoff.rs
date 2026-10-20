//! The deploy-to-evaluator hand-off file: what it must say and what it must refuse.

use kbf_bake::handoff::{HANDOFF_SCHEMA, Handoff};
use kbf_gate::{CommitId, SetHash};

fn handoff() -> Handoff {
    Handoff {
        schema: HANDOFF_SCHEMA,
        commit: CommitId::parse(&"ab".repeat(20)).unwrap(),
        set_hash: SetHash::of_sums(b"sums\n"),
        run_id: "bake-7".to_owned(),
        deploy_start: 1_000,
        deploy_end: Some(1_600),
        desired_generation: Some(9),
        reversible: true,
        previous_commit: Some(CommitId::parse(&"cd".repeat(20)).unwrap()),
    }
}

fn json() -> serde_json::Value {
    serde_json::from_slice(&handoff().to_bytes().unwrap()).unwrap()
}

fn parse(v: &serde_json::Value) -> Result<Handoff, String> {
    Handoff::parse(v.to_string().as_bytes()).map_err(|e| e.to_string())
}

/// Catches a field lost on the way through the file: every field the evaluator acts on (commit,
/// set hash, run id, span, generation, reversibility, previous commit) survives a round trip.
#[test]
fn a_handoff_round_trips() {
    let h = handoff();
    assert_eq!(Handoff::parse(&h.to_bytes().unwrap()).unwrap(), h);
}

/// Catches `reversible` defaulting: a file that does not say whether the release can be walked
/// back is refused, so "unknown" can never be read as "reversible".
#[test]
fn reversibility_has_no_default() {
    let mut v = json();
    v.as_object_mut().unwrap().remove("reversible");
    let why = parse(&v).unwrap_err();
    assert!(why.contains("reversible"), "{why}");
}

/// Catches a writer that grows a field nobody reads, a wrong schema, an empty run id and a deploy
/// that ended before it began.
#[test]
fn a_handoff_that_is_not_understood_is_refused() {
    let mut v = json();
    v["surprise"] = 1.into();
    assert!(parse(&v).unwrap_err().contains("surprise"));

    let mut v = json();
    v["schema"] = 2.into();
    assert!(parse(&v).unwrap_err().contains("schema 2"));

    let mut v = json();
    v["run_id"] = "  ".into();
    assert!(parse(&v).unwrap_err().contains("run_id"));

    let mut v = json();
    v["deploy_end"] = 999.into();
    assert!(parse(&v).unwrap_err().contains("before deploy_start"));

    let mut v = json();
    v["commit"] = "not-a-commit".into();
    assert!(parse(&v).is_err());
}

/// A deploy still running has no end and no generation yet; the file says so with nulls.
#[test]
fn a_deploy_in_progress_has_no_end_and_no_generation() {
    let mut v = json();
    v["deploy_end"] = serde_json::Value::Null;
    v["desired_generation"] = serde_json::Value::Null;
    v["previous_commit"] = serde_json::Value::Null;
    let h = parse(&v).unwrap();
    assert_eq!(
        (h.deploy_end, h.desired_generation, h.previous_commit),
        (None, None, None)
    );
}

/// A missing file is an error naming it.
#[test]
fn a_missing_file_is_an_error() {
    let e = Handoff::load(std::path::Path::new("/nonexistent/handoff.json")).unwrap_err();
    assert!(e.to_string().contains("/nonexistent/handoff.json"), "{e}");
}
