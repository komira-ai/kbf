//! `uninstall`: what a package leaves after a rollback, and the host it may be removed from.
//! Each test says the defect it catches.

use std::fs;
use std::path::PathBuf;

use super::journal::{Journal, Phase};
use super::kit_host::Form;
use super::rig::*;
use super::*;
use crate::exit::Exit;
use crate::layout::Platform;
use crate::state::{Locked, State};

const JOB: &str = "com.komira.kbf-installer.plist";
const PLIST_REL: &str = "Library/LaunchDaemons/com.komira.kbf-installer.plist";
const LOGS_REL: &str = "Library/Logs/kbf-installer";
const TOML_REL: &str = "usr/local/etc/kbf/installer.toml";
const RECEIPT: &str = "com.komira.kbf-installer";
const NOON: u64 = 1_792_497_600; // 2026-10-20T12:00:00Z
const LATER: u64 = NOON + 3600;
const NOON_SUFFIX: &str = ".rolled-back-20261020T120000Z";
const STAMP: &str = "20261020T120000Z";

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

fn uninstall_of(rig: &Rig) -> Uninstall {
    Uninstall {
        layout: rig.layout.clone(),
        roots: rig.roots(),
        owner_uid: me(),
        installer_unit: None,
        lock_wait: rig.lock_wait,
    }
}

fn apply_at(rig: &mut Rig, now: u64) -> Result<Report, MigrateError> {
    let u = uninstall_of(rig);
    uninstall_at(&u, &mut rig.host, Mode::Apply, now)
}

fn dry(rig: &mut Rig) -> Result<Report, MigrateError> {
    let u = uninstall_of(rig);
    uninstall_at(&u, &mut rig.host, Mode::DryRun, NOON)
}

/// What the package places and `setup` and the job leave: the plist, the log directory with a
/// log in it and the receipt.
fn package_leftovers(rig: &Rig) {
    put(&rig.at(PLIST_REL), "<plist/>\n", 0o644);
    put(
        &rig.at(LOGS_REL).join("kbf-installer.log"),
        "a log\n",
        0o644,
    );
    rig.receipts.borrow_mut().insert(RECEIPT.to_owned());
}

/// A kit Mac the installer adopted and the operator rolled back at [`NOON`], with the package's
/// leftovers: the installer's tree and `installer.toml` moved aside under [`NOON_SUFFIX`], its
/// job booted out and disabled, the journal in the state directory.
fn rolled_back_mac(name: &str) -> Rig {
    let mut rig = Rig::kit(name, Platform::MacOs, Form::New);
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    put(
        &rig.at(TOML_REL),
        "[installer]\nmin_generation = 17\n",
        0o644,
    );
    rig.active.borrow_mut().insert(JOB.to_owned());
    let m = rig.migration();
    rollback_at(&m, &mut rig.host, Mode::Apply, NOON).expect("rollback");
    package_leftovers(&rig);
    rig.events.borrow_mut().clear();
    rig
}

/// A Mac the kit manages that the installer never touched, with the package installed: its
/// tree (the package's payload), plist, logs and receipt, and no journal.
fn never_adopted_mac(name: &str) -> Rig {
    let rig = Rig::kit(name, Platform::MacOs, Form::New);
    put(
        &rig.at("usr/local/kbf-installer/bin/kbf-installer"),
        "the binary",
        0o755,
    );
    package_leftovers(&rig);
    rig
}

fn names(dir: PathBuf) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches a dry run that is not the exact list: every path that would go and the receipt,
/// one per line and in the order they are removed (the job's plist, the override, the receipt,
/// the logs, what the rollback moved aside, the journal's archive), each marked `DRY-RUN:`,
/// with nothing changed (no file, no command that changes a service or a receipt) and no
/// lock file made.
///
/// Mutant seen red: the dry-run `continue` removed (the dry run then runs every step). Not run as
/// mutants: a line left out of the listing; the receipt id missing from its line.
#[test]
fn a_dry_run_lists_every_path_and_the_receipt_and_changes_nothing() {
    let mut rig = rolled_back_mac("dry");
    let before = rig.snapshot("");
    let report = dry(&mut rig).expect("plan");
    assert_eq!(report.outcome, Outcome::Planned);
    let journal = "/var/db/kbf-installer/migration.json";
    assert_eq!(
        report.lines,
        [
            format!("DRY-RUN: remove /{PLIST_REL}"),
            format!("DRY-RUN: clear the disable override of {JOB} (launchctl enable)"),
            format!("DRY-RUN: forget the package receipt {RECEIPT} (pkgutil --forget)"),
            format!("DRY-RUN: remove tree /{LOGS_REL}"),
            format!("DRY-RUN: remove /{TOML_REL}{NOON_SUFFIX}"),
            format!("DRY-RUN: remove tree /usr/local/kbf-installer{NOON_SUFFIX}"),
            format!(
                "DRY-RUN: archive the journal {journal} as {journal}{NOON_SUFFIX} (kept, never deleted)"
            ),
        ]
    );
    assert_eq!(rig.snapshot(""), before, "a dry run changed a file");
    assert!(rig.events.borrow().is_empty(), "{:?}", rig.events.borrow());
    assert!(rig.receipts.borrow().contains(RECEIPT));
}

/// Catches an apply that leaves part of the package behind or loses the record: the plist,
/// the log directory with its files, the aside tree and `installer.toml` are gone, the
/// override is cleared (`launchctl enable`) and the receipt forgotten, in that order, while
/// the kit's own files and the state directory stay, and the journal is not deleted but kept
/// under the rollback's own suffix with its bytes read back whole. The state directory holds
/// no `uninstall.json` after.
///
/// Mutants seen red: the archive written with no bytes; the override left out; `uninstall.json` left
/// after the archive. Not run as mutants: the receipt left out; the aside tree left out.
#[test]
fn an_apply_removes_the_package_and_archives_the_journal() {
    let mut rig = rolled_back_mac("apply");
    let state = rig.new_state_dir();
    let journal_before = fs::read(state.join("migration.json")).expect("journal");
    let kit_before = rig.snapshot("usr/local/kbf");
    let report = apply_at(&mut rig, LATER).expect("uninstall");
    assert_eq!(report.outcome, Outcome::Uninstalled);

    for gone in [PLIST_REL, LOGS_REL, TOML_REL, "usr/local/kbf-installer"] {
        assert!(!rig.at(gone).exists(), "{gone} is still there");
    }
    assert!(
        names(rig.at("usr/local"))
            .iter()
            .all(|n| !n.contains("rolled-back")),
        "{:?}",
        names(rig.at("usr/local"))
    );
    assert!(
        names(rig.at("usr/local/etc/kbf"))
            .iter()
            .all(|n| !n.starts_with("installer.toml")),
        "{:?}",
        names(rig.at("usr/local/etc/kbf"))
    );
    assert!(rig.receipts.borrow().is_empty());
    assert_eq!(
        *rig.events.borrow(),
        [
            format!("enable {JOB} links=false"),
            format!("forget {RECEIPT} links=false")
        ]
    );
    assert!(rig.enabled.borrow().contains(JOB));
    assert_eq!(rig.snapshot("usr/local/kbf"), kit_before, "the kit's tree");
    assert!(
        rig.at("Library/LaunchDaemons/com.komira.kbf-daemon.plist")
            .exists()
    );

    // The journal: kept under the suffix the rollback chose (not the time of this run), whole.
    let kept = names(state);
    assert!(
        kept.contains(&format!("migration.json{NOON_SUFFIX}"))
            && !kept
                .iter()
                .any(|n| n == "migration.json" || n == "uninstall.json"),
        "the state directory: {kept:?}"
    );
    assert_eq!(
        fs::read(
            rig.new_state_dir()
                .join(format!("migration.json{NOON_SUFFIX}"))
        )
        .expect("kept"),
        journal_before
    );
    let shown = report.to_string();
    assert!(shown.contains("uninstalled"), "{shown}");
    assert!(shown.contains("with the journal archived in it"), "{shown}");
}

/// Catches an uninstall of a host the installer manages: a journal that is `Done` (adopted) or
/// `Started` (begun), and no journal but a `state.json` the installer wrote, are each refused
/// with exit 5 and a reason that names the way out, and nothing is changed or even locked.
///
/// Mutant seen red: the phase test removed (a `Done` journal goes ahead). Not run as a mutant: the
/// `state.json` test removed.
#[test]
fn a_host_the_installer_manages_is_refused_and_untouched() {
    for phase in [Phase::Done, Phase::Started] {
        let mut rig = rolled_back_mac(&format!("refuse-{phase:?}"));
        let path = rig.new_state_dir().join("migration.json");
        let mut journal = Journal::parse(&fs::read(&path).expect("journal")).expect("json");
        journal.phase = phase;
        put(
            &path,
            &String::from_utf8(journal.render()).expect("text"),
            0o600,
        );
        let before = rig.snapshot("");
        let error = apply_at(&mut rig, NOON).expect_err("refused");
        assert!(matches!(error, MigrateError::Adopted(_)), "{error}");
        assert_eq!(error.exit(), Exit::NeedsOperator, "{phase:?}");
        assert!(
            error.to_string().contains("migrate --rollback"),
            "{phase:?}: {error}"
        );
        assert_eq!(rig.snapshot(""), before, "{phase:?}");
        assert!(rig.events.borrow().is_empty(), "{phase:?}");
    }

    // No journal, but the installer's own state: `setup` ran here.
    let mut rig = never_adopted_mac("refuse-state");
    let layout = rig.layout.clone();
    fs::create_dir_all(rig.new_state_dir()).expect("state dir");
    Locked::acquire(&layout)
        .expect("lock")
        .write(&State::default())
        .expect("state");
    let before = rig.snapshot("");
    let error = apply_at(&mut rig, NOON).expect_err("refused");
    assert!(matches!(error, MigrateError::Adopted(_)), "{error}");
    assert!(error.to_string().contains("state.json"), "{error}");
    assert_eq!(rig.snapshot(""), before);

    // A state nobody can read is not proof that the installer is gone: the error is the
    // state's own, and nothing is changed.
    let mut rig = never_adopted_mac("refuse-unreadable-state");
    fs::create_dir_all(rig.new_state_dir()).expect("state dir");
    put(&rig.new_state_dir().join("state.json"), "not json", 0o600);
    let before = rig.snapshot("");
    let error = apply_at(&mut rig, NOON).expect_err("refused");
    assert!(matches!(error, MigrateError::State(_)), "{error}");
    assert_eq!(rig.snapshot(""), before);
}

/// Catches a never-adopted Mac that cannot be cleaned, and one that is cleaned of too little:
/// no journal and no state is a host the installer never managed, so its loaded job is booted
/// out, then (the plist still there when the job is booted out) the plist, the override, the
/// receipt, the logs, `installer.toml` and the package's own tree go, in that order, and there
/// is no journal to archive.
///
/// Mutants seen red: the dry-run `continue` removed; the override left out. Not run as mutants: the
/// stop done after the plist is removed (the order assertion); the live tree left out.
#[test]
fn a_never_adopted_mac_loses_its_job_first_then_the_rest() {
    let mut rig = never_adopted_mac("never");
    put(&rig.at(TOML_REL), "[installer]\n", 0o644);
    rig.active.borrow_mut().insert(JOB.to_owned());
    let root = rig.host.root.clone();
    let plist_at_stop = std::rc::Rc::new(std::cell::Cell::new(None));
    rig.on_command({
        let plist_at_stop = plist_at_stop.clone();
        move |verb, unit| {
            if verb == "stop" && unit == JOB {
                plist_at_stop.set(Some(root.join(PLIST_REL).exists()));
            }
        }
    });
    let report = apply_at(&mut rig, NOON).expect("uninstall");
    assert_eq!(report.outcome, Outcome::Uninstalled);
    assert_eq!(
        plist_at_stop.get(),
        Some(true),
        "the job was booted out after its plist was removed"
    );
    assert_eq!(
        *rig.events.borrow(),
        [
            format!("stop {JOB} links=false"),
            format!("enable {JOB} links=false"),
            format!("forget {RECEIPT} links=false"),
        ]
    );
    assert!(!rig.active.borrow().contains(JOB));
    for gone in [PLIST_REL, LOGS_REL, TOML_REL, "usr/local/kbf-installer"] {
        assert!(!rig.at(gone).exists(), "{gone} is still there");
    }
    let shown = report.to_string();
    assert!(shown.contains(&format!("boot out {JOB}")), "{shown}");
    assert!(!shown.contains("archive"), "{shown}");
    assert!(!shown.contains("journal"), "{shown}");
}

/// Catches an uninstall that goes ahead while a `run` pass holds the installer's lock (it
/// could pull the plist from under the pass): with the lock held the apply ends busy, exit 4,
/// changing nothing, while a dry run (which only reads) still answers; with the lock free it
/// goes ahead.
///
/// Mutant seen red: the lock not taken (the state read unlocked). Not run as a mutant: the lock taken
/// after the steps.
#[test]
fn a_held_lock_ends_busy_and_changes_nothing() {
    let mut rig = rolled_back_mac("busy");
    rig.lock_wait = std::time::Duration::from_millis(50);
    let before = rig.snapshot("var/db");
    let held = Locked::acquire(&rig.layout).expect("the pass's lock");
    let error = apply_at(&mut rig, NOON).expect_err("busy");
    assert_eq!(error.exit(), Exit::Busy, "{error}");
    assert!(rig.at(PLIST_REL).exists() && rig.at(LOGS_REL).exists());
    assert!(rig.events.borrow().is_empty());
    assert_eq!(rig.snapshot("var/db"), before);
    dry(&mut rig).expect("a dry run takes no lock");
    drop(held);
    apply_at(&mut rig, NOON).expect("free now");
    assert!(!rig.at(PLIST_REL).exists());
}

/// Catches an apply by a user who is not root changing a host (a rehearsal's owner is the
/// caller, a host's is root): exit 2 and nothing changed.
///
/// Mutant seen red: the euid check removed.
#[test]
fn an_apply_needs_the_owner_and_a_dry_run_does_not() {
    let mut rig = rolled_back_mac("not-root");
    rig.host.euid = me() + 1;
    let before = rig.snapshot("");
    let error = apply_at(&mut rig, NOON).expect_err("not root");
    assert!(
        matches!(error, MigrateError::NotRootToUninstall { .. }),
        "{error}"
    );
    assert_eq!(error.exit(), Exit::Usage);
    assert!(error.to_string().contains("run it as root"), "{error}");
    assert_eq!(rig.snapshot(""), before);
    dry(&mut rig).expect("a dry run is for anyone");
}

/// Catches an interrupted uninstall that cannot resume or that repeats itself: the forget of
/// the receipt fails after the override was cleared, `uninstall.json` names the suffix and
/// the steps done and the journal is still there; a run again (an hour later) does the
/// receipt and what is left and not the override again (one `enable` in all), archives under
/// the same suffix and takes `uninstall.json` away.
///
/// Mutants seen red: the skip of steps already done removed (the override repeats); the
/// `uninstall.json` left after the archive. Not run as mutants: the progress written once at the
/// start; the progress not read back.
#[test]
fn an_interrupted_uninstall_resumes_where_it_stopped() {
    let mut rig = rolled_back_mac("resume");
    *rig.fail_verb.borrow_mut() = Some("forget".to_owned());
    let error = apply_at(&mut rig, NOON).expect_err("the forget fails");
    assert!(matches!(error, MigrateError::Io { .. }), "{error}");
    let state = rig.new_state_dir();
    let progress = rig.json(&state.join("uninstall.json"));
    assert_eq!(
        progress["done"],
        serde_json::json!([format!("remove /{PLIST_REL}"), format!("enable {JOB}")])
    );
    assert!(state.join("migration.json").is_file(), "the journal stays");
    assert!(rig.at(LOGS_REL).exists(), "nothing past the failure ran");

    *rig.fail_verb.borrow_mut() = None;
    let report = apply_at(&mut rig, LATER).expect("resumed");
    assert_eq!(report.outcome, Outcome::Uninstalled);
    let shown = report.to_string();
    assert!(shown.contains("resuming an uninstall"), "{shown}");
    assert_eq!(
        rig.events
            .borrow()
            .iter()
            .filter(|e| e.starts_with("enable"))
            .count(),
        1,
        "{:?}",
        rig.events.borrow()
    );
    assert!(rig.receipts.borrow().is_empty());
    let kept = names(state);
    assert!(
        kept.contains(&format!("migration.json{NOON_SUFFIX}"))
            && !kept
                .iter()
                .any(|n| n == "migration.json" || n == "uninstall.json"),
        "{kept:?}"
    );

    // A dry run in the middle of one lists what is left, not what is done.
    let mut rig = rolled_back_mac("resume-dry");
    *rig.fail_verb.borrow_mut() = Some("forget".to_owned());
    apply_at(&mut rig, NOON).expect_err("the forget fails");
    let lines = dry(&mut rig).expect("plan").lines;
    assert!(
        lines[0].contains("resuming") && lines.iter().all(|l| !l.contains(PLIST_REL)),
        "{lines:?}"
    );
}

/// Catches a second run that is not a no-op: after an uninstall the same command says there
/// is nothing to uninstall, exits as success, runs no command and changes no file; so does a
/// Mac that never had the package (no state directory at all).
///
/// Not run as mutants: the override emitted whenever the host is a Mac; "nothing" reported as
/// `Uninstalled`.
#[test]
fn a_clean_host_has_nothing_to_uninstall() {
    let mut rig = rolled_back_mac("twice");
    apply_at(&mut rig, NOON).expect("first");
    rig.events.borrow_mut().clear();
    let before = rig.snapshot("");
    let again = apply_at(&mut rig, LATER).expect("second");
    assert_eq!(again.outcome, Outcome::NothingToMigrate);
    assert!(
        again.to_string().contains("nothing to uninstall"),
        "{again}"
    );
    assert!(rig.events.borrow().is_empty());
    assert_eq!(rig.snapshot(""), before);

    let mut bare = Rig::bare("bare-mac", Platform::MacOs);
    let report = apply_at(&mut bare, NOON).expect("bare");
    assert_eq!(report.outcome, Outcome::NothingToMigrate);
    assert!(bare.events.borrow().is_empty());
}

/// Catches a glob that is too wide or too narrow: only `<name>.rolled-back-<UTC stamp>` goes
/// (every one of them, from earlier rollbacks too); a name that only starts like one, a longer
/// stamp, another name and the live file stay (the live `installer.toml` of a never-adopted
/// host goes with the host, but a sibling named `installer.toml.keep` does not).
///
/// Mutant seen red: the stamp test replaced by "not empty". Not run as a mutant: only the first match
/// removed.
#[test]
fn only_the_names_a_rollback_makes_are_removed() {
    let mut rig = never_adopted_mac("globs");
    let earlier = ".rolled-back-20261001T000000Z";
    for dir in ["usr/local", "usr/local/etc/kbf"] {
        let name = if dir == "usr/local" {
            "kbf-installer"
        } else {
            "installer.toml"
        };
        for suffix in [NOON_SUFFIX, earlier] {
            put(&rig.at(dir).join(format!("{name}{suffix}")), "gone", 0o644);
        }
        for keep in [
            format!("{name}.rolled-back-"),
            format!("{name}.rolled-back-{STAMP}x"),
            format!("{name}.rolled-back-2026102OT120000Z"),
            format!("{name}.keep"),
            format!("other.rolled-back-{STAMP}"),
        ] {
            put(&rig.at(dir).join(keep), "kept", 0o644);
        }
    }
    apply_at(&mut rig, NOON).expect("uninstall");
    assert_eq!(
        names(rig.at("usr/local/etc/kbf"))
            .into_iter()
            .filter(|n| n.contains("installer.toml"))
            .collect::<Vec<_>>(),
        [
            "installer.toml.keep".to_owned(),
            "installer.toml.rolled-back-".to_owned(),
            format!("installer.toml.rolled-back-{STAMP}x"),
            "installer.toml.rolled-back-2026102OT120000Z".to_owned(),
        ]
    );
    assert!(
        names(rig.at("usr/local")).contains(&format!("other.rolled-back-{STAMP}")),
        "{:?}",
        names(rig.at("usr/local"))
    );
    assert!(!names(rig.at("usr/local")).contains(&format!("kbf-installer{earlier}")));
}

/// Catches an unreadable directory being taken for an empty one, and a damaged record being
/// taken for none: a configuration directory that is a file stops the plan with the io error
/// naming it, an `uninstall.json` that is not JSON or not a file is `Corrupt` (exit 1), and
/// nothing is changed.
///
/// Not run as mutants: the read-dir error swallowed; the record's parse error swallowed.
#[test]
fn an_unreadable_directory_or_record_stops_the_plan() {
    let mut rig = never_adopted_mac("unreadable");
    fs::remove_file(rig.at(TOML_REL)).ok();
    fs::remove_dir_all(rig.at("usr/local/etc/kbf")).expect("rm");
    put(
        &rig.at("usr/local/etc/kbf"),
        "a file where a directory goes",
        0o644,
    );
    let error = dry(&mut rig).expect_err("unreadable");
    assert!(matches!(error, MigrateError::Io { .. }), "{error}");
    assert!(error.to_string().contains("/usr/local/etc/kbf"), "{error}");

    let mut rig = rolled_back_mac("corrupt-record");
    put(&rig.new_state_dir().join("uninstall.json"), "{", 0o600);
    let error = dry(&mut rig).expect_err("corrupt");
    assert!(
        matches!(
            error,
            MigrateError::Corrupt {
                file: "uninstall.json",
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(error.exit(), Exit::Failed);

    let mut rig = rolled_back_mac("record-dir");
    fs::create_dir(rig.new_state_dir().join("uninstall.json")).expect("dir");
    let error = dry(&mut rig).expect_err("not a file");
    assert!(error.to_string().contains("not a regular file"), "{error}");
}

/// Catches a Linux host handled as a Mac: the package's files belong to the package manager,
/// so only the unit the flag names is stopped and disabled (when it runs and when it is
/// enabled), the installer's tree and `state.json` are not touched, the journal is archived
/// under a suffix of the time (a Linux rollback chose none), the report says to remove the
/// package with the package manager, and a Linux host without the flag runs no service command
/// at all.
///
/// Mutant seen red: `uninstall.json` left after the archive. Not run as mutants: the Mac's removals
/// run on Linux; the unit acted on without the flag; the `is-enabled` question dropped.
#[test]
fn a_linux_host_stops_its_unit_archives_the_journal_and_leaves_the_package() {
    let mut rig = Rig::linux("linux");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    let m = rig.migration();
    rollback_at(&m, &mut rig.host, Mode::Apply, NOON).expect("rollback");
    let tree = rig.snapshot("var/lib/kbf-installer");
    rig.events.borrow_mut().clear();

    // Without the flag: no unit is asked about; the journal is archived.
    let mut quiet = Rig::linux("linux-flagless");
    run_migrate(&mut quiet, &never(), Mode::Apply).expect("migrate");
    let m = quiet.migration();
    rollback_at(&m, &mut quiet.host, Mode::Apply, NOON).expect("rollback");
    quiet.events.borrow_mut().clear();
    quiet.host.queries.borrow_mut().clear();
    let report = apply_at(&mut quiet, LATER).expect("uninstall");
    assert_eq!(report.outcome, Outcome::Uninstalled);
    assert!(quiet.events.borrow().is_empty());
    assert!(quiet.host.queries.borrow().is_empty());

    // With the flag, the unit running and enabled.
    rig.active
        .borrow_mut()
        .insert("kbf-installer.service".to_owned());
    rig.enabled
        .borrow_mut()
        .insert("kbf-installer.service".to_owned());
    let mut u = uninstall_of(&rig);
    u.installer_unit = Some(unit("kbf-installer.service"));
    let report = uninstall_at(&u, &mut rig.host, Mode::Apply, LATER).expect("uninstall");
    assert_eq!(
        *rig.events.borrow(),
        [
            "stop kbf-installer.service links=true",
            "disable kbf-installer.service links=true"
        ]
    );
    let stamped = ".rolled-back-20261020T130000Z";
    let dir = rig.new_state_dir();
    assert!(!dir.join("migration.json").exists());
    assert!(dir.join(format!("migration.json{stamped}")).is_file());
    let mut after = rig.snapshot("var/lib/kbf-installer");
    after.retain(|name, _| !name.starts_with("migration.json"));
    let mut before = tree;
    before.retain(|name, _| !name.starts_with("migration.json"));
    assert_eq!(after, before, "the installer's tree");
    let shown = report.to_string();
    assert!(shown.contains("package manager"), "{shown}");

    // A unit that is neither running nor enabled is left alone.
    let mut rig = Rig::linux("linux-idle");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    let m = rig.migration();
    rollback_at(&m, &mut rig.host, Mode::Apply, NOON).expect("rollback");
    rig.events.borrow_mut().clear();
    let mut u = uninstall_of(&rig);
    u.installer_unit = Some(unit("kbf-installer.service"));
    uninstall_at(&u, &mut rig.host, Mode::Apply, NOON).expect("uninstall");
    assert!(rig.events.borrow().is_empty(), "{:?}", rig.events.borrow());
}

/// Catches the flag being ignored: a Mac's job named by `--installer-unit` is the one acted on
/// instead of the package's (and the package's own plist, not the named job's, stays).
///
/// Mutants seen red: the dry-run `continue` removed; the override left out (each moves the events this
/// test pins). Not run as a mutant: the named unit replaced by the default.
#[test]
fn a_named_unit_is_the_one_acted_on() {
    let mut rig = never_adopted_mac("named");
    put(
        &rig.at("Library/LaunchDaemons/com.komira.other.plist"),
        "<plist/>\n",
        0o644,
    );
    rig.active
        .borrow_mut()
        .insert("com.komira.other.plist".to_owned());
    let mut u = uninstall_of(&rig);
    u.installer_unit = Some(unit("com.komira.other.plist"));
    uninstall_at(&u, &mut rig.host, Mode::Apply, NOON).expect("uninstall");
    let events = rig.events.borrow().clone();
    assert_eq!(
        events[0], "stop com.komira.other.plist links=false",
        "{events:?}"
    );
    assert_eq!(
        events[1], "enable com.komira.other.plist links=false",
        "{events:?}"
    );
    assert!(
        !rig.at("Library/LaunchDaemons/com.komira.other.plist")
            .exists()
    );
    assert!(
        rig.at(PLIST_REL).exists(),
        "the package's own plist is not the named one"
    );
}

/// Catches a record left by a run that finished everything but its removal: a Linux host
/// whose journal is archived already and whose `uninstall.json` is still there has nothing to
/// uninstall, says so, and takes the record away (a Mac's run again finishes through its
/// skipped steps; this is the path with no step left at all). A dry run leaves it.
///
/// Mutant seen red: the record left when there is nothing to do. Not run as a mutant: the record
/// removed in a dry run.
#[test]
fn a_record_with_nothing_left_to_do_is_taken_away() {
    let mut rig = Rig::linux("linux-stale-record");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    let m = rig.migration();
    rollback_at(&m, &mut rig.host, Mode::Apply, NOON).expect("rollback");
    apply_at(&mut rig, NOON).expect("uninstall");
    let record = rig.new_state_dir().join("uninstall.json");
    put(
        &record,
        &format!("{{\"schema\":1,\"suffix\":\"{NOON_SUFFIX}\",\"done\":[]}}"),
        0o600,
    );
    assert_eq!(
        dry(&mut rig).expect("plan").outcome,
        Outcome::NothingToMigrate
    );
    assert!(record.exists(), "a dry run took the record away");
    let report = apply_at(&mut rig, LATER).expect("again");
    assert_eq!(report.outcome, Outcome::NothingToMigrate);
    assert!(!record.exists());
}
