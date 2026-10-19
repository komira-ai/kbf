//! The check of `--proxy-auth-file`: the file `kbf-server` would refuse at its start is
//! refused here first, with the fix. Each test goes from the flags to the report, so a
//! need that never reaches the check shows too.

use std::path::PathBuf;

use super::tests::{Fake, ME};
use super::{Class, Needs, Report, check, needs};

const SECRET: &str = "/etc/kbf/proxy-secret";

fn frontend() -> Needs {
    let flag = format!("--proxy-auth-file={SECRET}");
    let argv = [
        "kbf",
        "frontend",
        "--storage=http://127.0.0.1:8983",
        flag.as_str(),
    ];
    needs(&crate::parse_from(argv).expect("flags parse"))
}

fn host(bytes: &[u8], mode: u32) -> Fake {
    Fake::empty().file(SECRET, mode, bytes)
}

/// The one problem the check finds, which must be refused and say each of `words`.
fn refused(report: &Report, words: &[&str]) {
    assert_eq!(report.problems.len(), 1, "{report:#?}");
    assert_eq!(report.problems[0].class, Class::Refused, "{report:#?}");
    let text = report.render("frontend");
    for word in words {
        assert!(text.contains(word), "{word} not in {text}");
    }
}

/// Catches: the flag's file not checked at all, or checked as any secret is: it must also
/// be exactly 32 bytes.
#[test]
fn the_flag_names_a_secret_of_32_bytes() {
    let n = frontend();
    let file = n
        .files
        .iter()
        .find(|f| f.flag == "--proxy-auth-file")
        .expect("the secret is needed");
    assert!(file.secret);
    assert_eq!(file.exact_len, Some(32));
    assert_eq!(file.path, PathBuf::from(SECRET));
}

/// Catches: the size check dropped or off by one, in either direction.
///
/// Mutants seen red: the size check removed; `!=` as `<`; `!=` as `>`.
#[test]
fn a_secret_that_is_not_32_bytes_is_refused_with_its_size_and_the_fix() {
    for size in [0, 1, 31, 33, 64] {
        refused(
            &check(&frontend(), &host(&vec![7; size], 0o600)),
            &[
                "--proxy-auth-file",
                SECRET,
                &format!("holds {size} bytes, not exactly 32"),
                "head -c 32 /dev/urandom",
            ],
        );
    }
}

/// Catches: a secret written with a trailing newline (`echo`), which is 33 bytes.
#[test]
fn a_secret_with_a_newline_after_32_bytes_is_refused() {
    let mut bytes = vec![7; 32];
    bytes.push(b'\n');
    refused(
        &check(&frontend(), &host(&bytes, 0o600)),
        &["holds 33 bytes"],
    );
}

#[test]
fn a_secret_of_32_bytes_passes_at_0600_and_at_0400() {
    for mode in [0o600, 0o400] {
        assert_eq!(
            check(&frontend(), &host(&[7; 32], mode)),
            Report::default(),
            "{mode:o}"
        );
    }
}

/// Catches: the mode check dropped, or loosened to part of the group and others bits.
///
/// Mutants seen red: the mode test removed; its mask cut to the others bits; its mask cut
/// to the group bits.
#[test]
fn a_secret_the_group_or_others_can_reach_is_refused_with_the_chmod() {
    for mode in [0o640, 0o660, 0o604, 0o606, 0o644, 0o670, 0o601, 0o610] {
        refused(
            &check(&frontend(), &host(&[7; 32], mode)),
            &["--proxy-auth-file", &format!("{mode:04o}"), "chmod 600"],
        );
    }
}

#[test]
fn a_secret_the_owner_cannot_read_is_refused_once() {
    for mode in [0o000, 0o200] {
        refused(
            &check(&frontend(), &host(&[7; 32], mode)),
            &["cannot read", "chmod 600"],
        );
    }
}

#[test]
fn a_secret_owned_by_another_user_is_refused() {
    refused(
        &check(&frontend(), &host(&[7; 32], 0o600).chmod(SECRET, 0, 0o600)),
        &["uid 0", &format!("uid {ME}"), &format!("chown {ME}")],
    );
}

/// Catches: a missing file passing the check (the server would stop at its start).
#[test]
fn a_missing_secret_is_refused() {
    refused(
        &check(&frontend(), &Fake::empty()),
        &["--proxy-auth-file", SECRET, "does not exist"],
    );
}
