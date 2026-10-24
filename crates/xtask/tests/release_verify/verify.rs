//! `cargo xtask release verify` against real git repositories. Nothing here judges a verdict
//! (ruling, 2026-10-21): the tag's message is the Beta record `release verdict-write`
//! wrote, an audit record that nothing gates on. `verify` checks the tag, the commit, main
//! and the set; the tests that a record which says it failed, names another commit or set,
//! or is not a record at all, changes none of that are `a_record_*` and `a_tag_*_verifies`.

use kbf_gate::{CommitId, GateResult, Limit, Sample, SetHash, Stage, Status, Waiver};
use std::process::Command;

use crate::fixture::*;

#[test]
fn the_record_verdict_write_writes_verifies_and_prints_the_set_hash() {
    // Catches: the pipeline's own record refused by its own check, a check that cannot
    // read what git stores, or another set hash printed than the files'.
    let fx = fx("written");
    fx.annotate("v0.3.7", RELEASE, &std::fs::read(WRITTEN).unwrap());
    assert_eq!(
        fx.git(&["cat-file", "-t", "refs/tags/v0.3.7"]),
        "tag",
        "an annotated tag"
    );
    let output = fx.root.join("out");
    let out = output.to_str().unwrap();
    let (code, stdout, err) = run(&fx, "verify", "v0.3.7", RELEASE, &["--output", out]);
    assert_eq!(code, Some(0), "{err}");
    assert!(stdout.contains("tag v0.3.7 names "), "{stdout}");
    assert!(stdout.ends_with(&format!("set-hash={SET}\n")), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        format!("set-hash={SET}\n")
    );
}

#[test]
fn a_tag_with_an_empty_message_verifies() {
    // Catches: verify gating on a record being there (defect: refuse an empty message).
    let fx = fx("empty");
    let object = format!("object {RELEASE}\ntype commit\ntag {TAG}\ntagger T <t@e.x> 0 +0000\n\n");
    let sha = git(&fx.repo(), &["mktag"], Some(&object));
    fx.git(&["update-ref", &format!("refs/tags/{TAG}"), &sha]);
    let (code, out, err) = verify(&fx, TAG, RELEASE);
    assert_eq!(code, Some(0), "{out}{err}");
    assert!(out.ends_with(&format!("set-hash={SET}\n")), "{out}");
}

#[test]
fn a_tag_whose_message_is_prose_verifies() {
    passes_with("prose", b"release 0.10.0\n\nlooks fine\n");
}

#[test]
fn a_lightweight_tag_is_refused() {
    // Catches (defect: accept `Tag::Lightweight` again): a ref straight at the right commit
    // verifying. The release tag is annotated; the fixture is a real lightweight tag, and
    // git says its object is a commit.
    let fx = fx("lightweight");
    fx.git(&["tag", TAG, RELEASE]);
    assert_eq!(
        fx.git(&["cat-file", "-t", &format!("refs/tags/{TAG}")]),
        "commit"
    );
    let (code, out, err) = verify(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(err.contains("is lightweight"), "{err}");
}

#[test]
fn a_record_that_says_failed_or_names_something_else_does_not_block_verify() {
    // Catches (defect: re-add a verdict judgment in `verify`): any record content gating
    // the release. Each record would have failed the retired verdict check: red, extended
    // or running; another stage; another commit, set or tag; none of them is read.
    for status in [Status::Red, Status::Extended, Status::Running] {
        let mut d = good();
        d.status = status;
        passes_with(&format!("status-{status:?}"), &msg(&d));
    }
    let mut d = good();
    d.stage = Stage::Prod;
    passes_with("stage-prod", &msg(&d));
    let fx0 = fx("other-commit-probe");
    let mut d = good();
    d.commit = CommitId::parse(&fx0.first).unwrap();
    passes_with("other-commit", &msg(&d));
    let mut d = good();
    d.set_hash = SetHash::of_sums(b"other files");
    passes_with("other-set", &msg(&d));
    let mut d = good();
    d.tag = Some("v0.9.0".to_owned());
    passes_with("other-tag", &msg(&d));
    let mut d = good();
    d.tag = None;
    passes_with("no-tag", &msg(&d));
    // A green status over a red gate is not a record the parser reads: it is as good as none.
    let mut d = good();
    d.gates[0] = GateResult::decide("lost-blobs", Sample::Absent, Limit::AtMost(0.0), 1, 2);
    passes_with("green-over-red-gate", &msg(&d));
    // A record with a waiver that names no reason: hand-written, since the type refuses it.
    let mut d = good();
    d.waivers = vec![Waiver::new("min-load", "op", "x").unwrap()];
    let text = String::from_utf8(msg(&d))
        .unwrap()
        .replace("\"reason\": \"x\"", "\"reason\": \"\"");
    passes_with("blank-waiver", text.as_bytes());
}

#[test]
fn a_tag_that_is_not_there_fails() {
    let fx = fx("no-tag");
    let (code, _, err) = verify(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
}

#[test]
fn a_tag_on_a_commit_not_on_main_fails_even_with_a_perfect_record() {
    // Catches: a verify that never asks whether the commit is main's. The record names
    // the side commit, the tag names it, the set matches: only main refuses it.
    let fx = fx("off-main");
    let mut d = good();
    d.commit = CommitId::parse(&fx.side).unwrap();
    fx.annotate(TAG, &fx.side, &msg(&d));
    let (code, _, err) = verify(&fx, TAG, &fx.side);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("is not on main"), "{err}");
    // The same tag shape on a main commit passes (so the refusal above was the branch).
    let fx2 = fx_with_first_record("off-main-control");
    let (code, _, err) = verify(&fx2, TAG, &fx2.first);
    assert_eq!(code, Some(0), "{err}");
}

/// A fixture whose tag is at `first` (on main), with a record naming it.
fn fx_with_first_record(name: &str) -> Fx {
    let fx = fx(name);
    let mut d = good();
    d.commit = CommitId::parse(&fx.first).unwrap();
    fx.annotate(TAG, &fx.first, &msg(&d));
    fx
}

#[test]
fn a_tag_naming_another_commit_than_the_release_fails() {
    // Catches: a check that trusts `--commit` and never reads what the tag points at:
    // the record, the main check and the set all agree with `--commit`.
    let fx = fx("tag-elsewhere");
    let mut d = good();
    d.commit = CommitId::parse(&fx.first).unwrap();
    fx.annotate(TAG, RELEASE, &msg(&d));
    let (code, _, err) = verify(&fx, TAG, &fx.first);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("not commit"), "{err}");
}

#[test]
fn a_changed_sums_file_changes_the_printed_set() {
    // Catches: a set hash computed from something other than the SHA256SUMS bytes, or
    // taken from the tag's record (which names the old set and is not read).
    let fx = fx("changed-sums");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    std::fs::write(fx.dist().join("SHA256SUMS"), format!("{SUMS}{SUMS}")).unwrap();
    let (code, out, err) = verify(&fx, TAG, RELEASE);
    assert_eq!(code, Some(0), "{out}{err}");
    let other = SetHash::of_sums(format!("{SUMS}{SUMS}").as_bytes());
    assert!(
        out.ends_with(&format!("set-hash={}\n", other.as_str())),
        "{out}"
    );
    assert_ne!(other.as_str(), SET);
}

#[test]
fn a_tag_on_an_object_that_is_not_a_commit_fails_naming_its_kind() {
    // Catches: a tag of a tree (same message, same name) passing because only the
    // object id was compared, or the kind never being part of the check.
    let fx = fx("tag-of-tree");
    let tree = fx.git(&["rev-parse", &format!("{RELEASE}^{{tree}}")]);
    fx.annotate(TAG, &tree, &msg(&good()));
    let (code, out, err) = verify(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(err.contains("names tree"), "{err}");
}

#[test]
fn a_tag_object_named_other_than_its_ref_fails() {
    // Catches: refs/tags/v0.10.0 made to point at another tag's object.
    let fx = fx("renamed");
    fx.annotate("v0.9.0", RELEASE, &msg(&good()));
    let sha = fx.git(&["rev-parse", "refs/tags/v0.9.0"]);
    fx.git(&["update-ref", &format!("refs/tags/{TAG}"), &sha]);
    let (code, _, err) = verify(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("tag object is named"), "{err}");
}

#[test]
fn problems_are_all_reported_not_just_the_first() {
    let fx = fx("many");
    fx.annotate("v0.9.0", &fx.first, &msg(&good()));
    let sha = fx.git(&["rev-parse", "refs/tags/v0.9.0"]);
    fx.git(&["update-ref", &format!("refs/tags/{TAG}"), &sha]);
    let (_, _, err) = verify(&fx, TAG, RELEASE);
    assert!(
        err.contains("not commit") && err.contains("tag object is named"),
        "{err}"
    );
}

#[test]
fn an_unknown_main_ref_is_an_error_not_a_no() {
    // Catches: a missing origin/main read as "the commit is not on main" (or as on it).
    let fx = fx("no-main");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let (dist, repo) = (fx.dist(), fx.repo());
    let (code, _, err) = xtask(&[
        "release",
        "verify",
        dist.to_str().unwrap(),
        "--repo-dir",
        repo.to_str().unwrap(),
        "--tag",
        TAG,
        "--commit",
        RELEASE,
        "--main-ref",
        "refs/remotes/nowhere/main",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("merge-base"), "{err}");
}

#[test]
fn a_commit_that_is_not_a_full_sha_is_a_usage_error() {
    let fx = fx("short-commit");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let (code, _, err) = verify(&fx, TAG, "abc");
    assert_eq!(code, Some(2), "{err}");
}

#[test]
fn verify_output_appends_to_a_file_that_holds_earlier_lines() {
    // Catches: `--output` truncating instead of appending. On Actions the file is
    // GITHUB_OUTPUT, which holds the earlier steps' lines the publish step still reads.
    let fx = fx("output-append");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let output = fx.root.join("out");
    std::fs::write(&output, "earlier=1\n").unwrap();
    let out = output.to_str().unwrap();
    for _ in 0..2 {
        let (code, stdout, err) = run(&fx, "verify", TAG, RELEASE, &["--output", out]);
        assert_eq!(code, Some(0), "{stdout}{err}");
    }
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        format!("earlier=1\nset-hash={SET}\nset-hash={SET}\n")
    );
}

#[test]
fn verify_output_that_cannot_be_written_fails_after_the_checks_pass() {
    // Catches: a lost set hash reported as success (the publish step would then run
    // without `--set-hash`); here the output path is a directory.
    let fx = fx("output-unwritable");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let dir = fx.root.join("a-directory");
    std::fs::create_dir(&dir).unwrap();
    let (code, _, err) = run(
        &fx,
        "verify",
        TAG,
        RELEASE,
        &["--output", dir.to_str().unwrap()],
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("append to"), "{err}");
}

#[test]
fn a_machine_without_git_fails_naming_git() {
    // Catches: a git that cannot be started reported as anything but a failure that
    // names it (the spawn error arm of every git call).
    let fx = fx("no-git");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let (dist, repo) = (fx.dist(), fx.repo());
    let out = Command::new(XTASK)
        .args(["release", "verify", dist.to_str().unwrap(), "--repo-dir"])
        .arg(&repo)
        .args(["--tag", TAG, "--commit", RELEASE, "--main-ref", "main"])
        .env("PATH", fx.root.join("empty"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("git"));
}
