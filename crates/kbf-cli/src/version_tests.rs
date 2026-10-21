//! The `seq` of the build identity.

use std::process::Command;

use super::*;

/// `git args` in the checkout the tests run in: its trimmed stdout, when it succeeded.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().expect("git runs");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Catches a `seq` that is not the rule applied to this checkout: the build script not run
/// again when `HEAD` moves, a count taken of another directory, a number that is not
/// embedded at all. The rule is `build_seq::choose`, tested on its own against real
/// repositories (`src/build_seq.rs`); here it meets the live checkout, which is a full clone
/// on a person's machine and a shallow one in CI, so each run proves one of its two answers.
#[test]
fn seq_is_the_rule_applied_to_the_checkout_built() {
    let shallow = git(&["rev-parse", "--is-shallow-repository"]);
    let count = git(&["rev-list", "--count", "HEAD"]);
    let want = crate::build_seq::choose(shallow.as_deref(), count.as_deref());
    let seq = identity()["seq"].clone();
    assert!(seq.is_u64(), "seq is a number: {seq}");
    assert_eq!(seq, want);
}

/// Catches `seq` written as a literal: a literal equal to today's count passes the check above
/// on a machine where the count is what was written, and is wrong on the next commit. The
/// one place the number may come from is the build's, so the source of `identity` must read it.
/// This reads the source text of `version.rs` itself, because in a shallow CI checkout the
/// right answer and a literal 0 are the same number and no run of the binary tells them apart.
#[test]
fn seq_is_read_from_the_build_not_written_down() {
    let source = include_str!("version.rs");
    let at = source
        .find("\"seq\".to_owned()")
        .expect("seq is inserted into the identity");
    let insert = &source[at..source[at..].find(");").map_or(source.len(), |e| at + e)];
    assert!(
        insert.contains("build_seq()"),
        "seq must be read from build_seq(): {insert}"
    );
}
