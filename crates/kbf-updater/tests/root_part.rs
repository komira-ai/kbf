//! The root part of the host updater (`kbf-updater root`, docs/design/host-updates.md 3.3 to 3.5)
//! over a world in a scratch directory: declarations in a directory like `/etc/updater.d`, staged
//! and sealed releases, and the hook of a fake service, run through the step runner and the
//! transient-unit command of `update_rights` by a `systemd-run` that runs the hook directly.
//!
//! No test here waits for a time: the clock moves only when a step's wait moves it.

mod root_support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_step_runner::{Clock, CrashPoint};
use kbf_updater::root::socket::{self, Answer, Request};
use kbf_updater::root::{RootPart, boot};
use root_support::{FakeClock, World, my_uid, property, word};
use update_hook::{Class, Phase, Step, Verdict};

const SUCCEEDED: Verdict = Verdict::Succeeded {
    promotion_pending: false,
};

fn verdict(answer: &Answer) -> Verdict {
    answer
        .verdict
        .clone()
        .unwrap_or_else(|| panic!("no verdict: {answer:?}"))
}

/// Catches the root part not running a declared hook through the step runner: an update of the
/// fake service runs `prepare`, `cutover`, `verify` and `cleanup` in that order, each in the unit
/// the runner names, `prepare` with the staged release and every step with the same params file;
/// the step record is written and says the update succeeded, and the release is `current`.
#[test]
fn an_update_runs_the_four_steps_in_order_and_writes_the_step_record() {
    let world = World::new("e2e");
    world.declare("svc", &world.tierstore_shaped());
    let tree = world.stage("svc", "2.0.0", None);
    let root = world.root_part();

    let answer = root.update("svc", "2.0.0");
    assert!(answer.ok, "{answer:?}");
    assert_eq!(verdict(&answer), SUCCEEDED);
    assert_eq!(
        world.steps("svc"),
        ["prepare", "cutover", "verify", "cleanup"]
    );

    let launched = world.launched();
    let units: Vec<String> = launched
        .iter()
        .map(|run| property_unit(&root_support::words(run)))
        .collect();
    assert_eq!(
        units,
        [
            "updater-svc-prepare-1",
            "updater-svc-cutover-1",
            "updater-svc-verify-1",
            "updater-svc-cleanup-1"
        ]
    );
    for run in &launched {
        assert_eq!(property(run, "User").as_deref(), Some("svc-update"));
    }
    let args = fs::read_to_string(world.hook_dir("svc").join("args.log")).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let params = world.path("state/svc/params.toml");
    assert_eq!(
        lines[0],
        format!("prepare {} --params {}", tree.display(), params.display())
    );
    assert_eq!(lines[1], format!("cutover --params {}", params.display()));

    let record = world.record("svc");
    assert_eq!(record["target_version"], "2.0.0");
    assert_eq!(record["step"], "cleanup");
    assert_eq!(record["state"]["state"], "finished");
    assert!(
        !params.exists(),
        "the params file is removed with the update"
    );
    let status = root.status("svc");
    assert_eq!(status.step, Some(Step::Cleanup));
    assert_eq!(status.state.as_deref(), Some("finished"));
    assert_eq!(status.verdict, Some(SUCCEEDED));
    assert_eq!(
        fs::read_link(world.path("releases/svc/current")).unwrap(),
        std::path::Path::new("2.0.0")
    );
}

fn property_unit(args: &[String]) -> String {
    args.iter()
        .find_map(|a| a.strip_prefix("--unit=").map(str::to_owned))
        .expect("a unit")
}

/// Catches a step or the boot entry launched with another set than the hook-wide one: for the
/// tierstore-shaped declaration (`CAP_NET_ADMIN` hook-wide, no `step_capabilities`), every one of
/// the five steps and the boot entry holds exactly `CAP_NET_ADMIN`, ambient and bounding, with
/// `NoNewPrivileges=yes`.
#[test]
fn every_step_and_the_boot_entry_hold_the_hook_wide_set() {
    let world = World::new("caps-wide");
    world.declare("svc", &world.tierstore_shaped());
    world.stage("svc", "1.0.0", None);
    world.stage("svc", "2.0.0", None);
    let root = world.root_part();
    assert_eq!(verdict(&root.update("svc", "1.0.0")), SUCCEEDED);
    world.tell("svc", "verify", "1 {\"reason\":\"not_serving\"}");
    let failed = root.update("svc", "2.0.0");
    assert_eq!(
        verdict(&failed),
        Verdict::Failed {
            phase: Phase::GateRelease,
            class: Class::Release
        }
    );
    let booted = root.start("svc", false);
    assert_eq!(booted.boot.as_deref(), Some("success"), "{booted:?}");

    let mut words: Vec<String> = Vec::new();
    for run in world.launched() {
        let what = word(&run);
        assert_eq!(
            property(&run, "AmbientCapabilities").as_deref(),
            Some("CAP_NET_ADMIN"),
            "{what}"
        );
        assert_eq!(
            property(&run, "CapabilityBoundingSet").as_deref(),
            Some("CAP_NET_ADMIN"),
            "{what}"
        );
        assert_eq!(
            property(&run, "NoNewPrivileges").as_deref(),
            Some("yes"),
            "{what}"
        );
        words.push(what);
    }
    words.sort();
    words.dedup();
    assert_eq!(
        words,
        [
            "cleanup", "cutover", "prepare", "restore", "rollback", "verify"
        ]
    );
}

/// Catches a step that does not take its own entry, or a step without one that does not fall
/// back to the hook-wide set: with `verify = []` and `cleanup = ["CAP_KILL"]`, `verify` holds
/// nothing and `cleanup` only `CAP_KILL`, and `prepare` and `cutover`, which have no entry,
/// hold the hook-wide `CAP_NET_ADMIN`. A declaration with no capabilities at all starts its boot
/// entry with none.
#[test]
fn a_step_with_an_entry_holds_only_that_list_and_one_without_the_hook_wide_set() {
    let world = World::new("caps-step");
    let text = format!(
        "{}[hook.step_capabilities]\nverify = []\ncleanup = [\"CAP_KILL\"]\n",
        world.tierstore_shaped()
    );
    world.declare("svc", &text);
    world.stage("svc", "1.0.0", None);
    let root = world.root_part();
    assert_eq!(verdict(&root.update("svc", "1.0.0")), SUCCEEDED);
    let held: Vec<(String, Option<String>)> = world
        .launched()
        .iter()
        .map(|run| (word(run), property(run, "AmbientCapabilities")))
        .collect();
    let caps = |s: &str| Some(s.to_owned());
    assert_eq!(
        held,
        [
            ("prepare".to_owned(), caps("CAP_NET_ADMIN")),
            ("cutover".to_owned(), caps("CAP_NET_ADMIN")),
            ("verify".to_owned(), caps("")),
            ("cleanup".to_owned(), caps("CAP_KILL")),
        ]
    );

    let bare = World::new("caps-none");
    bare.declare(
        "svc",
        &bare.declaration(
            "svc",
            "boot = \"restore\"\n",
            "user = \"svc-update\"\ngroup = \"svc-update\"\n",
        ),
    );
    bare.stage("svc", "1.0.0", None);
    let root = bare.root_part();
    assert_eq!(verdict(&root.update("svc", "1.0.0")), SUCCEEDED);
    assert_eq!(root.start("svc", false).boot.as_deref(), Some("success"));
    let boot = bare.launched().pop().unwrap();
    assert_eq!(word(&boot), "restore");
    assert_eq!(property(&boot, "AmbientCapabilities").as_deref(), Some(""));
    assert_eq!(
        property(&boot, "CapabilityBoundingSet").as_deref(),
        Some("")
    );
}

/// The scenario of a crash test: what the hook is told before the update.
#[derive(Clone, Copy, Debug)]
enum Scenario {
    Success,
    VerifyFails,
}

fn crash_world(name: &str, scenario: Scenario) -> World {
    let world = World::new(name);
    // No boot entry: the start pass of a restarted root part would add one to the steps.
    world.declare(
        "svc",
        &world.declaration(
            "svc",
            "",
            "user = \"svc-update\"\ngroup = \"svc-update\"\ncapabilities = [\"CAP_NET_ADMIN\"]\n",
        ),
    );
    world.stage("svc", "2.0.0", None);
    if let Scenario::VerifyFails = scenario {
        world.tell("svc", "verify", "1");
    }
    world
}

fn dedup(mut steps: Vec<String>) -> Vec<String> {
    steps.dedup();
    steps
}

/// Kills the root part at every write of the step record of one update (before it touched the
/// disk, after it was durable), starts a new one as after a restart in the same boot, and has the
/// agent send the order again: the verdict and the steps run are those of an update that was
/// never killed.
fn sweep(scenario: Scenario) {
    let base = crash_world(&format!("crash-base-{scenario:?}"), scenario);
    let writes = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&writes);
    let counting = base.root_part_gated(Arc::new(move |point: CrashPoint| {
        if let CrashPoint::BeforeWrite(n) = point {
            seen.fetch_max(n, Ordering::SeqCst);
        }
        false
    }));
    let want = verdict(&counting.update("svc", "2.0.0"));
    let want_steps = dedup(base.steps("svc"));
    let writes = writes.load(Ordering::SeqCst);
    assert!(writes >= 8, "{scenario:?}: {writes} writes");

    for n in 1..=writes {
        for at in [CrashPoint::BeforeWrite(n), CrashPoint::AfterWrite(n)] {
            let what = format!("{scenario:?} killed at {at:?}");
            // The name of a world is in its paths, which a declaration holds to `[a-z0-9._-]`.
            let side = ["before", "after"][usize::from(matches!(at, CrashPoint::AfterWrite(_)))];
            let world = crash_world(&format!("crash-{scenario:?}-{side}-{n}"), scenario);
            let killed = world
                .root_part_gated(Arc::new(move |point: CrashPoint| point == at))
                .update("svc", "2.0.0");
            assert_eq!(killed.token(), Some("runner"), "{what}: {killed:?}");
            let restarted = world.root_part();
            let started = restarted.start("svc", false);
            assert!(started.ok, "{what}: {started:?}");
            let again = restarted.update("svc", "2.0.0");
            assert_eq!(verdict(&again), want, "{what}");
            assert_eq!(dedup(world.steps("svc")), want_steps, "{what}");
            assert_eq!(world.record("svc")["state"]["state"], "finished", "{what}");
        }
    }
}

/// Catches a root part that loses its place when it is killed: an update killed at any write of
/// its step record resumes from the record and succeeds as an update never killed does.
#[test]
fn a_killed_root_part_resumes_a_successful_update_from_every_record_write() {
    sweep(Scenario::Success);
}

/// Catches a root part that loses a rollback when it is killed: an update whose `verify` fails,
/// killed at any write of its step record, still rolls back and ends `Failed`.
#[test]
fn a_killed_root_part_resumes_a_rolled_back_update_from_every_record_write() {
    sweep(Scenario::VerifyFails);
}

/// The record says `cutover` is about to start: the gate kills the root part once that is durable.
fn killed_before_cutover(world: &World) {
    let record = world.path("state/svc/step-record.json");
    let gate = move |point: CrashPoint| {
        let pending_cutover = fs::read(&record)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|rec| rec["step"] == "cutover" && rec["state"]["state"] == "pending");
        matches!(point, CrashPoint::AfterWrite(_)) && pending_cutover
    };
    let killed = world.root_part_gated(Arc::new(gate)).update("svc", "2.0.0");
    assert_eq!(killed.token(), Some("runner"), "{killed:?}");
    assert_eq!(world.record("svc")["step"], "cutover");
    assert_eq!(world.record("svc")["state"]["state"], "pending");
}

fn pending_cutover_world(name: &str) -> World {
    let world = World::new(name);
    world.declare("svc", &world.tierstore_shaped());
    world.stage("svc", "1.0.0", None);
    world.stage("svc", "2.0.0", None);
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    killed_before_cutover(&world);
    world
}

/// Catches a boot that runs a `cutover` it found pending, or that starts the units before the
/// rollback: after a boot the pending `cutover` of the update to 2.0.0 is rolled back (the table
/// of 3.2 for a killed `cutover`), the update ends `Failed`, and only then does the boot entry
/// run, from the release that was running, 1.0.0.
#[test]
fn a_pending_cutover_found_at_boot_is_rolled_back_before_the_boot_entry() {
    let world = pending_cutover_world("boot-cutover");
    let before = world.steps("svc").len();
    world.clock.reboot("boot-b");
    let root = world.root_part();
    let first =
        kbf_updater::root::boot::first_start_since_boot(&world.path("state"), &*world.clock)
            .unwrap();
    assert!(first, "the boot id changed");
    let started = root.start("svc", first);
    assert_eq!(
        started.verdict,
        Some(Verdict::Failed {
            phase: Phase::BeforeCommit,
            class: Class::Release
        }),
        "{started:?}"
    );
    assert_eq!(started.boot.as_deref(), Some("success"));
    assert_eq!(&world.steps("svc")[before..], ["rollback", "restore"]);
    let boot = world.launched().pop().unwrap();
    assert!(
        root_support::words(&boot)
            .iter()
            .any(|a| a.ends_with("/releases/svc/1.0.0/hook")),
        "{boot:?}"
    );
    assert_eq!(
        fs::read_link(world.path("releases/svc/current")).unwrap(),
        std::path::Path::new("1.0.0")
    );
}

/// Catches the boot rule applied to a restart that is not a boot: the root part restarted in
/// the same boot resumes the update it found with `cutover` pending, as after any crash, and it
/// succeeds.
#[test]
fn a_pending_cutover_found_after_a_restart_in_the_same_boot_is_run() {
    let world = pending_cutover_world("restart-cutover");
    let before = world.steps("svc").len();
    let root = world.root_part();
    let first =
        kbf_updater::root::boot::first_start_since_boot(&world.path("state"), &*world.clock)
            .unwrap();
    // The first start of this world's root part wrote no boot id: it is read as a boot.
    assert!(first);
    let first =
        kbf_updater::root::boot::first_start_since_boot(&world.path("state"), &*world.clock)
            .unwrap();
    assert!(!first, "the same boot");
    let started = root.start("svc", first);
    assert_eq!(started.verdict, Some(SUCCEEDED), "{started:?}");
    assert_eq!(
        &world.steps("svc")[before..],
        ["cutover", "verify", "cleanup", "restore"]
    );
}

/// Catches a declaration installed past the gate: a release whose declaration widens the active
/// one of a root hook (it adds a capability) is `Deferred` with `rights_change` and the digest
/// of the new file, runs nothing and installs nothing; once `approve` has recorded that digest,
/// the same order installs the declaration and runs.
#[test]
fn a_widened_root_declaration_is_deferred_until_its_digest_is_approved() {
    let world = World::new("widened");
    let active = world.declaration(
        "svc",
        "",
        "user = \"root\"\ngroup = \"root\"\ncapabilities = [\"CAP_DAC_OVERRIDE\"]\n",
    );
    world.declare("svc", &active);
    let wider = world.declaration(
        "svc",
        "",
        "user = \"root\"\ngroup = \"root\"\ncapabilities = [\"CAP_DAC_OVERRIDE\", \"CAP_NET_ADMIN\"]\n",
    );
    world.stage("svc", "2.0.0", Some(&wider));
    deferred_until_approved(&world, "2.0.0", &wider, "hook.capabilities");
    assert!(
        !world.path("state/svc/declaration.previous").exists(),
        "the kept declaration is dropped once the update succeeded"
    );
}

/// Catches a first install of a root declaration that runs without approval: with no active
/// declaration, the one the release carries is `Deferred` as `rights_change` (`first_install`)
/// and runs nothing until its digest is approved.
#[test]
fn a_first_install_of_a_root_declaration_is_deferred_until_its_digest_is_approved() {
    let world = World::new("first");
    let first = world.declaration(
        "svc",
        "",
        "user = \"root\"\ngroup = \"root\"\ncapabilities = [\"CAP_DAC_OVERRIDE\"]\n",
    );
    world.stage("svc", "1.0.0", Some(&first));
    deferred_until_approved(&world, "1.0.0", &first, "first_install");
}

fn deferred_until_approved(world: &World, version: &str, carried: &str, widened: &str) {
    let file = world.path("updater.d/svc.toml");
    let before = fs::read(&file).ok();
    let root = world.root_part();
    let deferred = root.update("svc", version);
    assert!(deferred.ok, "{deferred:?}");
    assert_eq!(
        deferred.verdict,
        Some(Verdict::Deferred {
            retry_after_secs: None
        })
    );
    assert_eq!(deferred.reason.as_deref(), Some("rights_change"));
    let digest = update_rights::Digest::of(carried.as_bytes()).to_string();
    assert_eq!(deferred.digest.as_deref(), Some(digest.as_str()));
    assert!(
        deferred.widened.iter().any(|field| field == widened),
        "{deferred:?}"
    );
    assert!(world.launched().is_empty(), "nothing ran");
    assert_eq!(fs::read(&file).ok(), before, "nothing was installed");
    assert_eq!(
        verdict(&root.update("svc", version)),
        deferred.verdict.clone().unwrap(),
        "asked again, still deferred"
    );

    let env = update_rights::cli::Env {
        approvals: world.path("updater.d/approved"),
        trusted_uid: my_uid(),
        effective_uid: my_uid(),
    };
    let args: Vec<String> = ["kbf-updater", "approve", "svc", &digest]
        .map(str::to_owned)
        .to_vec();
    let (code, said) = kbf_updater::run_approve_with(&args, &env);
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("approved svc"), "{said}");

    let ran = root.update("svc", version);
    assert_eq!(verdict(&ran), SUCCEEDED, "{ran:?}");
    assert_eq!(
        world.steps("svc"),
        ["prepare", "cutover", "verify", "cleanup"]
    );
    assert_eq!(fs::read(&file).unwrap(), carried.as_bytes());
}

/// Catches a declaration installed by an update that then fails staying installed: the update
/// rolls back, and the declaration that was active before it is put back.
#[test]
fn a_declaration_installed_by_a_failed_update_is_put_back() {
    let world = World::new("restore");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    let narrower = active.replace("prepare = 600", "prepare = 500");
    world.stage("svc", "2.0.0", Some(&narrower));
    world.tell("svc", "verify", "1");
    let root = world.root_part();
    let failed = root.update("svc", "2.0.0");
    assert!(
        matches!(verdict(&failed), Verdict::Failed { .. }),
        "{failed:?}"
    );
    assert_eq!(
        world.steps("svc"),
        ["prepare", "cutover", "verify", "rollback"]
    );
    let prepare = world.launched()[0].clone();
    assert_eq!(
        property(&prepare, "RuntimeMaxSec").as_deref(),
        Some("500"),
        "the narrower declaration ran the update"
    );
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        active,
        "the active declaration is back"
    );
    assert!(!world.path("state/svc/declaration.previous").exists());
}

/// Catches one bad declaration taking the others down: a declaration that fails a rule of the
/// load (a file others may write) refuses only its service, which is `unmanaged`, and the other
/// declared service still updates.
#[test]
fn a_declaration_that_fails_a_load_rule_refuses_only_its_service() {
    let world = World::new("refuse-one");
    world.declare("svc", &world.tierstore_shaped());
    let bad = world
        .declaration(
            "other",
            "",
            "user = \"svc-update\"\ngroup = \"svc-update\"\n",
        )
        .replace("uid = \"svc\"", "uid = \"other\"");
    root_support::put(&world.path("updater.d/other.toml"), bad.as_bytes(), 0o666);
    world.stage("svc", "1.0.0", None);
    world.stage("other", "1.0.0", None);
    let root = world.root_part();
    assert_eq!(root.services(), ["svc"]);
    let refused = root.refused();
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].service, "other");

    let unmanaged = root.update("other", "1.0.0");
    assert_eq!(unmanaged.token(), Some("unmanaged"), "{unmanaged:?}");
    assert_eq!(root.status("other").token(), Some("unmanaged"));
    assert_eq!(verdict(&root.update("svc", "1.0.0")), SUCCEEDED);
    assert!(world.steps("other").is_empty());
}

/// Catches a declaration a release carries being installed when it cannot be: one that is not
/// UTF-8, not a declaration, or names another service, and one the approval record cannot be
/// trusted for, are refused and leave the active declaration in place; a release that carries the
/// active declaration itself changes nothing and runs.
#[test]
fn a_carried_declaration_that_cannot_be_installed_is_refused() {
    let world = World::new("carried-bad");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    let other = world.declaration(
        "other",
        "",
        "user = \"svc-update\"\ngroup = \"svc-update\"\n",
    );
    for (version, bytes) in [
        ("2.0.0", vec![0xff_u8]),
        ("2.0.1", b"not a declaration".to_vec()),
        ("2.0.2", other.into_bytes()),
    ] {
        world.stage_bytes("svc", version, Some(&bytes));
        let refused = world.root_part().update("svc", version);
        assert_eq!(
            refused.token(),
            Some("declaration_refused"),
            "{version}: {refused:?}"
        );
    }
    let wider = active.replace("[\"CAP_NET_ADMIN\"]", "[\"CAP_NET_ADMIN\", \"CAP_KILL\"]");
    assert_ne!(wider, active);
    world.stage("svc", "2.0.3", Some(&wider));
    let approvals = world.path("updater.d/approved");
    root_support::put(&approvals, b"", 0o666);
    let untrusted = world.root_part().update("svc", "2.0.3");
    assert_eq!(
        untrusted.token(),
        Some("declaration_refused"),
        "{untrusted:?}"
    );
    fs::remove_file(&approvals).unwrap();
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        active
    );
    assert!(world.launched().is_empty(), "nothing ran");

    world.stage("svc", "3.0.0", Some(&active));
    assert_eq!(
        verdict(&world.root_part().update("svc", "3.0.0")),
        SUCCEEDED
    );
    assert!(!world.path("state/svc/declaration.previous").exists());
}

/// Catches the first declaration of a service staying installed after its update failed: it is
/// removed, and the service is no longer declared.
#[test]
fn a_first_declaration_installed_by_a_failed_update_is_removed() {
    let world = World::new("first-fails");
    let first = world.tierstore_shaped();
    world.stage("svc", "1.0.0", Some(&first));
    world.approve("svc", first.as_bytes());
    world.tell("svc", "verify", "1");
    let root = world.root_part();
    let failed = root.update("svc", "1.0.0");
    assert!(
        matches!(verdict(&failed), Verdict::Failed { .. }),
        "{failed:?}"
    );
    assert!(!world.path("updater.d/svc.toml").exists());
    assert!(!world.path("state/svc/declaration.first").exists());
    assert!(root.services().is_empty());
}

/// Catches an installed declaration that the load refuses staying installed: an approved
/// declaration whose service account does not exist is refused as it is installed, the active one
/// is put back, and nothing runs.
#[test]
fn an_installed_declaration_the_load_refuses_is_put_back() {
    let world = World::new("ghost");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    let ghost = active.replace("uid = \"svc\"", "uid = \"ghost\"");
    world.stage("svc", "2.0.0", Some(&ghost));
    world.approve("svc", ghost.as_bytes());
    let root = world.root_part();
    let refused = root.update("svc", "2.0.0");
    assert_eq!(refused.token(), Some("declaration_refused"), "{refused:?}");
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        active
    );
    assert_eq!(root.services(), ["svc"]);
    assert!(world.launched().is_empty());
}

/// Catches an installed declaration left in place when the runner refuses the order: while an
/// update of the service is unfinished, an order whose release carries a narrower declaration is
/// refused as busy, and the active declaration is the one that was.
#[test]
fn a_declaration_installed_for_an_order_the_runner_refuses_is_put_back() {
    let world = World::new("busy-carried");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    world.stage("svc", "2.0.0", None);
    let narrower = active.replace("prepare = 600", "prepare = 500");
    world.stage("svc", "3.0.0", Some(&narrower));
    let killed = world
        .root_part_gated(Arc::new(|point: CrashPoint| {
            point == CrashPoint::AfterWrite(2)
        }))
        .update("svc", "2.0.0");
    assert_eq!(killed.token(), Some("runner"), "{killed:?}");
    let busy = world.root_part().update("svc", "3.0.0");
    assert_eq!(busy.token(), Some("busy"), "{busy:?}");
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        active
    );
    assert!(!world.path("state/svc/declaration.previous").exists());
}

/// Catches a root part killed between an update's verdict and what the verdict leaves: the next
/// start points `current` at the release that succeeded and drops the declaration kept for a
/// restore.
#[test]
fn a_start_does_what_a_verdict_left_undone() {
    let world = World::new("after-verdict");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    let narrower = active.replace("prepare = 600", "prepare = 500");
    world.stage("svc", "2.0.0", Some(&narrower));
    // The writes of the step record of one update, counted on a world like this one.
    let probe = World::new("after-verdict-probe");
    probe.declare("svc", &probe.tierstore_shaped());
    probe.stage("svc", "2.0.0", None);
    let most = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&most);
    let counting = probe.root_part_gated(Arc::new(move |point: CrashPoint| {
        if let CrashPoint::AfterWrite(n) = point {
            seen.fetch_max(n, Ordering::SeqCst);
        }
        false
    }));
    assert_eq!(verdict(&counting.update("svc", "2.0.0")), SUCCEEDED);
    let writes = most.load(Ordering::SeqCst);
    // Killed once the verdict is durable, before what it leaves is done.
    let killed = world
        .root_part_gated(Arc::new(move |point: CrashPoint| {
            point == CrashPoint::AfterWrite(writes)
        }))
        .update("svc", "2.0.0");
    assert_eq!(killed.token(), Some("runner"), "{killed:?}");
    assert_eq!(world.record("svc")["state"]["state"], "finished");
    assert!(world.path("state/svc/declaration.previous").exists());
    assert!(fs::read_link(world.path("releases/svc/current")).is_err());
    let started = world.root_part().start("svc", false);
    assert!(started.ok, "{started:?}");
    assert_eq!(
        fs::read_link(world.path("releases/svc/current")).unwrap(),
        std::path::Path::new("2.0.0")
    );
    assert!(!world.path("state/svc/declaration.previous").exists());
}

/// Catches a declaration put back under an update the runner has begun: a root part killed after
/// the first write of the step record leaves the declaration the release carried installed, and
/// the next start runs that update to its verdict under it and keeps it.
#[test]
fn a_killed_update_keeps_the_declaration_it_installed_until_its_verdict() {
    let world = World::new("killed-carried");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    let narrower = active.replace("prepare = 600", "prepare = 500");
    world.stage("svc", "2.0.0", Some(&narrower));
    let killed = world
        .root_part_gated(Arc::new(|point: CrashPoint| {
            point == CrashPoint::AfterWrite(1)
        }))
        .update("svc", "2.0.0");
    assert_eq!(killed.token(), Some("runner"), "{killed:?}");
    assert_ne!(world.record("svc")["state"]["state"], "finished");
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        narrower
    );
    let started = world.root_part().start("svc", false);
    assert!(started.ok, "{started:?}");
    assert_eq!(verdict(&started), SUCCEEDED);
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        narrower
    );
    assert!(!world.path("state/svc/declaration.previous").exists());
}

/// A clock whose dead-man polls are counted: a sleep of the dead-man's interval is a poll, and
/// waits a little on the real clock so the dead-man's loop does not spin; any other sleep moves
/// the fake clock.
struct PollClock {
    fake: Arc<FakeClock>,
    poll_secs: u64,
    polls: AtomicU64,
}

impl Clock for PollClock {
    fn mono_secs(&self) -> u64 {
        self.fake.mono_secs()
    }

    fn boot_id(&self) -> String {
        self.fake.boot_id()
    }

    fn sleep_secs(&self, secs: u64) {
        if secs == self.poll_secs {
            self.polls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(10));
        } else {
            self.fake.sleep_secs(secs);
        }
    }
}

/// Catches a service run with no dead-man: the root part gives each service's runner a dead-man
/// thread that polls at the interval it was given, and an update runs to its verdict beside it.
#[test]
fn each_service_gets_a_dead_man_that_polls_at_its_interval() {
    let world = World::new("deadman");
    world.declare("svc", &world.tierstore_shaped());
    world.stage("svc", "1.0.0", None);
    let clock = Arc::new(PollClock {
        fake: FakeClock::new(),
        poll_secs: 3600,
        polls: AtomicU64::new(0),
    });
    let mut parts = world.parts(None);
    parts.clock = clock.clone();
    parts.deadman_poll_secs = Some(3600);
    let root = RootPart::new(world.config(), parts).unwrap();
    assert_eq!(verdict(&root.update("svc", "1.0.0")), SUCCEEDED);
    // Waits for the dead-man's first poll; a bound on the number of looks, not on time.
    let polled = (0..3000).any(|_| {
        let seen = clock.polls.load(Ordering::SeqCst) > 0;
        if !seen {
            std::thread::sleep(Duration::from_millis(10));
        }
        seen
    });
    assert!(polled, "no dead-man polled for svc");
}

/// Catches a boot entry run with a record directory the service's account can write: the check
/// every step gets before its launch is made before the boot entry too, and nothing is launched.
#[test]
fn a_boot_entry_whose_record_directory_the_service_can_write_runs_nothing() {
    let world = World::new("boot-state");
    world.declare("svc", &world.tierstore_shaped());
    world.stage("svc", "1.0.0", None);
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    let launched = world.launched().len();
    // Opened to the service's account after the update, before the next start.
    root_support::dir(&world.hook_dir("svc"), 0o777);
    let started = world.root_part().start("svc", false);
    assert_eq!(started.token(), Some("boot"), "{started:?}");
    assert!(
        started.error.as_deref().unwrap_or_default().contains("can be written"),
        "{started:?}"
    );
    assert_eq!(world.launched().len(), launched);
}

/// `kbf-updater root` as a host starts it (`run_config`), over `world`, with the agent's socket in
/// it: `at_ready` runs when the root part is about to answer the agent, then the agent asks for
/// the status of `svc` and the root part is stopped. The answer.
fn serve_once(world: &World, at_ready: impl FnOnce()) -> Answer {
    let socket = world.path("root.sock");
    let flags = kbf_updater::root::flags::parse(&[
        "--socket".to_owned(),
        socket.display().to_string(),
        "--socket-user".to_owned(),
        "me".to_owned(),
    ])
    .unwrap();
    let (sent, answered) = std::sync::mpsc::channel();
    let (code, text) =
        kbf_updater::root::run_config(&flags, world.config(), world.parts(None), move |stop| {
            at_ready();
            std::thread::spawn(move || {
                let answer = socket::request(
                    &socket,
                    &Request::Status {
                        service: "svc".to_owned(),
                    },
                );
                stop.stop();
                sent.send(answer.map_err(|e| e.to_string())).unwrap();
            });
        });
    assert_eq!((code, text.as_str()), (0, ""));
    answered.recv().unwrap().unwrap()
}

/// The boot id of the last start pass, as the root part wrote it.
fn committed_boot_id(world: &World) -> String {
    fs::read_to_string(world.path("state").join(boot::BOOT_ID_FILE)).unwrap_or_default()
}

/// Catches a root part that answers the agent before its start pass, or that does not read the
/// boot the way a host starts it: `kbf-updater root` started after a boot finds the update to
/// 2.0.0 cut off before its `cutover`, rolls it back and runs the boot entry from 1.0.0, all
/// before it answers the agent, and only then writes the new boot id.
#[test]
fn the_root_part_rolls_back_a_cutover_a_boot_cut_off_before_it_answers_the_agent() {
    let world = pending_cutover_world("serve-boot");
    boot::commit_boot(&world.path("state"), &*world.clock).unwrap();
    let before = world.steps("svc").len();
    world.clock.reboot("boot-b");
    let mut at_ready = None;
    let answer = serve_once(&world, || {
        at_ready = Some((
            world.steps("svc")[before..].to_vec(),
            world.record("svc")["state"].clone(),
        ));
    });
    let (steps, state) = at_ready.unwrap();
    assert_eq!(steps, ["rollback", "restore"]);
    assert_eq!(state["state"], "finished", "{state}");
    assert_eq!(
        serde_json::from_value::<Verdict>(state["verdict"].clone()).unwrap(),
        Verdict::Failed {
            phase: Phase::BeforeCommit,
            class: Class::Release
        },
        "{state}"
    );
    assert!(answer.ok, "{answer:?}");
    assert_eq!(committed_boot_id(&world), "boot-b");
}

/// Catches a boot rule kept only for the services whose declaration loaded: after a boot the
/// root part reads the record of a service whose declaration is refused as well, so the
/// `cutover` the boot cut off is rolled back once the declaration is put right, in the same boot,
/// and not run forward.
#[test]
fn a_cutover_a_boot_cut_off_is_rolled_back_even_when_its_declaration_was_refused_at_boot() {
    let world = pending_cutover_world("boot-refused");
    boot::commit_boot(&world.path("state"), &*world.clock).unwrap();
    let file = world.path("updater.d/svc.toml");
    let mode = |m: u32| fs::set_permissions(&file, fs::Permissions::from_mode(m)).unwrap();
    // Open to everyone: the load refuses it.
    mode(0o666);
    world.clock.reboot("boot-b");
    let before = world.steps("svc").len();
    serve_once(&world, || {});
    assert_eq!(world.steps("svc").len(), before, "a refused service runs nothing");
    assert_eq!(world.record("svc")["state"]["state"], "done");
    assert_eq!(committed_boot_id(&world), "boot-b");
    // Put right in the same boot; the root part restarted.
    mode(0o644);
    let started = world.root_part().start("svc", false);
    assert_eq!(
        started.verdict,
        Some(Verdict::Failed {
            phase: Phase::BeforeCommit,
            class: Class::Release
        }),
        "{started:?}"
    );
    assert_eq!(&world.steps("svc")[before..], ["rollback", "restore"]);
}
