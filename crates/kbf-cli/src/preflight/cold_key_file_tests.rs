//! The check of `--cold-store-credentials-file`: with it the S3 key pair is a file, so the file
//! is checked and the environment is not. Each test goes from the flags to the report.

use super::tests::{Fake, needs_from};
use super::{Class, KeyRule, Needs, Report, check};

const FILE: &str = "/etc/kbf/cold.json";
const S3: &str = "--cold-store=s3://bucket/p?endpoint=http://127.0.0.1:9000";

/// The needs of a storage node over the S3 cold store, with the key file if `file`. The rule
/// over the legacy key file is set aside: it is another check.
fn storage(file: bool) -> Needs {
    let flag = format!("--cold-store-credentials-file={FILE}");
    let mut args = vec![
        "storage",
        S3,
        "--meta-dir=/m",
        "--hot-store=s3://bucket/p?endpoint=http://127.0.0.1:9000",
    ];
    if file {
        args.push(&flag);
    }
    let mut needs = needs_from(&args);
    needs.key = KeyRule::None;
    needs
}

fn refused(report: &Report, words: &[&str]) {
    assert_eq!(report.problems.len(), 1, "{report:#?}");
    assert_eq!(report.problems[0].class, Class::Refused, "{report:#?}");
    let text = report.render("storage");
    for word in words {
        assert!(text.contains(word), "{word} not in {text}");
    }
}

/// Catches: the environment still demanded when a key file is the key pair (a unit with a
/// key file and no `AWS_*` would be refused for it), and the file not checked: a good file
/// passes with no variable set, and the flag is among the files needed, as a secret.
#[test]
fn preflight_checks_the_key_file_not_the_environment() {
    let needs = storage(true);
    assert!(!needs.s3, "the environment's pair is not needed");
    let file = needs
        .files
        .iter()
        .find(|f| f.flag == "--cold-store-credentials-file")
        .expect("the key file is needed");
    assert!(file.secret);
    for mode in [0o600, 0o400] {
        let host = Fake::empty().file(FILE, mode, b"{}");
        assert_eq!(check(&needs, &host), Report::default(), "{mode:o}");
    }
}

/// Catches: a key file others can read, or that is not there, passing the check that exists
/// to refuse it before the node opens a store.
#[test]
fn a_key_file_others_can_read_or_that_is_missing_is_refused_with_the_fix() {
    let needs = storage(true);
    refused(
        &check(&needs, &Fake::empty().file(FILE, 0o644, b"{}")),
        &["--cold-store-credentials-file", FILE, "chmod 600"],
    );
    refused(
        &check(&needs, &Fake::empty()),
        &["--cold-store-credentials-file", FILE, "does not exist"],
    );
}

/// Catches: the key pair of the environment no longer checked when there is no key file (it
/// is deprecated, not gone): both variables are named.
#[test]
fn without_a_key_file_the_environment_is_still_checked() {
    let needs = storage(false);
    assert!(needs.s3);
    let report = check(&needs, &Fake::empty());
    assert_eq!(report.problems.len(), 2, "{report:#?}");
    let text = report.render("storage");
    assert!(text.contains("AWS_ACCESS_KEY_ID"), "{text}");
    assert!(text.contains("AWS_SECRET_ACCESS_KEY"), "{text}");
}
