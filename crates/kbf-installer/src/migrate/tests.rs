//! The migration end to end on a scratch host with the old layout: what it writes, in
//! what order it stops, copies, repoints and starts, and what a repeat does.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::rc::Rc;

use serde_json::json;

use super::rig::*;
use super::*;
use crate::state::State;
use crate::testkit::{A, B};

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

/// The pipeline played: it installs `releases/<claimed>` with the marker, from bytes of its own.
fn installing(rig: &Rig, calls: Rc<RefCell<Vec<(String, String)>>>) -> FnVerifier {
    let tree = rig.new_tree().join("releases");
    FnVerifier(Box::new(move |claimed, probed| {
        calls
            .borrow_mut()
            .push((claimed.to_string(), probed.commit.clone()));
        let dir = tree.join(claimed.as_str());
        put(&dir.join("kbf"), "verified bytes", 0o755);
        put(&dir.join("VERIFIED"), "", 0o644);
        Ok(())
    }))
}

/// Catches the heart of the migration: a Linux host with the dev installer's layout
/// comes over whole. Both releases are copied with the old bytes and no `VERIFIED`, the
/// links are relative, `state.json` is schema 2 with the floor carried and seeded,
/// `status.json` names both releases as unverified, the unit points at the new link, the
/// old helper is off, and the old tree and state are exactly as they were.
///
/// Mutants seen red: a `VERIFIED` written for a copy; the status without the
/// `unverified` list; `previous` not adopted; the floor's order id not carried.
#[test]
fn a_dev_installer_host_is_adopted_unverified_and_the_old_layout_is_kept() {
    let mut rig = Rig::linux("full");
    let old_tree = rig.snapshot("usr/local/kbf");
    let old_state = rig.snapshot("var/lib/kbf-deploy");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");

    assert_eq!(report.outcome, Outcome::Migrated);
    assert_eq!(report.unverified, vec![commit(A), commit(B)]);
    assert_eq!(report.min_generation, Some(17));
    let shown = report.to_string();
    assert!(shown.contains("UNVERIFIED"), "{shown}");
    assert!(
        shown.contains("copied UNVERIFIED: no pipeline here"),
        "{shown}"
    );
    assert!(!shown.contains("NO REPLAY FLOOR"), "{shown}");

    let tree = rig.new_tree();
    for (c, body) in [(A, "binary of A"), (B, "binary of B")] {
        let dir = tree.join("releases").join(c);
        assert_eq!(fs::read_to_string(dir.join("kbf")).expect("binary"), body);
        assert!(
            !dir.join("VERIFIED").exists(),
            "a copy never carries the marker"
        );
        assert_eq!(
            fs::metadata(dir.join("kbf")).expect("meta").mode() & 0o777,
            0o755
        );
    }
    assert_eq!(
        fs::read_link(tree.join("releases/current"))
            .expect("link")
            .to_str(),
        Some(A)
    );
    assert_eq!(
        fs::read_link(tree.join("releases/previous"))
            .expect("link")
            .to_str(),
        Some(B)
    );

    let state = rig.json(&tree.join("state.json"));
    assert_eq!(
        state,
        json!({
            "schema": 2, "current": A, "previous": B, "staged": null, "pending_confirm": null,
            "floor": {"epoch": 3, "order_id": 42, "generation": 17},
            "serving_instance": null, "last_acked_order_id": 42, "unacked": null
        })
    );
    assert_eq!(
        fs::metadata(tree.join("state.json")).expect("meta").mode() & 0o777,
        0o600
    );
    let status = rig.json(&tree.join("status.json"));
    assert_eq!(status["unverified"], json!([A, B]));
    assert_eq!(status["installed_commit"], json!(A));
    assert_eq!(status["state"], json!("idle"));
    assert_eq!(status["probe"]["commit"], json!(A));
    assert_eq!(status["probe"]["protocol"], json!(4));
    assert_eq!(
        fs::metadata(tree.join("status.json")).expect("meta").mode() & 0o777,
        0o644
    );

    let unit = fs::read_to_string(rig.at("etc/systemd/system/kbf-daemon.service")).expect("unit");
    assert_eq!(
        unit,
        "[Service]\nExecStart=/var/lib/kbf-installer/releases/current/kbf daemon\n"
    );
    assert_eq!(
        fs::read_to_string(tree.join("migration-units/kbf-daemon.service")).expect("backup"),
        OLD_UNIT
    );
    let journal = rig.json(&tree.join("migration.json"));
    assert_eq!(journal["phase"], json!("done"));

    assert_eq!(
        rig.snapshot("usr/local/kbf"),
        old_tree,
        "the old tree is untouched"
    );
    let mut after = rig.snapshot("var/lib/kbf-deploy");
    assert!(
        after
            .remove("MIGRATED")
            .expect("the note")
            .contains("rollback path")
    );
    assert_eq!(after, old_state, "the old state is untouched");
}

/// Catches a service that runs from a tree being replaced, and a helper left to manage
/// the host: the helper and the unit stop before the links exist, the helper is disabled
/// (not restarted), and the unit starts only after the links, the state and the unit file
/// are in place.
///
/// Mutants seen red: stopping after `set_link` (`links=true` on the stop events); starting
/// the helper again with the units.
#[test]
fn services_stop_before_the_links_move_and_start_after_everything_is_written() {
    let mut rig = Rig::linux("order");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    assert_eq!(
        *rig.events.borrow(),
        [
            "stop kbf-helper.service links=false",
            "stop kbf-daemon.service links=false",
            "disable kbf-helper.service links=false",
            "daemon-reload links=true",
            "start kbf-daemon.service links=true",
        ]
    );
    let active = rig.active.borrow();
    assert!(active.contains("kbf-daemon.service") && !active.contains("kbf-helper.service"));
}

/// Catches a dry run that changes anything: no file under the root differs afterwards, no
/// service is touched, and the plan says what the real run would do.
///
/// Mutant seen red: the `Mode::DryRun` early return moved below the stop.
#[test]
fn a_dry_run_changes_nothing() {
    let mut rig = Rig::linux("dry");
    let before = rig.snapshot("");
    let report = run_migrate(&mut rig, &never(), Mode::DryRun).expect("plan");
    assert_eq!(report.outcome, Outcome::Planned);
    assert_eq!(rig.snapshot(""), before);
    assert!(rig.host.log.is_empty() && rig.events.borrow().is_empty());
    let shown = report.to_string();
    assert!(
        shown.contains("DRY-RUN: release") && shown.contains("kbf-helper.service: stop it"),
        "{shown}"
    );
    assert_eq!(report.min_generation, Some(17));
    rig.active.borrow_mut().remove("kbf-daemon.service");
    let report = run_migrate(&mut rig, &never(), Mode::DryRun).expect("plan");
    assert!(
        report
            .to_string()
            .contains("kbf-daemon.service: it is not running")
    );
}

/// Catches trust in the verify pipeline's word or a skipped pipeline: a release it installs
/// with the marker is adopted verified with the pipeline's bytes (no copy of the old
/// ones), it is called with the commit the probe named, and one that says Ok without
/// writing the marker is treated as unverified, with that reason.
///
/// Mutants seen red: the `verified` re-check after `Ok(())` removed; the pipeline called
/// with the directory name's commit but not the probe's version.
#[test]
fn the_verify_pipeline_decides_and_its_ok_is_checked() {
    let mut rig = Rig::linux("verified");
    let calls = Rc::new(RefCell::new(Vec::new()));
    let pipeline = installing(&rig, calls.clone());
    let report = run_migrate(&mut rig, &pipeline, Mode::Apply).expect("migrate");
    assert!(report.unverified.is_empty());
    assert!(!report.to_string().contains("UNVERIFIED"));
    let dir = rig.new_tree().join("releases").join(A);
    assert_eq!(
        fs::read_to_string(dir.join("kbf")).expect("bytes"),
        "verified bytes"
    );
    assert!(dir.join("VERIFIED").exists());
    assert_eq!(
        *calls.borrow(),
        [
            (A.to_owned(), A[..12].to_owned()),
            (B.to_owned(), B[..12].to_owned())
        ]
    );
    assert_eq!(
        rig.json(&rig.new_tree().join("status.json"))["unverified"],
        json!([])
    );

    let mut rig = Rig::linux("liar");
    let liar = FnVerifier(Box::new(|_, _| Ok(())));
    let report = run_migrate(&mut rig, &liar, Mode::Apply).expect("migrate");
    assert_eq!(report.unverified, vec![commit(A), commit(B)]);
    assert!(report.to_string().contains("wrote no VERIFIED marker"));
    assert!(
        !rig.new_tree()
            .join("releases")
            .join(A)
            .join("VERIFIED")
            .exists()
    );
}

/// Catches a probe that runs with more than it needs: the old binary is asked for
/// `--version --json` (and nothing else) by the path in the old tree, as the fetch user,
/// with an empty environment, from `/`, for at most ten seconds.
///
/// Mutants seen red: `env` passed through; `run_as` None; the wrong binary on macOS.
#[test]
fn the_probe_runs_unprivileged_bounded_and_with_nothing() {
    let mut rig = Rig::linux("spawn");
    let seen = runner();
    run_with(&mut rig, &seen, &never(), &NoFrontend, Mode::Apply).expect("migrate");
    let calls = seen.calls();
    assert_eq!(calls.len(), 2);
    for (spawn, c) in calls.iter().zip([A, B]) {
        assert_eq!(
            spawn.program,
            rig.at(&format!("usr/local/kbf/releases/{c}/kbf"))
        );
        assert_eq!(spawn.args, ["--version", "--json"]);
        assert!(spawn.env.is_empty());
        assert_eq!(spawn.cwd.to_str(), Some("/"));
        assert_eq!(
            spawn.run_as,
            Some(RunAs {
                uid: 65534,
                gid: 65534
            })
        );
        assert_eq!(spawn.timeout, std::time::Duration::from_secs(10));
    }
}

/// Catches a repeat that does harm: after a finished migration `migrate` is a no-op (no
/// command runs, no file changes), and on a host with no old layout it says so.
#[test]
fn a_finished_migration_is_not_run_again_and_a_new_host_has_nothing_to_do() {
    let mut rig = Rig::linux("again");
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    let (files, commands) = (rig.snapshot(""), rig.events.borrow().len());
    let again = run_migrate(&mut rig, &never(), Mode::Apply).expect("again");
    assert_eq!(again.outcome, Outcome::AlreadyMigrated);
    assert_eq!(rig.snapshot(""), files);
    assert_eq!(rig.events.borrow().len(), commands);

    let mut fresh = Rig::linux("fresh");
    fs::remove_file(fresh.at("var/lib/kbf-deploy/state.json")).expect("rm");
    fs::remove_file(fresh.at("var/lib/kbf-deploy/helper.json")).expect("rm");
    let none = run_migrate(&mut fresh, &never(), Mode::Apply).expect("nothing");
    assert_eq!(none.outcome, Outcome::NothingToMigrate);
    assert!(!fresh.new_tree().exists());
}

/// Catches a crash between the stop and the start that strands a service: the first run
/// stops the units and fails to start the daemon; the second finds the daemon not running,
/// still starts it (the journal remembers), leaves the helper stopped, keeps the copy it
/// already made (same directory, not recopied), and does not lower a floor that is higher
/// than what it would seed.
///
/// Mutants seen red: `stopped` rebuilt from what is running (the daemon never restarted);
/// the floor written with `seeded.floor` instead of `raise`; the copy always redone.
#[test]
fn a_run_that_died_before_the_start_is_finished_by_the_next() {
    let mut rig = Rig::linux("crash");
    rig.fail_start.set(true);
    let err = run_migrate(&mut rig, &never(), Mode::Apply).expect_err("start fails");
    assert!(err.to_string().contains("planted: start failed"), "{err}");
    assert!(rig.active.borrow().is_empty(), "both stopped, none started");
    let tree = rig.new_tree();
    assert_eq!(
        rig.json(&tree.join("migration.json"))["phase"],
        json!("started")
    );
    let inode = fs::metadata(tree.join("releases").join(A))
        .expect("meta")
        .ino();
    // Someone with a higher floor than this run would seed (an earlier run with a flag).
    let mut state = rig.json(&tree.join("state.json"));
    state["floor"]["generation"] = json!(99);
    fs::write(
        tree.join("state.json"),
        serde_json::to_vec(&state).expect("json"),
    )
    .expect("write");

    rig.fail_start.set(false);
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("finish");
    assert_eq!(report.outcome, Outcome::Migrated);
    let active = rig.active.borrow();
    assert!(active.contains("kbf-daemon.service") && !active.contains("kbf-helper.service"));
    assert_eq!(
        fs::metadata(tree.join("releases").join(A))
            .expect("meta")
            .ino(),
        inode
    );
    let state = rig.json(&tree.join("state.json"));
    assert_eq!(
        state["floor"]["generation"],
        json!(99),
        "a floor only goes up"
    );
    assert_eq!(
        rig.json(&tree.join("migration.json"))["phase"],
        json!("done")
    );
    let unit = fs::read_to_string(rig.at("etc/systemd/system/kbf-daemon.service")).expect("unit");
    assert!(unit.contains("/var/lib/kbf-installer/releases/current/kbf"));
}

/// Catches a leftover that survives or is trusted: a half copy in `work/` is swept, and an
/// unreferenced release directory with other content and no marker is replaced by the old
/// release's bytes; one with the same content is kept as it is.
///
/// Mutants seen red: `sweep_work` removed; the `Some(_)` arm of `adopt` keeping the stale dir.
#[test]
fn leftovers_of_an_interrupted_copy_are_swept_or_replaced() {
    let mut rig = Rig::linux("leftover");
    let tree = rig.new_tree();
    put(&tree.join("work/adopt-leftover.partial/junk"), "x", 0o644);
    put(&tree.join("work/keep-me"), "x", 0o644);
    put(
        &tree.join("releases").join(A).join("kbf"),
        "a stale half copy",
        0o755,
    );
    run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    assert!(!tree.join("work/adopt-leftover.partial").exists());
    assert!(
        tree.join("work/keep-me").exists(),
        "only its own scratch is swept"
    );
    assert_eq!(
        fs::read_to_string(tree.join("releases").join(A).join("kbf")).expect("bytes"),
        "binary of A"
    );
    assert!(tree.join("releases").join(A).join("manifest.json").exists());
    assert!(fs::read_dir(tree.join("work")).expect("work").all(|e| {
        !e.expect("entry")
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")
    }));
}

/// Catches a previous release that is invented or dropped: a host with no `previous`
/// adopts one release, makes no `previous` link, and a stale `previous` link from an earlier
/// run is removed.
#[test]
fn a_host_without_a_previous_release_gets_no_previous_link() {
    let mut rig = Rig::linux("noprev");
    fs::remove_file(rig.at("usr/local/kbf/previous")).expect("rm");
    rig.write_old_state(&State {
        current: Some(commit(A)),
        ..State::default()
    });
    let tree = rig.new_tree();
    fs::create_dir_all(tree.join("releases")).expect("releases");
    std::os::unix::fs::symlink(B, tree.join("releases/previous")).expect("stale link");
    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("migrate");
    assert_eq!(report.unverified, vec![commit(A)]);
    assert!(tree.join("releases/previous").symlink_metadata().is_err());
    assert!(!tree.join("releases").join(B).exists());
    let state = rig.json(&tree.join("state.json"));
    assert_eq!(
        (state["current"].clone(), state["previous"].clone()),
        (json!(A), json!(null))
    );
}

/// Catches a unit that does not run kbf being rewritten, a missing one failing the run,
/// and the report not saying which: three units, one of each kind.
#[test]
fn each_unit_is_rewritten_left_alone_or_skipped_and_the_report_says_which() {
    let mut rig = Rig::linux("units");
    put(
        &rig.at("etc/systemd/system/other.service"),
        "[Service]\nExecStart=/bin/true\n",
        0o644,
    );
    let mut m = rig.migration();
    m.units = vec![
        unit("kbf-daemon.service"),
        unit("other.service"),
        unit("absent.service"),
    ];
    let report = migrate(
        &m,
        &mut rig.host,
        &Hooks {
            runner: &runner(),
            verifier: &never(),
            frontend: &NoFrontend,
        },
        Mode::Apply,
    )
    .expect("migrate");
    let shown = report.to_string();
    assert!(shown.contains("kbf-daemon.service: repointed"), "{shown}");
    assert!(
        shown.contains("other.service: does not run kbf from the old link"),
        "{shown}"
    );
    assert!(
        shown.contains("absent.service: no such unit file"),
        "{shown}"
    );
    assert_eq!(
        fs::read_to_string(rig.at("etc/systemd/system/other.service")).expect("unit"),
        "[Service]\nExecStart=/bin/true\n"
    );
}

/// Catches the macOS path using Linux's: a Mac host's plist is rewritten to the Mac tree
/// (`/usr/local/kbf-installer`), the state goes to `/var/db/kbf-installer`, the probe asks
/// `kbf-daemon`, launchd is stopped with `bootout`, restarted with `bootstrap`, and the
/// helper is disabled in the system domain.
///
/// Mutants seen red: the Linux roots used on macOS; `bootstrap` given the label, not the plist.
#[test]
fn a_mac_host_comes_over_through_launchd() {
    let mut rig = Rig::mac("mac");
    let seen = runner();
    run_with(&mut rig, &seen, &never(), &NoFrontend, Mode::Apply).expect("migrate");
    assert!(
        seen.calls()[0]
            .program
            .ends_with("releases/".to_owned() + A + "/kbf-daemon")
    );
    let tree = rig.new_tree();
    assert_eq!(tree, rig.at("usr/local/kbf-installer"));
    assert_eq!(
        fs::read_link(tree.join("releases/current"))
            .expect("link")
            .to_str(),
        Some(A)
    );
    assert!(
        rig.at("var/db/kbf-installer/state.json").exists() && !tree.join("state.json").exists()
    );
    let plist = fs::read_to_string(rig.at("Library/LaunchDaemons/com.komira.kbf-daemon.plist"))
        .expect("plist");
    assert!(
        plist.contains("<string>/usr/local/kbf-installer/releases/current/kbf-daemon</string>"),
        "{plist}"
    );
    assert_eq!(
        *rig.events.borrow(),
        [
            "stop com.komira.kbf-helper.plist links=false",
            "stop com.komira.kbf-daemon.plist links=false",
            "disable com.komira.kbf-helper.plist links=false",
            "start com.komira.kbf-daemon.plist links=true",
        ]
    );
    let bootstraps: Vec<_> = rig
        .host
        .log
        .iter()
        .map(ToString::to_string)
        .filter(|l| l.contains("bootstrap"))
        .collect();
    assert_eq!(bootstraps.len(), 1);
    assert!(
        bootstraps[0]
            .ends_with("bootstrap system /Library/LaunchDaemons/com.komira.kbf-daemon.plist"),
        "{bootstraps:?}"
    );
}

/// Catches the state written beyond what the golden fixtures pin: the bytes of a
/// migrated `state.json` and `status.json` (key order and all), so the next build reads
/// what this one wrote.
#[test]
fn the_written_files_have_the_golden_shape() {
    let state = schema::NewState::migrated(
        Some(commit(A)),
        None,
        schema::Floor {
            epoch: 3,
            order_id: 42,
            generation: 17,
        },
    );
    let status = schema::NewStatus::migrated(&state, None, vec![commit(A)]);
    assert_eq!(
        String::from_utf8(state.render()).expect("utf8"),
        format!(
            "{{\n  \"schema\": 2,\n  \"current\": \"{A}\",\n  \"previous\": null,\n  \"staged\": null,\n  \
             \"pending_confirm\": null,\n  \"floor\": {{\n    \"epoch\": 3,\n    \"order_id\": 42,\n    \
             \"generation\": 17\n  }},\n  \"serving_instance\": null,\n  \"last_acked_order_id\": 42,\n  \
             \"unacked\": null\n}}\n"
        )
    );
    assert_eq!(
        String::from_utf8(status.render()).expect("utf8"),
        format!(
            "{{\n  \"schema\": 2,\n  \"installed_commit\": \"{A}\",\n  \"previous_commit\": null,\n  \
             \"staged_commit\": null,\n  \"state\": \"idle\",\n  \"refusal\": null,\n  \"acked_order_id\": 42,\n  \
             \"staged\": null,\n  \"probe\": null,\n  \"trust_days_left\": null,\n  \"unverified\": [\n    \"{A}\"\n  ]\n}}\n"
        )
    );
    assert_eq!(
        schema::NewState::parse(&state.render()).expect("round trip"),
        state
    );
    assert!(schema::NewState::parse(b"{\"schema\": 1}").is_err());
    let mut other = state.clone();
    other.schema = 3;
    assert!(schema::NewState::parse(&other.render()).is_err());
}

/// Catches a failed copy leaving a half release behind: a source that cannot be copied (a
/// link inside it) leaves neither the scratch directory in `work/` nor a release directory,
/// so the next run starts clean and nothing half-written can be linked.
///
/// Mutant seen red: the scratch directory not removed when `fill` fails.
#[test]
fn a_copy_that_fails_leaves_no_scratch_and_no_release() {
    let rig = Rig::linux("copy-fails");
    std::os::unix::fs::symlink(
        "/etc/passwd",
        rig.at(&format!("usr/local/kbf/releases/{A}/passwd")),
    )
    .expect("link");
    let rules = tree::Rules::release(me());
    let new = super::newtree::NewTree::create(&rig.layout, &rig.roots(), &rules).expect("tree");
    let src = tree::open_root(&rig.at(&format!("usr/local/kbf/releases/{A}"))).expect("open");
    assert!(new.copy_in(&commit(A), &src).is_err());
    let tree = rig.new_tree();
    assert_eq!(fs::read_dir(tree.join("work")).expect("work").count(), 0);
    assert!(!tree.join("releases").join(A).exists());
}
