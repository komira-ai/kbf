//! The `seq` and the `commit_full` of the build identity.

use std::process::Command;

use super::*;

/// A full commit as the build embeds it: 40 lowercase hex digits.
const FULL: &str = "0123456789abcdef0123456789abcdef01234567";

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
/// repositories (`kbf-server/src/build_seq.rs`); here it meets the live checkout, which is a full clone
/// on a person's machine and a shallow one in CI, so each run proves one of its two answers.
#[test]
fn seq_is_the_rule_applied_to_the_checkout_built() {
    let shallow = git(&["rev-parse", "--is-shallow-repository"]);
    let count = git(&["rev-list", "--count", "HEAD"]);
    let want = kbf_server::build_seq::choose(shallow.as_deref(), count.as_deref());
    let seq = identity()["seq"].clone();
    assert!(seq.is_u64(), "seq is a number: {seq}");
    assert_eq!(seq, want);
}

/// Catches the capability left out of `kbf --version --json`, or printed as something other than
/// the build's own constant: the installer reads it before it takes `bundled`, and a `kbf` that
/// does not declare the relay is one the frontend cannot learn from that a host fetches nothing.
/// The line carries it in the sorted place between `protocol` and `seq`.
///
/// Catches: the key removed; the key written as `0`; the key written as text.
#[test]
fn relay_is_the_level_this_build_declares() {
    assert_eq!(identity()["relay"], serde_json::json!(RELAY));
    let line = line();
    let at = |key: &str| {
        line.find(&format!("\"{key}\":"))
            .unwrap_or_else(|| panic!("{key} is printed: {line}"))
    };
    assert!(at("protocol") < at("relay"), "{line}");
    assert!(at("relay") < at("seq"), "{line}");
}

/// Catches the full commit left out of `kbf --version --json` when the build knows it, or
/// printed as something other than what the build knows (shortened, quoted twice, another
/// field's text). The line carries it in the sorted place between `commit` and `meta_log`.
///
/// Catches: the key never inserted; the key written as `BUILD_COMMIT` (12 digits).
#[test]
fn commit_full_is_printed_when_the_build_knows_it() {
    let said = identity_for(FULL);
    assert_eq!(said["commit_full"], FULL);
    assert_eq!(said["commit_full"].as_str().map(str::len), Some(40));
    let line = said.to_string();
    let at = |key: &str| {
        line.find(&format!("\"{key}\":"))
            .unwrap_or_else(|| panic!("{key} is printed: {line}"))
    };
    assert!(at("commit") < at("commit_full"), "{line}");
    assert!(at("commit_full") < at("meta_log"), "{line}");
}

/// Catches an unknown full commit printed as the empty text: a reader would take `""` for a
/// commit, where an absent field says the build does not know it. Every other field is still
/// printed.
///
/// Catches: the `is_empty` test dropped (the key is then printed as `""`).
#[test]
fn commit_full_is_absent_when_the_build_does_not_know_it() {
    let said = identity_for("");
    assert!(said.get("commit_full").is_none(), "{said}");
    assert!(said.get("commit").is_some(), "{said}");
    let count = |v: &Value| v.as_object().map_or(0, Map::len);
    assert_eq!(count(&said) + 1, count(&identity_for(FULL)));
}

/// Catches `identity` handing `identity_for` a full commit of its own (a literal, or the empty
/// text) instead of the one the server library embeds. A build that stamped its commit
/// (`KBF_BUILD_COMMIT_OVERRIDE`) embeds none, so the empty text and the constant agree there
/// and only a build from a checkout tells them apart.
///
/// Catches: `identity` calling `identity_for("")` (red in a build from a checkout).
#[test]
fn identity_prints_the_full_commit_the_server_embeds() {
    assert_eq!(identity(), identity_for(kbf_server::BUILD_COMMIT_FULL));
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
