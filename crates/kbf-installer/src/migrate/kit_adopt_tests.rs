//! `migrate` on a Mac the Mac kit installed (`testdata/kit_host/`): the links and the
//! LaunchDaemon plist are the truth, there is no `state.json`, and the release binary is
//! `kbf`. Docs: design section 8 ("Migration is owned by P3").

use std::collections::BTreeMap;
use std::fs;

use super::kit_host::{
    CURRENT, Form, KBF_TOML, KBF_TOML_PATH, PLIST_NEW, PLIST_OLD, PLIST_PATH, PREVIOUS, at,
};
use super::rig::*;
use super::*;
use crate::exit::Exit;
use crate::layout::Platform;

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

fn plist_of(form: Form) -> &'static str {
    match form {
        Form::New => PLIST_NEW,
        Form::Old => PLIST_OLD,
    }
}

fn new_bin(rig: &Rig) -> String {
    rig.layout.current().to_string()
}

/// Catches `migrate` answering "nothing to migrate" for a Mac the kit installed, and a
/// migration that is not whole: both releases adopted from the links, state schema 2 with
/// `current` and `previous` as the links named them, the plist repointed at the installer's
/// tree with every other byte kept, the kit's tree untouched and `kbf.toml` with the node's two keys added, the version
/// asked of `releases/<sha>/kbf`, and nothing made in the old tools' places (no
/// `/var/db/kbf-deploy`, no lock file).
///
/// Mutant seen red: `Roots::binary` naming `kbf-daemon` (the old probe). Not run as mutants: `previous`
/// left out; the plist not repointed.
#[test]
fn a_kit_host_is_adopted_from_its_links_and_plist_in_both_forms() {
    for form in [Form::New, Form::Old] {
        let mut rig = Rig::kit(&format!("adopt-{form:?}"), Platform::MacOs, form);
        let tree = rig.snapshot("usr/local/kbf");
        let toml = fs::read(at(&rig.host.root, KBF_TOML_PATH)).expect("kbf.toml");
        let runner = runner();
        let report = run_with(&mut rig, &runner, &never(), &NoFrontend, Mode::Apply)
            .unwrap_or_else(|e| panic!("{form:?}: {e}"));

        assert_eq!(report.outcome, Outcome::Migrated, "{form:?}");
        assert_eq!(report.unverified, vec![commit(CURRENT), commit(PREVIOUS)]);
        let programs: Vec<String> = runner
            .calls()
            .iter()
            .map(|c| c.program.display().to_string())
            .collect();
        for sha in [CURRENT, PREVIOUS] {
            let want = rig.at(&format!("usr/local/kbf/releases/{sha}/kbf"));
            assert!(
                programs.contains(&want.display().to_string()),
                "{programs:?}"
            );
        }
        let state = rig.json(&rig.new_state_dir().join("state.json"));
        assert_eq!(state["schema"], 2);
        assert_eq!(state["current"], CURRENT);
        assert_eq!(state["previous"], PREVIOUS);
        let links = rig.new_tree().join("releases");
        for (name, sha) in [("current", CURRENT), ("previous", PREVIOUS)] {
            let target = fs::read_link(links.join(name)).expect("link");
            assert_eq!(target.to_str(), Some(sha));
            assert!(links.join(sha).join("kbf").is_file());
        }
        let now = fs::read_to_string(at(&rig.host.root, PLIST_PATH)).expect("plist");
        assert_eq!(
            now,
            plist_of(form).replace("/usr/local/kbf/current", &new_bin(&rig)),
            "{form:?}"
        );
        assert_eq!(
            rig.snapshot("usr/local/kbf"),
            tree,
            "the kit's tree is kept"
        );
        // Only the node's two keys are new: the layout's directories, in the `[daemon]` table.
        let wanted = String::from_utf8(toml).expect("text").replacen(
            "[daemon]\n",
            "[daemon]\nupdate-dir = \"/var/db/kbf-installer\"\norders-dir = \"/var/db/kbf-installer/orders\"\n",
            1,
        );
        assert_eq!(
            fs::read_to_string(at(&rig.host.root, KBF_TOML_PATH)).expect("toml"),
            wanted,
            "{form:?}"
        );
        assert!(rig.at("var/db/kbf-installer/orders").is_dir());
        assert!(!rig.at("var/db/kbf-deploy").exists());
        assert!(!rig.at("var/run/kbf-deploy.lock").exists());
        let journal = rig.json(&rig.new_state_dir().join("migration.json"));
        assert_eq!(journal["phase"], "done");
        assert_eq!(journal["kit"], true);
    }
}

/// Catches a dry run that writes, and a re-run that migrates twice: a dry run says what it
/// would do (the kit host, both releases) and leaves the host as it was; after an apply a
/// second apply is a no-op that says so.
///
/// Not run as mutants: the kit line missing from the plan; a dry run that took the journal.
#[test]
fn a_kit_host_dry_run_changes_nothing_and_a_rerun_is_a_no_op() {
    let mut rig = Rig::kit("dry", Platform::MacOs, Form::New);
    let before = (rig.snapshot("usr/local"), rig.snapshot("Library"));
    let report = run_migrate(&mut rig, &never(), Mode::DryRun).expect("plan");
    assert_eq!(report.outcome, Outcome::Planned);
    let shown = report.to_string();
    assert!(shown.contains("a host the Mac kit installed"), "{shown}");
    assert!(
        shown.contains(CURRENT) && shown.contains(PREVIOUS),
        "{shown}"
    );
    assert_eq!(before, (rig.snapshot("usr/local"), rig.snapshot("Library")));

    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    let again = run_migrate(&mut rig, &never(), Mode::Apply).expect("again");
    assert_eq!(again.outcome, Outcome::AlreadyMigrated);
}

/// Catches `setup` letting an unmigrated kit host through, and `standing` forgetting the
/// units once it is migrated: before the migration `guard` refuses naming the kit's tree
/// (there is no state directory to name), after it the plist is the unit setup may retire.
///
/// Not run as mutants: `standing` naming `/var/db/kbf-deploy` for a kit host.
#[test]
fn a_kit_host_is_unmigrated_until_migrate_finishes() {
    let mut rig = Rig::kit("standing", Platform::MacOs, Form::Old);
    let error = guard(&rig.layout, me()).expect_err("unmigrated");
    assert!(
        matches!(&error, MigrateError::Unmigrated(w) if w == "/usr/local/kbf"),
        "{error}"
    );
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    guard(&rig.layout, me()).expect("migrated");
    assert_eq!(
        standing(&rig.layout, me()).expect("units"),
        vec![rig.daemon()]
    );
}

/// Catches a rollback that leaves the kit host changed: after `migrate` and `rollback` the
/// plist has its bytes back (the README's rollback target, both forms), `/usr/local/kbf` and
/// `kbf.toml` are as they were, the job was stopped and bootstrapped again, no old lock or
/// state directory was made, and a second rollback says it is done.
///
/// Mutant seen red: a rollback that takes the old lock (its file appears). Not run as mutants: the
/// plist not restored.
#[test]
fn rolling_back_a_kit_host_restores_the_kits_plist_and_links() {
    for form in [Form::New, Form::Old] {
        let mut rig = Rig::kit(&format!("rollback-{form:?}"), Platform::MacOs, form);
        let before = (rig.snapshot("usr/local/kbf"), rig.snapshot("usr/local/etc"));
        run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
        rig.events.borrow_mut().clear();
        let mut m = rig.migration();
        m.installer_unit = None;
        let report = rollback(&m, &mut rig.host, Mode::Apply).expect("rollback");
        assert_eq!(report.outcome, Outcome::RolledBack);
        assert_eq!(
            fs::read_to_string(at(&rig.host.root, PLIST_PATH)).expect("plist"),
            plist_of(form),
            "{form:?}: byte for byte"
        );
        assert_eq!(
            fs::read_to_string(at(&rig.host.root, KBF_TOML_PATH)).expect("toml"),
            KBF_TOML
        );
        assert_eq!(
            before,
            (rig.snapshot("usr/local/kbf"), rig.snapshot("usr/local/etc"))
        );
        let events = rig.events.borrow().join("|");
        assert!(
            events.contains("stop com.komira.kbf-daemon.plist")
                && events.contains("start com.komira.kbf-daemon.plist"),
            "{events}"
        );
        assert!(rig.active.borrow().contains("com.komira.kbf-daemon.plist"));
        assert!(!rig.at("var/db/kbf-deploy").exists());
        assert!(!rig.at("var/run/kbf-deploy.lock").exists());
        let again = rollback(&m, &mut rig.host, Mode::Apply).expect("again");
        assert!(again.to_string().contains("already rolled back"));
    }
}

/// What a test does to a kit host to make it half of one.
type Damage<'a> = &'a dyn Fn(&Rig);

fn refused(rig: &mut Rig) -> MigrateError {
    let before = rig.snapshot("");
    let error = run_migrate(rig, &never(), Mode::Apply).expect_err("refused");
    assert_eq!(
        rig.snapshot(""),
        before,
        "a refusal changes nothing: {error}"
    );
    error
}

/// Catches a half kit tree adopted or called "nothing to migrate": a tree with no job, a job
/// with a tree that has no `current` or no `releases/`, and no LaunchDaemons directory at
/// all each end the run (exit 3) naming what is missing, with the host untouched.
///
/// Not run as mutants: a missing plist read as an empty one; `current` unchecked.
#[test]
fn a_half_kit_tree_is_refused() {
    let cases: [(&str, Damage, &str); 4] = [
        (
            "no-plist",
            &|r| {
                fs::remove_file(r.at("Library/LaunchDaemons/com.komira.kbf-daemon.plist"))
                    .expect("rm")
            },
            "is not in",
        ),
        (
            "no-dir",
            &|r| fs::remove_dir_all(r.at("Library")).expect("rm"),
            "is not in",
        ),
        (
            "no-current",
            &|r| fs::remove_file(r.at("usr/local/kbf/current")).expect("rm"),
            "no current link",
        ),
        (
            "no-releases",
            &|r| fs::remove_dir_all(r.at("usr/local/kbf/releases")).expect("rm"),
            "no releases/",
        ),
    ];
    for (name, damage, says) in cases {
        let mut rig = Rig::kit(name, Platform::MacOs, Form::New);
        damage(&rig);
        let error = refused(&mut rig);
        assert!(matches!(error, MigrateError::HalfKit(_)), "{name}: {error}");
        assert!(error.to_string().contains(says), "{name}: {error}");
        assert_eq!(error.exit().code(), Exit::Refused.code());
    }
}

/// Catches a `current` link that leaves `releases/` adopted: `current` naming a path outside
/// it, or a directory that is not a commit, is refused as the old layout's bad link is.
///
/// Not run as mutants: the link target not checked (the check is the existing `linked`).
#[test]
fn a_current_link_outside_releases_is_refused() {
    for target in ["/etc", "../elsewhere", "releases/not-a-commit"] {
        let mut rig = Rig::kit("badlink", Platform::MacOs, Form::New);
        let link = rig.at("usr/local/kbf/current");
        fs::remove_file(&link).expect("rm");
        std::os::unix::fs::symlink(target, &link).expect("link");
        let error = refused(&mut rig);
        assert!(
            matches!(&error, MigrateError::BadLink { link: "current", target: t } if t == target),
            "{target}: {error}"
        );
        assert_eq!(error.exit().code(), Exit::Refused.code());
    }
}

/// Catches a plist of no known form being repointed: another program (in either form), the new form with a
/// flag after the file, a plist with no argument array and one whose array never ends are
/// each refused (exit 3) and say what the job runs.
///
/// Mutant seen red: the new form accepted with trailing arguments.
#[test]
fn a_plist_of_no_known_form_is_refused() {
    let broken = [
        PLIST_NEW.replace("/usr/local/kbf/current/kbf", "/usr/local/bin/other"),
        PLIST_NEW.replace("</array>", "<string>--extra</string>\n</array>"),
        PLIST_NEW.replace("<key>ProgramArguments</key>", "<key>Program</key>"),
        PLIST_NEW.replace("<array>", "<dict>"),
        PLIST_NEW.replace("</array>", "</dict>"),
        PLIST_NEW.replace("--mode=daemon", "serve"),
        PLIST_OLD.replace("/usr/local/kbf/current/kbf", "/usr/local/bin/other"),
    ];
    for (n, text) in broken.iter().enumerate() {
        let mut rig = Rig::kit(&format!("plist{n}"), Platform::MacOs, Form::New);
        fs::write(at(&rig.host.root, PLIST_PATH), text).expect("plist");
        let error = refused(&mut rig);
        assert!(
            matches!(error, MigrateError::UnknownPlist(_)),
            "{n}: {error}"
        );
        assert!(
            error
                .to_string()
                .contains(PLIST_PATH.rsplit('/').next().unwrap())
        );
        assert_eq!(error.exit().code(), Exit::Refused.code());
    }
}

/// Catches two managers' state adopted as one: `state.json` beside the kit's tree (the kit's
/// own settings file is in it) is a conflict (exit 5) and nothing is changed; the same
/// state beside a tree without the kit's file is an old host and is not refused for it.
///
/// Mutant seen red: the marker not looked for.
#[test]
fn old_state_beside_a_kit_tree_is_refused() {
    let mut rig = Rig::kit("both", Platform::MacOs, Form::New);
    fs::create_dir_all(rig.at("var/db/kbf-deploy")).expect("dir");
    rig.write_old_state(Some(CURRENT), Some(PREVIOUS), None, None);
    let error = refused(&mut rig);
    assert!(matches!(error, MigrateError::Conflict(_)), "{error}");
    assert!(error.to_string().contains("av2_mac.conf"), "{error}");
    assert_eq!(error.exit().code(), Exit::NeedsOperator.code());

    fs::remove_file(rig.at("usr/local/kbf/etc/av2_mac.conf")).expect("rm");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("an old host");
    assert_eq!(report.outcome, Outcome::Migrated);
    assert!(!report.to_string().contains("Mac kit"), "{report}");
}

/// Catches a kit tree adopted on Linux: the kit's job file on a Linux host is refused (exit
/// 3); a Linux host with a tree and no old state and no such file is still "nothing to
/// migrate"; so is a Mac with no tree and one with an empty tree.
///
/// Mutant seen red: the Linux arm absent.
#[test]
fn a_kit_tree_on_linux_is_refused_and_nothing_else_changes() {
    let mut rig = Rig::kit("linux", Platform::Linux, Form::New);
    let error = refused(&mut rig);
    assert!(matches!(error, MigrateError::KitOnLinux(_)), "{error}");
    assert_eq!(error.exit().code(), Exit::Refused.code());

    fs::remove_file(rig.at("Library/LaunchDaemons/com.komira.kbf-daemon.plist")).expect("rm");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("nothing");
    assert_eq!(report.outcome, Outcome::NothingToMigrate);

    let mut mac = Rig::kit("empty", Platform::MacOs, Form::New);
    fs::remove_dir_all(mac.at("usr/local/kbf")).expect("rm");
    let report = run_migrate(&mut mac, &never(), Mode::Apply).expect("no tree");
    assert_eq!(report.outcome, Outcome::NothingToMigrate);
    fs::create_dir_all(mac.at("usr/local/kbf")).expect("dir");
    let report = run_migrate(&mut mac, &never(), Mode::Apply).expect("empty tree");
    assert_eq!(report.outcome, Outcome::NothingToMigrate);
}

/// Catches a host that stops being a kit host between the look and the locks being taken as
/// one: the old tools' state appears and the kit's file goes while the first probe runs, so
/// the locked look reads an old host, and the run ends (exit 5) with nothing migrated.
///
/// Mutant seen red: the kind not compared under the locks.
#[test]
fn a_kit_host_that_becomes_an_old_host_under_the_migration_is_changed() {
    let mut rig = Rig::kit("flip", Platform::MacOs, Form::New);
    let (state, marker) = (rig.old_hand(), rig.at("usr/local/kbf/etc/av2_mac.conf"));
    let dir = rig.at("var/db/kbf-deploy");
    let flip = runner_between(move || {
        fs::create_dir_all(&dir).expect("dir");
        state.state(Some(CURRENT), Some(PREVIOUS), None, None);
        fs::remove_file(&marker).expect("rm");
    });
    let error = run_with(&mut rig, &flip, &never(), &NoFrontend, Mode::Apply).expect_err("changed");
    assert!(matches!(error, MigrateError::Changed(_)), "{error}");
    assert_eq!(error.exit().code(), Exit::NeedsOperator.code());
    assert!(!rig.new_state_dir().join("state.json").exists());
}

/// Catches the version probe asking a release for the wrong binary. A host the kit installed
/// holds only `kbf` in a release (on either platform), so it is asked `kbf` even where the
/// release also holds `kbf-daemon`; an old-host Mac is asked `kbf-daemon` when its release
/// holds one and `kbf` when it does not (the old `state.json` names no binary); Linux is
/// asked `kbf` whatever else is there.
///
/// Mutants seen red: `kbf-daemon` on every Mac; `kbf` on every Mac; `holds_daemon` ignored;
/// `kit` ignored.
#[test]
fn the_release_binary_probed_depends_on_the_host_not_only_the_platform() {
    let want = [
        (Platform::Linux, false, false, "kbf"),
        (Platform::Linux, false, true, "kbf"),
        (Platform::Linux, true, true, "kbf"),
        (Platform::MacOs, true, false, "kbf"),
        (Platform::MacOs, true, true, "kbf"),
        (Platform::MacOs, false, false, "kbf"),
        (Platform::MacOs, false, true, "kbf-daemon"),
    ];
    for (platform, kit, daemon, binary) in want {
        assert_eq!(
            Roots::new(platform).binary(kit, daemon),
            binary,
            "{platform:?} kit={kit} daemon={daemon}"
        );
    }
}

/// Catches a kit host probed for `kbf-daemon` because a release happens to hold one: the
/// kit's releases are asked `kbf` whatever else is in them.
///
/// Mutant seen red: `Roots::binary` not told the host is the kit's.
#[test]
fn a_kit_host_is_probed_for_kbf_even_where_a_release_holds_kbf_daemon() {
    let mut rig = Rig::kit("daemon-too", Platform::MacOs, Form::New);
    for sha in [CURRENT, PREVIOUS] {
        put(
            &rig.at(&format!("usr/local/kbf/releases/{sha}/kbf-daemon")),
            "daemon",
            0o755,
        );
    }
    let runner = runner();
    run_with(&mut rig, &runner, &never(), &NoFrontend, Mode::Apply).expect("migrates");
    let programs: Vec<_> = runner.calls().iter().map(|c| c.program.clone()).collect();
    assert_eq!(programs.len(), 2);
    assert!(programs.iter().all(|p| p.ends_with("kbf")), "{programs:?}");
}

/// What a finished migration leaves, to compare a resumed one with: the plist, the
/// installer's tree and state directory, and the kit's tree.
fn whole(rig: &Rig) -> (String, BTreeMap<String, String>, BTreeMap<String, String>) {
    (
        fs::read_to_string(at(&rig.host.root, PLIST_PATH)).expect("plist"),
        rig.snapshot("usr/local/kbf-installer"),
        rig.snapshot("usr/local/kbf"),
    )
}

fn journal_path(rig: &Rig) -> std::path::PathBuf {
    rig.new_state_dir().join("migration.json")
}

/// Catches a run that died after it repointed the plist leaving a host no re-run can finish:
/// `start` fails (the plist is repointed, the journal says `started`), and the next run
/// reads the plist as the kit's anyway, finishes (`Migrated`, links, state, journal `done`,
/// the daemon running) and leaves exactly what an uninterrupted run leaves. Both forms.
///
/// Mutant seen red: the repointed form not accepted on a resume (the re-run is `UnknownPlist`).
#[test]
fn a_kit_run_that_died_at_the_start_is_finished_by_the_next() {
    for form in [Form::New, Form::Old] {
        let mut whole_run = Rig::kit(&format!("whole-{form:?}"), Platform::MacOs, form);
        run_migrate(&mut whole_run, &never(), Mode::Apply).expect("whole");

        let mut rig = Rig::kit(&format!("died-{form:?}"), Platform::MacOs, form);
        rig.fail_start.set(true);
        let error = run_migrate(&mut rig, &never(), Mode::Apply).expect_err("start fails");
        assert!(
            error.to_string().contains("planted: start failed"),
            "{error}"
        );
        let repointed = plist_of(form).replace("/usr/local/kbf/current", &new_bin(&rig));
        assert_eq!(
            fs::read_to_string(at(&rig.host.root, PLIST_PATH)).expect("plist"),
            repointed,
            "the plist is repointed when the run dies"
        );
        assert_eq!(rig.json(&journal_path(&rig))["phase"], "started");
        assert!(!rig.active.borrow().contains("com.komira.kbf-daemon.plist"));

        rig.fail_start.set(false);
        let report = run_migrate(&mut rig, &never(), Mode::Apply)
            .unwrap_or_else(|e| panic!("{form:?}: {e}"));
        assert_eq!(report.outcome, Outcome::Migrated, "{form:?}");
        assert_eq!(rig.json(&journal_path(&rig))["phase"], "done");
        assert!(rig.active.borrow().contains("com.komira.kbf-daemon.plist"));
        let links = rig.new_tree().join("releases");
        for (name, sha) in [("current", CURRENT), ("previous", PREVIOUS)] {
            assert_eq!(
                fs::read_link(links.join(name)).expect("link").to_str(),
                Some(sha)
            );
        }
        assert_eq!(whole(&rig), whole(&whole_run), "{form:?}");
    }
}

/// Catches a resume that is not the same migration finished: a run that died at each step
/// from the journal to the rewrite is simulated by rewinding a finished host by hand (the
/// files the later steps wrote are removed, the plist is the kit's again where the rewrite
/// had not happened, the journal says `started`); the next run must finish with the host
/// byte for byte as the uninterrupted run left it. The points: the journal and the copies
/// only; the state written; the links made; `status.json` written; the plist repointed.
/// A real death at the start is the test above, and one after the stop the one below. The
/// rig cannot fail a step between those, hence the rewind.
///
/// Not run as mutants: the state, the links or the status not rewritten on a resume.
#[test]
fn a_kit_run_that_died_at_any_step_is_finished_by_the_next() {
    // What each point has written by then: (state, helper memory, links, status, plist).
    let points: [(&str, [bool; 5]); 5] = [
        ("journal", [false, false, false, false, false]),
        ("state", [true, true, false, false, false]),
        ("links", [true, true, true, false, false]),
        ("status", [true, true, true, true, false]),
        ("plist", [true, true, true, true, true]),
    ];
    for form in [Form::New, Form::Old] {
        let mut rig = Rig::kit(&format!("steps-{form:?}"), Platform::MacOs, form);
        run_migrate(&mut rig, &never(), Mode::Apply).expect("whole");
        let finished = whole(&rig);
        let (state, tree) = (rig.new_state_dir(), rig.new_tree());
        let journal = fs::read_to_string(journal_path(&rig)).expect("journal");
        for (point, [has_state, has_memory, has_links, has_status, has_plist]) in points {
            let kept = |keep: bool, path: std::path::PathBuf| {
                if !keep && path.symlink_metadata().is_ok() {
                    fs::remove_file(&path).expect("rm");
                }
            };
            kept(has_state, state.join("state.json"));
            kept(has_memory, state.join("helper.json"));
            kept(has_links, tree.join("releases/current"));
            kept(has_links, tree.join("releases/previous"));
            kept(has_status, state.join("status.json"));
            if !has_plist {
                fs::write(at(&rig.host.root, PLIST_PATH), plist_of(form)).expect("plist");
            }
            fs::write(
                journal_path(&rig),
                journal.replace("\"phase\": \"done\"", "\"phase\": \"started\""),
            )
            .expect("journal");
            let report = run_migrate(&mut rig, &never(), Mode::Apply)
                .unwrap_or_else(|e| panic!("{form:?} {point}: {e}"));
            assert_eq!(report.outcome, Outcome::Migrated, "{form:?} {point}");
            assert_eq!(whole(&rig), finished, "{form:?} {point}");
        }
    }
}

/// Catches a run that died after the services stopped and before anything was written: the
/// plist becomes writable by others while the daemon is being stopped, so the re-read of the
/// host (`settle`) refuses; the daemon is started again as the run ends, nothing but the
/// journal is written, and once the plist is right the next run migrates. This is the resume
/// of a journal whose plist is still the kit's.
///
/// Not run as a mutant: the services not started again on that refusal (existing code).
#[test]
fn a_kit_run_refused_after_the_stop_is_finished_by_the_next() {
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::kit("settle", Platform::MacOs, Form::New);
    let plist = at(&rig.host.root, PLIST_PATH);
    rig.active
        .borrow_mut()
        .insert("com.komira.kbf-daemon.plist".to_owned());
    let hook_plist = plist.clone();
    rig.on_command(move |verb, _| {
        if verb == "stop" {
            fs::set_permissions(&hook_plist, fs::Permissions::from_mode(0o666)).expect("chmod");
        }
    });
    let error = run_migrate(&mut rig, &never(), Mode::Apply).expect_err("refused");
    assert!(error.to_string().contains("writable"), "{error}");
    assert!(!rig.new_state_dir().join("state.json").exists());
    assert_eq!(rig.json(&journal_path(&rig))["phase"], "started");
    assert_eq!(fs::read_to_string(&plist).expect("plist"), PLIST_NEW);
    assert!(rig.active.borrow().contains("com.komira.kbf-daemon.plist"));

    fs::set_permissions(&plist, fs::Permissions::from_mode(0o644)).expect("chmod");
    rig.on_command(|_, _| {});
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("finish");
    assert_eq!(report.outcome, Outcome::Migrated);
}

/// Catches the repointed plist accepted without a kit migration to resume. The same host as
/// the first test (died at the start), with the journal changed to say what it must not: an
/// old host's migration (`kit` false), a rolled-back one, and none at all. Each is refused as
/// `UnknownPlist` (exit 3) and the host is left as it was; with the journal back it resumes,
/// and a finished journal is `AlreadyMigrated`.
///
/// Mutant seen red: the repointed form accepted whatever the journal says.
#[test]
fn the_repointed_plist_is_the_kits_only_with_a_kit_journal() {
    let mut rig = Rig::kit("nojournal", Platform::MacOs, Form::New);
    rig.fail_start.set(true);
    run_migrate(&mut rig, &never(), Mode::Apply).expect_err("start fails");
    rig.fail_start.set(false);
    let journal = fs::read_to_string(journal_path(&rig)).expect("journal");
    let others = [
        journal.replace("\"kit\": true", "\"kit\": false"),
        journal.replace("\"phase\": \"started\"", "\"phase\": \"rolled_back\""),
    ];
    for (n, text) in others.iter().enumerate() {
        assert_ne!(*text, journal, "{n}");
        fs::write(journal_path(&rig), text).expect("journal");
        let error = refused(&mut rig);
        assert!(
            matches!(error, MigrateError::UnknownPlist(_)),
            "{n}: {error}"
        );
        assert_eq!(error.exit().code(), Exit::Refused.code());
    }
    fs::remove_file(journal_path(&rig)).expect("rm");
    let error = refused(&mut rig);
    assert!(matches!(error, MigrateError::UnknownPlist(_)), "{error}");

    fs::write(journal_path(&rig), &journal).expect("journal");
    let again = run_migrate(&mut rig, &never(), Mode::Apply).expect("resumed");
    assert_eq!(again.outcome, Outcome::Migrated);
    assert_eq!(
        run_migrate(&mut rig, &never(), Mode::Apply)
            .expect("done")
            .outcome,
        Outcome::AlreadyMigrated
    );
}

/// Catches `setup`'s guard naming a half-migrated kit host wrongly: after a run that died at
/// the start the plist is repointed, and the guard must still say the host is unmigrated
/// (naming the kit's tree), not that the plist is of no known form.
///
/// Mutant seen red: `standing` reading the plist without the journal.
#[test]
fn a_kit_host_that_died_at_the_start_is_still_unmigrated_to_setup() {
    let mut rig = Rig::kit("guard", Platform::MacOs, Form::Old);
    rig.fail_start.set(true);
    run_migrate(&mut rig, &never(), Mode::Apply).expect_err("start fails");
    let error = guard(&rig.layout, me()).expect_err("unmigrated");
    assert!(
        matches!(&error, MigrateError::Unmigrated(w) if w == "/usr/local/kbf"),
        "{error}"
    );
}

/// Catches a rollback after an interrupted run not putting the kit's plist back: the original
/// is kept the first time the plist is repointed, and neither the run that died nor the one
/// that finished it may replace it with the repointed copy. Rolled back from each (both
/// forms) the plist is the kit's, byte for byte, and the kit's tree is as it was.
///
/// Not run as a mutant: the backup overwritten by the second run's rewrite.
#[test]
fn rolling_back_after_an_interrupted_kit_run_restores_the_kits_plist() {
    for form in [Form::New, Form::Old] {
        for finished in [false, true] {
            let mut rig = Rig::kit(&format!("rb-{form:?}-{finished}"), Platform::MacOs, form);
            let kit_tree = rig.snapshot("usr/local/kbf");
            rig.fail_start.set(true);
            run_migrate(&mut rig, &never(), Mode::Apply).expect_err("start fails");
            rig.fail_start.set(false);
            if finished {
                run_migrate(&mut rig, &never(), Mode::Apply).expect("finish");
            }
            let mut m = rig.migration();
            m.installer_unit = None;
            let report = rollback(&m, &mut rig.host, Mode::Apply).expect("rollback");
            assert_eq!(report.outcome, Outcome::RolledBack);
            assert_eq!(
                fs::read_to_string(at(&rig.host.root, PLIST_PATH)).expect("plist"),
                plist_of(form),
                "{form:?} finished={finished}: byte for byte"
            );
            assert_eq!(rig.snapshot("usr/local/kbf"), kit_tree);
            assert!(rig.active.borrow().contains("com.komira.kbf-daemon.plist"));
        }
    }
}
