//! What the migration refuses, and that it refuses before it has changed anything.

use std::fs;
use std::os::unix::fs::symlink;

use serde_json::json;

use super::rig::*;
use super::*;
use crate::fixtures::{FakeRunner, ran, version_json};
use crate::testkit::{A, B, C};

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

/// Run a migration that must be refused; nothing under the root may differ afterwards
/// except where the test says, and no service was touched.
fn refused(rig: &mut Rig) -> MigrateError {
    let before = rig.snapshot("");
    let error = run_migrate(rig, &never(), Mode::Apply).expect_err("refused");
    assert_eq!(rig.snapshot(""), before, "a refusal changes nothing");
    assert!(rig.events.borrow().is_empty() && rig.host.log.is_empty());
    error
}

/// Catches a move that takes a release out from under a swap on trial: with a pending
/// confirm in the old state the migration refuses (needs the operator, exit 5), whichever release it is.
///
/// Mutant seen red: the `pending_confirm` check removed from `migrate`.
#[test]
fn a_swap_on_trial_refuses_the_migration() {
    let mut rig = Rig::linux("pending");
    rig.write_old_state(Some(A), Some(B), None, Some(A));
    let error = refused(&mut rig);
    assert!(
        matches!(error, MigrateError::Pending { ref commit } if commit == A),
        "{error}"
    );
    assert_eq!(error.exit().code(), 5);
}

/// Catches a migration that guesses when the old layout disagrees with itself: the state
/// names a `current` the link does not (an interrupted old verb), or the link is not
/// `releases/<commit>` at all.
///
/// Mutant seen red: `check_links` removed.
#[test]
fn links_that_disagree_with_the_state_or_are_not_releases_are_refused() {
    let mut rig = Rig::linux("disagree");
    rig.write_old_state(Some(B), Some(A), None, None);
    let error = refused(&mut rig);
    assert!(matches!(error, MigrateError::Disagree(_)), "{error}");
    assert!(
        error.to_string().contains("state.json says current is"),
        "{error}"
    );
    assert_eq!(error.exit().code(), 5);

    let mut rig = Rig::linux("badlink");
    fs::remove_file(rig.at("usr/local/kbf/current")).expect("rm");
    symlink("../../../etc", rig.at("usr/local/kbf/current")).expect("link");
    let error = refused(&mut rig);
    assert!(
        matches!(
            error,
            MigrateError::BadLink {
                link: "current",
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(error.exit().code(), 3);

    let mut rig = Rig::linux("missing-current");
    fs::remove_file(rig.at("usr/local/kbf/previous")).expect("rm");
    let error = refused(&mut rig);
    assert!(error.to_string().contains("the link is missing"), "{error}");
}

/// Catches the commit taken from a name: a directory called `A` that holds the build of
/// `C` is refused (naming both), as is every way the probe can fail: it cannot start, it
/// times out, exits non-zero, prints no JSON, or is a debug build.
///
/// Mutant seen red: the probe's `check` replaced by trusting the directory name.
#[test]
fn the_commit_comes_from_the_binary_and_a_failed_probe_refuses() {
    let mut rig = Rig::linux("mislabeled");
    let lying = FakeRunner::new(|_| Ok(ran(0, &version_json(C, "release"), "")));
    let before = rig.snapshot("");
    let error =
        run_with(&mut rig, &lying, &never(), &NoFrontend, Mode::Apply).expect_err("refused");
    assert!(
        matches!(error, MigrateError::Mislabeled { ref probed, .. } if probed == &C[..12]),
        "{error}"
    );
    assert_eq!(error.exit().code(), 3);
    assert_eq!(rig.snapshot(""), before);

    type Answer = fn() -> std::io::Result<crate::run::Ran>;
    let cases: [(&str, Answer); 5] = [
        ("entity not found", || {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        }),
        ("no answer", || {
            Ok(crate::run::Ran {
                code: None,
                stdout: vec![],
                stderr: vec![],
                timed_out: true,
            })
        }),
        ("exit status", || Ok(ran(1, "", "boom"))),
        ("printed no JSON", || Ok(ran(0, "kbf 0.1\n", ""))),
        ("not \"release\"", || {
            Ok(ran(0, &version_json(A, "debug"), ""))
        }),
    ];
    for (why, answer) in cases {
        let mut rig = Rig::linux("probe-fails");
        let runner = FakeRunner::new(move |_| answer());
        let error =
            run_with(&mut rig, &runner, &never(), &NoFrontend, Mode::Apply).expect_err("refused");
        assert!(
            matches!(error, MigrateError::Probe { .. }),
            "{why}: {error}"
        );
        assert!(error.to_string().contains(why), "{why}: {error}");
        assert!(rig.events.borrow().is_empty());
    }
}

/// Catches an old release copied that an unprivileged user could have shaped: a link
/// inside it, a `VERIFIED` marker at its top (which would make a copy count as verified),
/// a file with a second link, and a group-writable file are each refused before a
/// service is stopped.
///
/// Mutants seen red: `reserved` empty; `NOFOLLOW` removed from `entry`.
#[test]
fn an_old_release_that_is_not_plain_files_is_refused_before_anything_changes() {
    let release = |c: &str| format!("usr/local/kbf/releases/{c}");
    let mut rig = Rig::linux("tree-link");
    symlink("/etc/passwd", rig.at(&release(A)).join("passwd")).expect("link");
    let error = refused(&mut rig);
    assert!(matches!(error, MigrateError::Tree(_)), "{error}");

    let mut rig = Rig::linux("tree-marker");
    put(&rig.at(&release(B)).join("VERIFIED"), "", 0o644);
    let error = refused(&mut rig);
    assert!(
        error.to_string().contains("only a check may write"),
        "{error}"
    );

    let mut rig = Rig::linux("tree-hardlink");
    fs::hard_link(
        rig.at(&release(A)).join("kbf"),
        rig.at(&release(A)).join("kbf2"),
    )
    .expect("link");
    assert!(refused(&mut rig).to_string().contains("another hard link"));

    let mut rig = Rig::linux("tree-writable");
    put(&rig.at(&release(A)).join("units/x"), "", 0o666);
    assert!(
        refused(&mut rig)
            .to_string()
            .contains("writable by group or other")
    );
}

/// Catches an apply by a user who does not own the tree: it is refused (usage, exit 2)
/// before anything changes, since the new files would not be root's.
///
/// Mutant seen red: the euid check removed.
#[test]
fn only_the_owner_of_the_tree_may_apply() {
    let mut rig = Rig::linux("notroot");
    rig.host.euid = me() + 1;
    let error = refused(&mut rig);
    assert!(matches!(error, MigrateError::NotRoot { .. }), "{error}");
    assert_eq!(error.exit().code(), 2);
    assert!(
        run_migrate(&mut rig, &never(), Mode::DryRun).is_ok(),
        "a dry run needs no root"
    );
}

/// Catches two managers: the installer's `state.json` beside the old state with no
/// migration behind it is refused (the migration will not pick one), and so is a journal
/// whose old state has gone.
#[test]
fn a_new_state_without_a_migration_and_a_journal_without_old_state_are_refused() {
    let mut rig = Rig::linux("conflict");
    fs::create_dir_all(rig.new_state_dir()).expect("the installer's directory");
    crate::state::Locked::acquire(&rig.layout)
        .expect("lock")
        .write(&crate::state::State::default())
        .expect("the installer's state");
    let error = refused(&mut rig);
    assert!(matches!(error, MigrateError::Conflict(_)), "{error}");

    let mut rig = Rig::linux("gone");
    rig.fail_start.set(true);
    run_migrate(&mut rig, &never(), Mode::Apply).expect_err("start fails");
    fs::remove_file(rig.at("var/lib/kbf-deploy/state.json")).expect("rm");
    fs::remove_file(rig.at("var/lib/kbf-deploy/helper.json")).expect("rm");
    let error = run_migrate(&mut rig, &never(), Mode::Apply).expect_err("refused");
    assert!(matches!(error, MigrateError::Disagree(_)), "{error}");
}

/// Catches a file of the migration that is read as something else when it is damaged: a
/// garbage `migration.json`, `state.json` or `helper.json` stops the run (exit 1) naming the
/// file, and is never treated as absent.
#[test]
fn damaged_files_stop_the_run_naming_the_file() {
    let mut rig = Rig::linux("journal-bad");
    put(
        &rig.new_state_dir().join("migration.json"),
        "not json",
        0o600,
    );
    let error = refused(&mut rig);
    assert!(
        matches!(
            error,
            MigrateError::Corrupt {
                file: "migration.json",
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(error.exit().code(), 1);

    let mut rig = Rig::linux("state-bad");
    rig.fail_start.set(true);
    run_migrate(&mut rig, &never(), Mode::Apply).expect_err("start fails");
    fs::write(rig.new_state_dir().join("state.json"), "{\"schema\": 9}").expect("write");
    let error = run_migrate(&mut rig, &never(), Mode::Apply).expect_err("refused");
    assert!(
        matches!(
            error,
            MigrateError::State(crate::state::StateError::Schema { .. })
        ),
        "{error}"
    );
    assert_eq!(error.exit().code(), 1);
    // A rollback is not held up by a state of another schema either: it is the installer's
    // to judge, so the rollback says what it is and changes nothing.
    let m = rig.migration();
    let before = rig.snapshot("");
    let error = rollback(&m, &mut rig.host, Mode::Apply).expect_err("refused");
    assert!(
        matches!(
            error,
            MigrateError::State(crate::state::StateError::Schema { .. })
        ),
        "{error}"
    );
    assert_eq!(rig.snapshot(""), before);

    let mut rig = Rig::linux("memory-bad");
    put(
        &rig.at("var/lib/kbf-deploy/helper.json"),
        "{\"nope\": 1}",
        0o600,
    );
    let error = refused(&mut rig);
    assert!(
        matches!(
            error,
            MigrateError::Corrupt {
                file: "helper.json",
                ..
            }
        ),
        "{error}"
    );

    // The one key the deployed helper wrote that this build dropped is forgiven, and only
    // that one: beside another unknown key, or in a file that is no object, it still stops.
    for (name, text) in [
        ("memory-extra", "{\"handled_entry\": \"2-x\", \"nope\": 1}"),
        ("memory-array", "[1]"),
    ] {
        let mut rig = Rig::linux(name);
        put(&rig.at("var/lib/kbf-deploy/helper.json"), text, 0o600);
        let error = refused(&mut rig);
        assert!(
            matches!(
                error,
                MigrateError::Corrupt {
                    file: "helper.json",
                    ..
                }
            ),
            "{name}: {error}"
        );
    }

    let mut rig = Rig::linux("old-state-bad");
    // The installer's own schema in the old directory is not the old tools' state, whether
    // the file has the old keys or not.
    for text in [
        "{\"schema\": 2}",
        "{\"schema\": 2, \"current\": null, \"previous\": null, \"staged\": null, \"pending_confirm\": null, \"order_floor\": 0}",
        "{\"schema\": 1, \"current\": null, \"previous\": null, \"staged\": null, \"pending_confirm\": null, \"order_floor\": 0, \"floor\": 1}",
    ] {
        put(&rig.at("var/lib/kbf-deploy/state.json"), text, 0o600);
        let error = refused(&mut rig);
        assert!(
            matches!(
                error,
                MigrateError::Corrupt {
                    file: "state.json",
                    ..
                }
            ),
            "{text}: {error}"
        );
    }
}

/// Catches a floor seeded from nothing: the frontend as the source and unreachable, with no
/// `--min-generation`, refuses (needs the operator, exit 5) before anything changes.
#[test]
fn an_unreachable_frontend_without_a_flag_refuses_before_any_change() {
    let mut rig = Rig::linux("nofloor");
    let mut m = rig.migration();
    m.seed = Seed {
        source: Source::Frontend,
        min_generation: None,
    };
    let before = rig.snapshot("");
    let error = migrate(
        &m,
        &mut rig.host,
        &Hooks {
            runner: &runner(),
            verifier: &never(),
            frontend: &NoFrontend,
        },
        Mode::Apply,
    )
    .expect_err("refused");
    assert!(
        matches!(error, MigrateError::FloorUnavailable(_)),
        "{error}"
    );
    assert_eq!(error.exit().code(), 5);
    assert_eq!(rig.snapshot(""), before);
}

/// Catches a staged release that vanishes silently: it is not carried over (a staged
/// release was verified by the old rules), and the report says to stage it again.
#[test]
fn a_staged_release_is_not_carried_and_the_report_says_so() {
    let mut rig = Rig::linux("staged");
    rig.write_old_state(Some(A), Some(B), Some(C), None);
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains(C) && w.contains("stage it again"))
    );
    assert_eq!(
        rig.json(&rig.new_state_dir().join("state.json"))["staged"],
        json!(null)
    );
}

/// Catches the exit codes drifting from the contract (1 failed, 2 usage, 3 refused, 5
/// needs the operator; a swap of the old tool on trial is one of those, not a busy lock): one error of each kind.
#[test]
fn every_error_has_the_contract_exit_code() {
    let io = || std::io::Error::other("x");
    let cases: Vec<(MigrateError, u8)> = vec![
        (MigrateError::NotRoot { have: 1, want: 0 }, 2),
        (
            MigrateError::BadLink {
                link: "current",
                target: String::new(),
            },
            3,
        ),
        (
            MigrateError::Mislabeled {
                dir: String::new(),
                probed: String::new(),
            },
            3,
        ),
        (
            MigrateError::Probe {
                release: String::new(),
                why: String::new(),
            },
            3,
        ),
        (
            MigrateError::Pending {
                commit: String::new(),
            },
            5,
        ),
        (
            MigrateError::State(crate::state::StateError::Unusable {
                primary: String::new(),
                backup: String::new(),
            }),
            5,
        ),
        (
            MigrateError::State(crate::state::StateError::Busy {
                path: std::path::PathBuf::new(),
            }),
            4,
        ),
        (MigrateError::Disagree(String::new()), 5),
        (MigrateError::Changed(String::new()), 5),
        (MigrateError::FloorUnavailable(String::new()), 5),
        (MigrateError::Conflict(String::new()), 5),
        (MigrateError::Unmigrated(String::new()), 5),
        (MigrateError::MovedOn(String::new()), 5),
        (MigrateError::NoJournal, 5),
        (
            MigrateError::Corrupt {
                file: "x",
                why: String::new(),
            },
            1,
        ),
        (
            MigrateError::Io {
                what: String::new(),
                source: io(),
            },
            1,
        ),
    ];
    for (error, code) in cases {
        assert_eq!(error.exit().code(), code, "{error}");
    }
}

/// Catches an I/O failure reported as something else: an old tree that is not there (the
/// host's old state exists but `/usr/local/kbf` was removed) stops the run (exit 1) naming
/// the path, before anything changes.
#[test]
fn a_missing_old_tree_is_an_io_error_naming_the_path() {
    let mut rig = Rig::linux("no-tree");
    fs::remove_dir_all(rig.at("usr/local/kbf")).expect("remove");
    let error = refused(&mut rig);
    assert!(matches!(error, MigrateError::Io { .. }), "{error}");
    assert!(error.to_string().contains("/usr/local/kbf"), "{error}");
    assert_eq!(error.exit().code(), 1);
}
