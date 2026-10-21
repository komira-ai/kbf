//! Rule `traverse`, ported from `kbf-deploy linux check-units` (its `traverse_tests.rs`):
//! the directory `--hot-store=file://` names can be reached by the user kbf runs as.
//! Each test goes from the flags to the report, on a planted host.

use std::io::ErrorKind;

use super::super::host::Kind;
use super::super::tests::{DAEMON, Fake, needs_from};
use super::super::{Class, Report};
use super::check;

fn run(hot: &str, host: &Fake) -> Report {
    let flag = format!("--hot-store=file://{hot}");
    let needs = needs_from(&[
        "storage",
        "--storage-listen=127.0.0.1:0",
        "--cold-store=file:///var/lib/kbf-cold",
        "--meta-dir=/var/lib/kbf/meta",
        &flag,
    ]);
    let mut report = Report::default();
    check(&needs, host, &mut report);
    report
}

/// The one directory the report names as not enterable, or `None` for a clean report.
fn blocked(report: &Report) -> Option<String> {
    assert!(report.warnings.is_empty(), "{report:#?}");
    assert!(report.problems.len() <= 1, "{report:#?}");
    report.problems.first().map(|p| {
        assert_eq!(p.class, Class::Refused, "{p:?}");
        p.what.clone()
    })
}

fn tree(host: Fake) -> Fake {
    host.dir("/var", 0, 0, 0o755).dir("/var/lib", 0, 0, 0o755)
}

/// The failure seen live: `/var/lib/kbf-server` is the server account's, 0700, and the
/// process runs as another user. Catches a check that looks only at the directory itself
/// or at the owner class, and one that skips the ancestors.
#[test]
fn a_parent_the_user_cannot_enter_is_named() {
    let host = tree(Fake::empty()).dir("/var/lib/kbf-server", 996, 996, 0o700);
    let hot = "/var/lib/kbf-server/hot";
    let report = run(hot, &host.clone().running_as(1500, 1500, &[1500]));
    let what = blocked(&report).expect("refused");
    assert!(
        what.contains("--hot-store=file://") && what.contains(hot),
        "{what}"
    );
    assert!(
        what.contains("/var/lib/kbf-server (mode 0700, uid 996 gid 996)"),
        "{what}"
    );
    assert!(what.contains("uid 1500"), "{what}");
    let p = &report.problems[0];
    assert!(p.fix.contains("chmod o+x /var/lib/kbf-server"), "{}", p.fix);
    assert!(p.fix.contains("hot-store in kbf.toml"), "{}", p.fix);
    // The owner itself gets in.
    assert_eq!(blocked(&run(hot, &host.running_as(996, 996, &[996]))), None);
}

/// The class that matches decides, as in the kernel: an owner with no owner bit is
/// refused even when other has it; a group member needs the group bit; everyone else
/// needs other's. Catches a check that ORs the three classes.
#[test]
fn the_first_matching_class_decides() {
    let with = |bits| Fake::empty().dir("/d", 10, 20, bits);
    let run_as = |uid, gid, groups: &[u32], bits| {
        blocked(&run("/d/hot", &with(bits).running_as(uid, gid, groups)))
    };
    // owner, no owner x, other x
    assert_eq!(
        run_as(10, 99, &[99], 0o601)
            .as_deref()
            .map(|w| w.contains("/d (")),
        Some(true)
    );
    // group member
    assert_eq!(run_as(11, 99, &[99, 20], 0o710), None);
    assert!(run_as(11, 99, &[99, 20], 0o701).is_some());
    // primary gid counts even if groups omits it
    assert_eq!(run_as(11, 20, &[], 0o710), None);
    // other
    assert_eq!(run_as(11, 99, &[99], 0o701), None);
    assert!(run_as(11, 99, &[99], 0o750).is_some());
}

/// Directories that do not exist yet are made by the install, so they and everything
/// below are not judged; the hot directory itself is not judged either (it is made the
/// user's); a symbolic link (not a directory here) is not followed; nor is a file.
#[test]
fn missing_directories_the_dir_itself_links_and_files_are_not_judged() {
    let me = |h: Fake| h.running_as(1500, 1500, &[1500]);
    let host = me(Fake::empty().dir("/srv", 0, 0, 0o700));
    assert!(blocked(&run("/srv/x/hot", &host)).is_some());
    assert_eq!(blocked(&run("/var/lib/kbf/hot", &host)), None);
    let own = me(Fake::empty()
        .dir("/srv", 0, 0, 0o755)
        .dir("/srv/hot", 0, 0, 0o000));
    assert_eq!(blocked(&run("/srv/hot", &own)), None);
    let link = me(Fake::empty().put("/l", Kind::Link, 0, 0o000, b""));
    assert_eq!(blocked(&run("/l/hot", &link)), None);
    let file = me(Fake::empty().put("/f", Kind::File, 0, 0o000, b""));
    assert_eq!(blocked(&run("/f/hot", &file)), None);
}

/// Root enters every directory, so a process run as root is not judged (the install runs
/// `kbf --check` as the service user; as root there is no one to judge).
#[test]
fn root_enters_every_directory() {
    let host = Fake::empty()
        .dir("/srv", 5, 5, 0o000)
        .running_as(0, 0, &[0]);
    assert_eq!(blocked(&run("/srv/hot", &host)), None);
}

/// A directory the process cannot stat is a warning, not a refusal, and the walk stops:
/// the files check and the mode's own start say the rest.
#[test]
fn a_directory_that_cannot_be_read_is_a_warning() {
    let host = Fake::empty()
        .dir("/srv", 0, 0, 0o755)
        .unreadable("/srv/x", ErrorKind::PermissionDenied)
        .dir("/srv/x/y", 0, 0, 0o000)
        .running_as(1500, 1500, &[1500]);
    let report = run("/srv/x/y/hot", &host);
    assert!(report.problems.is_empty(), "{report:#?}");
    assert_eq!(report.warnings.len(), 1, "{report:#?}");
    assert!(
        report.warnings[0].contains("traverse")
            && report.warnings[0].contains("/srv/x")
            && report.warnings[0].contains("--hot-store"),
        "{report:#?}"
    );
}

/// Catches the rule reading a store that is not a directory, and a mode with no hot
/// store: an `s3://` hot store has no ancestors, and `--hot-store` left out names none.
#[test]
fn only_a_file_hot_store_is_read() {
    let host = Fake::empty()
        .dir("/srv", 0, 0, 0o000)
        .running_as(1500, 1500, &[1500]);
    let mut report = Report::default();
    for flags in [
        [
            "storage",
            "--hot-store=s3://b/p?endpoint=http://127.0.0.1:1",
        ],
        ["storage", "--meta-dir=/srv/meta"],
    ] {
        let mut all = vec![flags[0], "--storage-listen=127.0.0.1:0", flags[1]];
        all.push("--cold-store=file:///srv/cold");
        check(&needs_from(&all), &host, &mut report);
    }
    assert_eq!(report, Report::default());
}

/// Catches the flag of the all-in-one mode (its server half's `--hot-store`) not
/// reaching the rule.
#[test]
fn the_all_in_one_hot_store_is_read_too() {
    let mut flags = vec![
        "all-in-one",
        "--meta-dir=/srv/meta",
        "--hot-store=file:///srv/hot",
    ];
    flags.extend(DAEMON);
    let needs = needs_from(&flags);
    let host = Fake::empty()
        .dir("/srv", 0, 0, 0o000)
        .running_as(1500, 1500, &[1500]);
    let mut report = Report::default();
    check(&needs, &host, &mut report);
    assert!(blocked(&report).is_some(), "{report:#?}");
}
