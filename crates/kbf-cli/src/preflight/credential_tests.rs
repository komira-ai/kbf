//! Rule `credential`, ported from `kbf-deploy linux check-units` (its `credential_tests.rs`),
//! the half that reads the host. The units are static now and name no `EnvironmentFile=`,
//! so kbf reads what the process has instead of what a unit line says:
//!
//! - a mode with no storage half (`daemon`, and `all-in-one` with a file cold store) has no
//!   `AWS_*` variable in its environment, whatever file or unit line put it there
//!   (check-units read each `EnvironmentFile=` for one);
//! - the storage node's key file, `s3.env` beside `kbf.toml` (check-units read the file the
//!   unit's `EnvironmentFile=` named), is a plain file, root's or the user kbf runs as, with
//!   no group or other bit; the mode that holds the key beside its builds
//!   (`storage-and-daemon`) has it root's alone, mode 0600.
//!
//! Each test goes from the flags to the report, on a planted host.

use std::io::ErrorKind;

use super::super::host::Kind;
use super::super::tests::{DAEMON, Fake, needs_from};
use super::super::{Class, Report};
use super::check;

const KEY: &str = "/etc/kbf/s3.env";
/// The user the storage node runs as.
const SVC: u32 = 990;

const STORAGE: [&str; 6] = [
    "--cold-store=s3://bucket/p?endpoint=http://127.0.0.1:9000",
    "--hot-store=file:///var/lib/kbf-hot",
    "--meta-dir=/var/lib/kbf/meta",
    "--storage-tls-cert=/c/storage.pem",
    "--storage-tls-key=/c/storage.key",
    "--storage-client-ca=/c/ca.pem",
];

fn run(mode: &str, parts: &[&[&str]], host: &Fake) -> Report {
    let mut flags = vec![mode];
    flags.extend(parts.iter().flat_map(|p| p.iter().copied()));
    let mut report = Report::default();
    check(&needs_from(&flags), host, &mut report);
    report
}

fn storage(host: &Fake) -> Report {
    run("storage", &[&STORAGE[..]], host)
}

fn both(host: &Fake) -> Report {
    run("storage-and-daemon", &[&STORAGE[..], &DAEMON[..]], host)
}

fn daemon(host: &Fake) -> Report {
    run("daemon", &[&DAEMON[..]], host)
}

/// The what of each problem; every one refused.
fn whats(report: &Report) -> Vec<String> {
    assert!(report.warnings.is_empty(), "{report:#?}");
    assert!(report.problems.iter().all(|p| p.class == Class::Refused));
    report.problems.iter().map(|p| p.what.clone()).collect()
}

/// Catches a build node whose environment holds a copy of the key passed (check-units read
/// the `EnvironmentFile=` for one): the key pair, `AWS_SESSION_TOKEN` and the variables that
/// point an SDK at a key elsewhere are all refused, once, with every name; other variables
/// (and a name that merely starts with `AWS`) are not.
#[test]
fn a_build_node_with_an_aws_variable_is_refused() {
    let host = Fake::empty()
        .var("AWS_SECRET_ACCESS_KEY", "k")
        .var("AWS_ACCESS_KEY_ID", "a")
        .var("AWS_PROFILE", "p")
        .var("AWSX", "no")
        .var("RUST_LOG", "info");
    let report = daemon(&host);
    let what = whats(&report);
    assert_eq!(what.len(), 1, "{report:#?}");
    assert!(
        what[0].contains("AWS_ACCESS_KEY_ID, AWS_PROFILE, AWS_SECRET_ACCESS_KEY")
            && !what[0].contains("AWSX"),
        "{what:?}"
    );
    assert!(
        report.problems[0]
            .why
            .contains("build node holds no credential"),
        "{report:#?}"
    );
    assert_eq!(
        daemon(&Fake::empty().var("AWSX", "x").var("RUST_LOG", "info")),
        Report::default()
    );
    assert_eq!(daemon(&Fake::empty()), Report::default());
}

/// Catches the rule run on the wrong modes: the storage node's half reads the key, so the
/// variables are its business, and an all-in-one with an S3 cold store holds the key on
/// purpose (and is told so by the `s3` check); an all-in-one with a file cold store has no
/// use for one.
#[test]
fn only_a_mode_with_no_key_is_checked_for_aws_variables() {
    let host = Fake::empty()
        .var("AWS_ACCESS_KEY_ID", "a")
        .var("AWS_SECRET_ACCESS_KEY", "k");
    assert_eq!(storage(&host), Report::default());
    let aio = |cold: &str| {
        run(
            "all-in-one",
            &[&DAEMON[..], &["--meta-dir=/m", cold][..]],
            &host,
        )
    };
    assert_eq!(
        whats(&aio("--cold-store=file:///var/lib/kbf-cold")).len(),
        1
    );
    assert_eq!(
        aio("--cold-store=s3://b/p?endpoint=http://127.0.0.1:9000"),
        Report::default()
    );
    // The frontend has no rule.
    let mut report = Report::default();
    check(
        &needs_from(&["frontend", "--storage=http://127.0.0.1:8983"]),
        &host,
        &mut report,
    );
    assert_eq!(report, Report::default());
}

fn key_file(host: Fake, bits: u32, uid: u32) -> Fake {
    host.put(KEY, Kind::File, uid, bits, b"AWS_ACCESS_KEY_ID=a\n")
}

/// Catches the key holder's file not read: a key other users can read, a key another user
/// owns, or a link in its place passed; a root-owned 0600 key refused (systemd reads it as
/// root); and the process's own user not told from another one.
#[test]
fn the_key_holders_file_is_root_or_its_users_alone() {
    let me = Fake::empty().running_as(SVC, SVC, &[SVC]);
    for (bits, uid, want) in [
        (0o600, SVC, Vec::<&str>::new()),
        (0o600, 0, vec![]),
        (0o400, SVC, vec![]),
        (
            0o640,
            SVC,
            vec!["/etc/kbf/s3.env is mode 0640, so users other than uid 990 can read the key"],
        ),
        (
            0o604,
            0,
            vec!["/etc/kbf/s3.env is mode 0604, so users other than uid 990 can read the key"],
        ),
        (
            0o600,
            1000,
            vec![
                "/etc/kbf/s3.env is uid 1000's, neither root's nor uid 990's, the user kbf runs as: that user can read or replace the key",
            ],
        ),
    ] {
        let report = storage(&key_file(me.clone(), bits, uid));
        let what = whats(&report);
        assert_eq!(what.len(), want.len(), "{bits:o} {uid}: {report:#?}");
        for (got, want) in what.iter().zip(&want) {
            assert!(got.contains(want), "{bits:o} {uid}: {got}");
        }
        for p in &report.problems {
            assert!(
                p.fix.contains("chmod 600") || p.fix.contains("chown"),
                "{p:?}"
            );
        }
    }
    // Both defects at once: both named, the mode first.
    let report = storage(&key_file(me, 0o644, 1000));
    assert_eq!(whats(&report).len(), 2, "{report:#?}");
}

/// Catches a link or a directory in the key's place passed (the check does not follow
/// links, so it cannot know what the key is), a missing key file refused (a node that gets
/// its key from the frontend has none, and that is fine), and a key path the process cannot
/// stat passed without a word.
#[test]
fn a_key_that_is_not_a_plain_file_is_refused_and_a_missing_or_unreadable_one_is_not() {
    let me = Fake::empty().running_as(SVC, SVC, &[SVC]);
    for kind in [Kind::Link, Kind::Dir, Kind::Other] {
        let report = storage(&me.clone().put(KEY, kind, SVC, 0o600, b""));
        let what = whats(&report);
        assert_eq!(what.len(), 1, "{report:#?}");
        assert!(
            what[0].contains("is not a plain file (a link or a directory)"),
            "{what:?}"
        );
        let report = both(&me.clone().put(KEY, kind, 0, 0o600, b""));
        assert_eq!(whats(&report).len(), 1, "{report:#?}");
    }
    assert_eq!(storage(&me), Report::default());
    assert_eq!(both(&me), Report::default());
    for run_it in [storage, both] {
        let report = run_it(&me.clone().unreadable(KEY, ErrorKind::PermissionDenied));
        assert!(report.problems.is_empty(), "{report:#?}");
        assert_eq!(report.warnings.len(), 1, "{report:#?}");
        assert!(
            report.warnings[0].contains(KEY) && report.warnings[0].contains("not checked"),
            "{report:#?}"
        );
    }
}

/// Catches the key file of `storage-and-daemon` read as the storage unit's: it must be a
/// plain file, root's, with no group or other bit (the process runs as the build user, so a
/// file that user owns, or that is group-readable, is the key in a build's reach).
#[test]
fn the_key_beside_builds_is_roots_alone() {
    let me = Fake::empty().running_as(1500, 1500, &[1500]);
    for (bits, uid, want) in [
        (0o600, 0, Vec::<&str>::new()),
        (0o400, 0, vec![]),
        (
            0o600,
            1500,
            vec!["/etc/kbf/s3.env is uid 1500's, not root's: that user can replace the key"],
        ),
        (
            0o640,
            0,
            vec!["/etc/kbf/s3.env is mode 0640, so other users can read the key"],
        ),
        (
            0o644,
            1500,
            vec![
                "/etc/kbf/s3.env is uid 1500's, not root's: that user can replace the key",
                "/etc/kbf/s3.env is mode 0644, so other users can read the key",
            ],
        ),
    ] {
        let report = both(&key_file(me.clone(), bits, uid));
        let what = whats(&report);
        assert_eq!(what.len(), want.len(), "{bits:o} {uid}: {report:#?}");
        for (got, want) in what.iter().zip(&want) {
            assert!(got.contains(want), "{bits:o} {uid}: {got}");
        }
    }
}

/// The key file is `s3.env` beside the default `kbf.toml`, and a frontend or a daemon
/// reads none.
#[test]
fn the_key_file_is_beside_kbf_toml() {
    assert_eq!(super::super::key_file(), std::path::PathBuf::from(KEY));
    let bad = key_file(Fake::empty(), 0o666, 1000);
    assert_eq!(daemon(&bad), Report::default());
}
