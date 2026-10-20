//! `migrate` and the fetch account (`_kbf-fetch` on a Mac): the version probe needs it, and
//! only `setup` used to make it, while `setup` needed the migration first. The first step of an
//! applying migration makes it with the plan `setup` uses. Each test says the defect it catches.

use std::cell::Cell;
use std::rc::Rc;

use clap::Parser;
use kbf_host::testkit::{FakeExec, reply};

use super::*;
use crate::cli::Cli;
use crate::migrate::kit_host::Form;
use crate::migrate::rig::{Rig, me};

/// The fetch account's directory-service record (`dscl`), written split so the repository's
/// home-path lint does not read it as a home directory (as setup/tests.rs does).
const RECORD: &str = concat!("/User", "s/_kbf-fetch");

fn migrate_args(extra: &[&str]) -> MigrateArgs {
    let mut full = vec!["kbf-installer", "migrate"];
    full.extend_from_slice(extra);
    match Cli::try_parse_from(full).expect("parses").verb {
        Verb::Migrate(args) => args,
        _ => panic!("not the migrate verb"),
    }
}

/// How the Mac's account database answers: `exists` is whether `_kbf-fetch` is there, and its
/// uid, home and shell. Creating the account (the `IsHidden` step, its last) makes it exist
/// with the shape `setup` makes.
struct Accounts {
    made: Rc<Cell<bool>>,
}

fn mac_accounts(
    exists: bool,
    uid: u32,
    home: &'static str,
    shell: &'static str,
) -> (FakeExec, Accounts) {
    let made = Rc::new(Cell::new(false));
    let seen = made.clone();
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(move |cmd| {
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        let program = cmd.program.as_str();
        if program.ends_with("dscl") {
            return Ok(match args.as_slice() {
                [".", "-create", RECORD, "IsHidden", _] => {
                    seen.set(true);
                    reply(0, "", "")
                }
                [".", "-list", "/Users", "UniqueID"] => reply(0, "root 0\n_other 301\n", ""),
                [".", "-list", "/Groups", "PrimaryGroupID"] => reply(0, "wheel 0\n", ""),
                [".", "-read", RECORD, "UserShell"] => {
                    let shell = if seen.get() { "/usr/bin/false" } else { shell };
                    reply(0, &format!("UserShell: {shell}\n"), "")
                }
                _ => reply(0, "", ""),
            });
        }
        let there = exists || seen.get();
        let (uid, home) = if seen.get() {
            (300, "/var/empty")
        } else {
            (uid, home)
        };
        Ok(match (program.ends_with("getent"), there) {
            (true, false) => reply(2, "", ""),
            (true, true) => reply(0, &format!("_kbf-fetch:*:{uid}:{uid}::{home}:/x\n"), ""),
            (false, _) => reply(0, &format!("{uid}\n"), ""),
        })
    });
    (exec, Accounts { made })
}

fn creating(exec: &FakeExec) -> Vec<String> {
    exec.log
        .iter()
        .filter(|s| s.to_string().contains("-create"))
        .map(ToString::to_string)
        .collect()
}

/// Catches the chicken-and-egg: an applying migration of a kit Mac whose `_kbf-fetch` does
/// not exist stopped with "no user _kbf-fetch to run the version probe as" (before this change
/// only `setup` made the account, and `setup` refuses until the migration is done). Now it makes
/// the account with the steps `setup` makes (the same `dscl` records, at the first free id) and
/// the probe user is the new account (uid 300, as `setup` would give it); the lines say so.
///
/// Mutants seen red: the creation not called (the old failure); the creation called for a dry
/// run (see the dry-run test); the lines of the steps dropped.
#[test]
fn an_applying_migration_makes_the_fetch_account_of_a_kit_mac() {
    let rig = Rig::kit("fetch-make", Platform::MacOs, Form::New);
    let (mut exec, accounts) = mac_accounts(false, 0, "", "");
    let args = migrate_args(&[]);
    let (user, made) =
        probe_account(&mut exec, &rig.layout, &args, true, false, me()).expect("account");
    assert!(accounts.made.get());
    assert_eq!(user.uid, 300);
    assert_eq!(
        creating(&exec),
        [
            "run /usr/bin/dscl . -create /Groups/_kbf-fetch PrimaryGroupID 300".to_owned(),
            format!("run /usr/bin/dscl . -create {RECORD} UniqueID 300"),
            format!("run /usr/bin/dscl . -create {RECORD} PrimaryGroupID 300"),
            format!("run /usr/bin/dscl . -create {RECORD} UserShell /usr/bin/false"),
            format!("run /usr/bin/dscl . -create {RECORD} NFSHomeDirectory /var/empty"),
            format!("run /usr/bin/dscl . -create {RECORD} IsHidden 1"),
        ]
    );
    assert_eq!(made.len(), 6, "{made:?}");
    assert!(made[0].starts_with("+ run /usr/bin/dscl"), "{made:?}");
}

/// Catches the creation being the same code only by accident: what `migrate` runs is what
/// `setup` plans for the same host (the same steps in the same order), compared on a host
/// with the same account records.
///
/// Mutant seen red: a step removed from `create_fetch_user` (both sides move; the pinned list
/// in the test above fails).
#[test]
fn the_account_migrate_makes_is_the_plan_setup_uses() {
    let rig = Rig::kit("fetch-same", Platform::MacOs, Form::New);
    let (mut exec, _) = mac_accounts(false, 0, "", "");
    let args = migrate_args(&[]);
    probe_account(&mut exec, &rig.layout, &args, true, false, me()).expect("account");
    let (view, _) = mac_accounts(false, 0, "", "");
    let os = setup::os::SystemOs::new(&view, Platform::MacOs);
    let plan = setup::plan::fetch_user_plan(&os, &view).expect("plan");
    let planned: Vec<String> = plan.steps().iter().map(ToString::to_string).collect();
    let ran: Vec<String> = exec.log.iter().map(ToString::to_string).collect();
    assert_eq!(planned, ran);
}

/// Catches a second run (or an account made by `setup`, or by the kit's own pkg) being made
/// again: an account of the right shape is used as it is, no step is run.
///
/// Mutant seen red: the existing account not looked up (the steps run every time).
#[test]
fn an_existing_fetch_account_of_the_right_shape_is_used_as_it_is() {
    let rig = Rig::kit("fetch-there", Platform::MacOs, Form::New);
    let (mut exec, _) = mac_accounts(true, 333, "/var/empty", "/usr/bin/false");
    let args = migrate_args(&[]);
    let (user, made) =
        probe_account(&mut exec, &rig.layout, &args, true, false, me()).expect("account");
    assert_eq!(user.uid, 333);
    assert!(
        made.is_empty() && exec.log.is_empty(),
        "{made:?} {:?}",
        exec.log
    );
}

/// Catches a wrong account being used for the probe or changed: an existing `_kbf-fetch` that
/// is an ordinary user's shape (uid 501, a login shell, a real home) is refused (exit 5), the
/// reason names each wrong field, and no step is run (the account may be someone's).
///
/// Mutants seen red: the shell not checked; the home not checked; the uid not checked.
#[test]
fn a_fetch_account_of_the_wrong_shape_is_refused_and_not_changed() {
    let rig = Rig::kit("fetch-wrong", Platform::MacOs, Form::New);
    let (mut exec, _) = mac_accounts(true, 501, "/var/fetch-home", "/usr/local/bin/login-shell");
    let args = migrate_args(&[]);
    let failure = probe_account(&mut exec, &rig.layout, &args, true, false, me())
        .expect_err("a wrong account");
    assert_eq!(failure.exit, Exit::NeedsOperator);
    for field in [
        "uid 501",
        "gid 501",
        "home \"/var/fetch-home\"",
        "shell \"/usr/local/bin/login-shell\"",
    ] {
        assert!(failure.why.contains(field), "{field}: {}", failure.why);
    }
    assert!(exec.log.is_empty());
    for (uid, home, shell, field) in [
        (333, "/var/fetch-home", "/usr/bin/false", "home"),
        (333, "/var/empty", "/usr/local/bin/login-shell", "shell"),
        (0, "/var/empty", "/usr/bin/false", "uid 0"),
    ] {
        let (mut exec, _) = mac_accounts(true, uid, home, shell);
        let failure = probe_account(&mut exec, &rig.layout, &args, true, false, me())
            .expect_err("a wrong account");
        assert!(failure.why.contains(field), "{field}: {}", failure.why);
    }
}

/// Catches a dry run that changes the host, or one that fails with the bare "no user" error
/// and no way forward: a dry run makes nothing and says `--apply` makes the account; with the
/// account there it goes on.
///
/// Mutant seen red: the creation made in a dry run (the log is not empty).
#[test]
fn a_dry_run_makes_nothing_and_says_how_the_account_comes_to_be() {
    let rig = Rig::kit("fetch-dry", Platform::MacOs, Form::New);
    let (mut exec, accounts) = mac_accounts(false, 0, "", "");
    let args = migrate_args(&[]);
    let failure =
        probe_account(&mut exec, &rig.layout, &args, false, false, me()).expect_err("no account");
    assert_eq!(failure.exit, Exit::Usage);
    assert!(failure.why.contains("migrate --apply"), "{}", failure.why);
    assert!(exec.log.is_empty() && !accounts.made.get());
    let (mut exec, _) = mac_accounts(true, 333, "/var/empty", "/usr/bin/false");
    probe_account(&mut exec, &rig.layout, &args, false, false, me()).expect("there");
}

/// Catches the "a dry run cannot make it" hint on a failure it does not explain: the hint says
/// `migrate --apply` creates the standard account on a Mac, so a dry run on Linux (the package's
/// sysusers entry makes it) and a dry run naming another account with `--fetch-user` (never
/// made by migrate) keep the plain "no user" error.
///
/// Mutants seen red: the hint given on any platform; the hint given for any account name.
#[test]
fn the_dry_run_hint_is_for_the_standard_account_on_a_mac_only() {
    let missing = |exec: &mut FakeExec| {
        exec.answer = Box::new(|_| Ok(reply(2, "", "")));
    };
    let rig = Rig::linux("fetch-dry-linux");
    let mut exec = FakeExec::new(0);
    missing(&mut exec);
    let failure = probe_account(
        &mut exec,
        &rig.layout,
        &migrate_args(&[]),
        false,
        false,
        me(),
    )
    .expect_err("no package");
    assert!(failure.why.contains("kbf-fetch"), "{}", failure.why);
    assert!(!failure.why.contains("migrate --apply"), "{}", failure.why);

    let rig = Rig::kit("fetch-dry-named", Platform::MacOs, Form::New);
    let mut exec = FakeExec::new(0);
    missing(&mut exec);
    let args = migrate_args(&["--fetch-user", "_other"]);
    let failure =
        probe_account(&mut exec, &rig.layout, &args, false, false, me()).expect_err("no account");
    assert!(failure.why.contains("_other"), "{}", failure.why);
    assert!(!failure.why.contains("migrate --apply"), "{}", failure.why);
}

/// Catches an account made where it is not the migration's to make: a rollback, a rehearsal
/// under `--root`, an operator's own `--fetch-user` (even the name `_kbf`), a host with no old
/// layout and a host whose migration finished make nothing, and the old error stands.
///
/// Mutants seen red: each condition of the gate removed in turn.
#[test]
fn the_account_is_made_only_by_an_applying_migration_of_an_old_layout() {
    let rig = Rig::kit("fetch-gate", Platform::MacOs, Form::New);
    let none = |exec: &FakeExec| assert!(exec.log.is_empty(), "{:?}", exec.log);
    for (extra, rehearsal) in [
        (vec!["--rollback"], false),
        (vec![], true),
        (vec!["--fetch-user", "_kbf"], false),
    ] {
        let (mut exec, _) = mac_accounts(false, 0, "", "");
        let args = migrate_args(&extra);
        let failure = probe_account(&mut exec, &rig.layout, &args, true, rehearsal, me())
            .expect_err("no account");
        assert!(
            failure.why.contains("no user"),
            "{extra:?}: {}",
            failure.why
        );
        none(&exec);
    }
    // No old layout: an empty scratch root.
    let bare = crate::layout::Layout::new(
        kbf_host::Root::new(kbf_host::testkit::scratch("fetch-gate-empty")).expect("root"),
        Platform::MacOs,
    );
    let (mut exec, _) = mac_accounts(false, 0, "", "");
    let failure = probe_account(&mut exec, &bare, &migrate_args(&[]), true, false, me())
        .expect_err("nothing to migrate");
    assert!(failure.why.contains("no user"), "{}", failure.why);
    none(&exec);
}

/// Catches the creation on a host where it was never the gap: on Linux the package's sysusers
/// entry makes `kbf-fetch`, and `migrate` does not need `setup` first, so a Linux migration
/// makes no account (and says "no user" as before when the package is missing).
///
/// Mutant seen red: the platform check removed from `make_fetch_user` (useradd runs).
#[test]
fn linux_makes_no_account_because_the_package_does() {
    let rig = Rig::linux("fetch-linux");
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| {
        Ok(if cmd.program.as_str().ends_with("getent") {
            reply(2, "", "")
        } else {
            reply(0, "", "")
        })
    });
    let failure = probe_account(
        &mut exec,
        &rig.layout,
        &migrate_args(&[]),
        true,
        false,
        me(),
    )
    .expect_err("no package");
    assert!(failure.why.contains("kbf-fetch"), "{}", failure.why);
    assert!(exec.log.is_empty());
    assert!(
        make_fetch_user(&mut exec, Platform::Linux)
            .expect("none")
            .is_empty()
    );
}

/// Catches a step that fails being passed over: when a `dscl` step fails the migration stops
/// with the step named (exit 1) and goes no further.
///
/// Mutant seen red: the apply error dropped (`let _ =`).
#[test]
fn a_failing_step_stops_the_migration_naming_the_step() {
    let rig = Rig::kit("fetch-fail", Platform::MacOs, Form::New);
    let (mut exec, _) = mac_accounts(false, 0, "", "");
    exec.fail_at = Some(2);
    let failure = probe_account(
        &mut exec,
        &rig.layout,
        &migrate_args(&[]),
        true,
        false,
        me(),
    )
    .expect_err("a step fails");
    assert_eq!(failure.exit, Exit::Failed);
    assert!(
        failure.why.contains("dscl") && failure.why.contains("planted"),
        "{}",
        failure.why
    );
    assert_eq!(exec.log.len(), 3);
}
