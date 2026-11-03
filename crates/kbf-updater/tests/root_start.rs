//! The start of the root part (`kbf-updater root`), with a world in a scratch directory and the
//! hook run directly as this user: the pass over the records before the agent is answered (the
//! boot rule of docs/design/host-updates.md 3.5), the boot entry, and the boot id.

mod root_support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use kbf_step_runner::CrashPoint;
use kbf_updater::root::boot;
use kbf_updater::root::socket::{self, Answer, Request};
use root_support::{SUCCEEDED, World, dir, pending_cutover_world, put, verdict};
use update_hook::{Class, Phase, Verdict};

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
    assert!(root.read_after_boot().is_empty());
    let started = root.start("svc");
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
    // In the same boot no record is read as cut off by a boot: the start resumes it as it is.
    let root = world.root_part();
    let started = root.start("svc");
    assert_eq!(started.verdict, Some(SUCCEEDED), "{started:?}");
    assert_eq!(
        &world.steps("svc")[before..],
        ["cutover", "verify", "cleanup", "restore"]
    );
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
    // Opened to the service's account once the declarations are loaded, before the boot entry: a
    // load after this refuses the declaration itself.
    let root = world.root_part();
    root_support::dir(&world.hook_dir("svc"), 0o777);
    let started = root.start("svc");
    assert_eq!(started.token(), Some("boot"), "{started:?}");
    assert!(
        started
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("can be written"),
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

/// Catches a boot id written over a record the start pass could not read, or a service started
/// from such a record: after a boot, a record that does not parse holds the boot id back and its
/// service is not started; once it is put right the next start reads it after the boot and
/// writes the boot id, and a start in the same boot after that leaves the boot id as it is.
#[test]
fn the_boot_id_is_written_only_once_every_record_was_read() {
    let world = World::new("boot-unread");
    world.declare("svc", &world.tierstore_shaped());
    let state = world.path("state");
    root_support::dir(&state, 0o700);
    boot::commit_boot(&state, &*world.clock).unwrap();
    root_support::dir(&state.join("svc"), 0o700);
    let record = state.join("svc/step-record.json");
    root_support::put(&record, b"{", 0o600);
    // Neither a directory nor a service's name: not a record of anyone.
    root_support::dir(&state.join(".scratch"), 0o700);
    world.clock.reboot("boot-b");
    serve_once(&world, || {});
    assert_eq!(committed_boot_id(&world), "boot-a");
    assert_eq!(
        fs::read(&record).unwrap(),
        b"{",
        "the record is left as it was"
    );
    fs::remove_file(&record).unwrap();
    serve_once(&world, || {});
    assert_eq!(committed_boot_id(&world), "boot-b");
    // A start in the same boot reads nothing as cut off and leaves the boot id as it is.
    let after = serve_once(&world, || {});
    assert!(after.ok, "{after:?}");
    assert_eq!(committed_boot_id(&world), "boot-b");
}

/// Catches a state directory the root part cannot list read as one with no record in it: the
/// read after a boot reports it, so the boot id is not written.
#[test]
fn a_state_directory_that_cannot_be_listed_is_reported_by_the_read_after_a_boot() {
    let world = World::new("boot-unlisted");
    world.declare("svc", &world.tierstore_shaped());
    let root = world.root_part();
    let state = world.path("state");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    let unread = root.read_after_boot();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(unread.len(), 1, "{unread:?}");
    assert_eq!(unread[0].0, "", "{unread:?}");
    // Every service is unread until it is read: an order reads the record first (there is none),
    // then goes its way (here to a release that is not staged).
    assert_eq!(root.update("svc", "1.0.0").token(), Some("not_staged"));
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
    assert_eq!(
        world.steps("svc").len(),
        before,
        "a refused service runs nothing"
    );
    assert_eq!(world.record("svc")["state"]["state"], "done");
    assert_eq!(committed_boot_id(&world), "boot-b");
    // Put right in the same boot; the root part restarted.
    mode(0o644);
    let started = world.root_part().start("svc");
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

/// Catches a resumed update run from a release that changed after it was verified, or one left
/// pending for ever because of it: at the next start the update to 2.0.0, cut off before its
/// `cutover`, is found with its release changed; it is halted with `artifact_changed`, as the step
/// runner halts one, nothing of 2.0.0 runs, and the boot entry runs from 1.0.0.
#[test]
fn an_unfinished_update_whose_release_changed_is_halted_at_the_next_start() {
    let world = pending_cutover_world("resume-changed");
    let before = world.steps("svc").len();
    root_support::put(&world.path("releases/svc/2.0.0/extra"), b"x", 0o644);
    let started = world.root_part().start("svc");
    assert_eq!(started.verdict, Some(Verdict::Halt), "{started:?}");
    assert_eq!(started.reason.as_deref(), Some("artifact_changed"));
    assert_eq!(world.record("svc")["state"]["state"], "finished");
    assert_eq!(&world.steps("svc")[before..], ["restore"]);
}

/// The state an install leaves when the root part stops right after it: the declaration
/// `installed` in place, and kept for a restore what was before it (`kept`: the active file, or
/// `None` for a first install) with the generation the record had then (`began`).
fn installed_then_stopped(world: &World, installed: &str, kept: Option<&str>, began: &str) {
    let state = world.path("state/svc");
    dir(&world.path("state"), 0o700);
    dir(&state, 0o700);
    match kept {
        Some(text) => put(&state.join("declaration.previous"), text.as_bytes(), 0o600),
        None => put(&state.join("declaration.first"), b"", 0o600),
    }
    put(&state.join("declaration.began"), began.as_bytes(), 0o600);
    world.declare("svc", installed);
}

/// Catches a declaration left installed by an update that never began: the root part stopped
/// between installing a narrower declaration and the first write of the new record (the record is
/// still the finished one of generation 1); the next start puts the one that was active back and
/// keeps nothing, and the update that succeeded stays as it was.
#[test]
fn a_declaration_installed_for_an_update_that_never_began_goes_back_at_the_start() {
    let world = World::new("began-never");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    world.stage("svc", "1.0.0", None);
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    let narrower = active.replace("prepare = 600", "prepare = 500");
    installed_then_stopped(&world, &narrower, Some(&active), "1");
    let started = world.root_part().start("svc");
    assert!(started.ok, "{started:?}");
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        active
    );
    for kept in ["declaration.previous", "declaration.began"] {
        assert!(!world.path("state/svc").join(kept).exists(), "{kept}");
    }
    assert_eq!(world.record("svc")["generation"], 1);
}

/// Catches a first declaration left installed by an update that never began: with no record and
/// the first declaration of the service installed, the start removes it, and the service is no
/// longer declared: nothing is launched.
#[test]
fn a_first_declaration_installed_for_an_update_that_never_began_is_removed_at_the_start() {
    let world = World::new("began-first");
    installed_then_stopped(&world, &world.tierstore_shaped(), None, "none");
    let started = world.root_part().start("svc");
    assert!(started.ok, "{started:?}");
    assert!(!world.path("updater.d/svc.toml").exists());
    assert!(world.launched().is_empty());
    assert_eq!(
        world.root_part().start("svc").token(),
        Some("unknown_service")
    );
}

/// Catches a kept declaration's marker that is not one read as some generation: the start says
/// so and changes nothing.
#[test]
fn a_marker_that_holds_no_generation_stops_the_start() {
    let world = World::new("began-bad");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    installed_then_stopped(&world, &active, Some(&active), "seven");
    assert_eq!(world.root_part().start("svc").token(), Some("internal"));
    assert!(world.path("state/svc/declaration.began").exists());
}

/// Catches a service let through after the read after a boot failed on its record: the record
/// of the update to 2.0.0, cut off before its `cutover`, cannot be read at the read after the
/// boot; an order is refused (`boot_unread`) while it cannot; once it is put right, the next order
/// to the same root part reads it as a boot cut it off first, and the `cutover` is rolled back,
/// not run.
#[test]
fn a_record_unread_after_a_boot_is_read_before_the_next_order() {
    let world = pending_cutover_world("boot-repaired");
    let record = world.path("state/svc/step-record.json");
    let bytes = fs::read(&record).unwrap();
    put(&record, b"{", 0o600);
    world.clock.reboot("boot-b");
    let root = world.root_part();
    let unread = root.read_after_boot();
    assert_eq!(unread.len(), 1, "{unread:?}");
    assert_eq!(unread[0].0, "svc");
    assert_eq!(root.update("svc", "2.0.0").token(), Some("boot_unread"));
    put(&record, &bytes, 0o600);
    let before = world.steps("svc").len();
    let answer = root.update("svc", "2.0.0");
    assert!(
        matches!(answer.verdict, Some(Verdict::Failed { .. })),
        "{answer:?}"
    );
    assert_eq!(&world.steps("svc")[before..], ["rollback"]);
}

/// Catches a changed release halted while its step may still run: the update to 2.0.0 is cut off
/// with its `cutover` running (in this boot), and 2.0.0 is changed; the start stops the step's
/// unit by name before it halts the update with `artifact_changed`.
#[test]
fn a_running_step_of_a_changed_release_is_stopped_before_the_halt() {
    let world = World::new("halt-running");
    world.declare("svc", &world.tierstore_shaped());
    world.stage("svc", "1.0.0", None);
    world.stage("svc", "2.0.0", None);
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    let record = world.path("state/svc/step-record.json");
    let gate = move |point: CrashPoint| {
        let running_cutover = fs::read(&record)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|rec| rec["step"] == "cutover" && rec["state"]["state"] == "running");
        matches!(point, CrashPoint::AfterWrite(_)) && running_cutover
    };
    let killed = world.root_part_gated(Arc::new(gate)).update("svc", "2.0.0");
    assert_eq!(killed.token(), Some("runner"), "{killed:?}");
    let unit = format!("updater-svc-cutover-{}", world.record("svc")["generation"]);
    put(&world.path("releases/svc/2.0.0/extra"), b"x", 0o644);
    let started = world.root_part().start("svc");
    assert_eq!(started.verdict, Some(Verdict::Halt), "{started:?}");
    assert!(world.systemctl.stopped().contains(&unit), "{unit}");
}

/// Catches a resume that runs from a previous release that changed, or that gives up for good on
/// one it cannot read for a moment: with only 1.0.0 (the previous release) changed, the update to
/// 2.0.0 is halted (`artifact_changed`); with 2.0.0 unreadable for now, the start is refused
/// (`artifact`) and the update stays unfinished, and once it can be read the update runs on.
#[test]
fn a_resume_halts_on_a_changed_previous_release_and_waits_on_an_unreadable_one() {
    let world = pending_cutover_world("resume-previous");
    put(&world.path("releases/svc/1.0.0/extra"), b"x", 0o644);
    world.root_part().start("svc");
    assert_eq!(world.record("svc")["state"]["state"], "finished");
    assert_eq!(world.record("svc")["state"]["reason"], "artifact_changed");

    let world = pending_cutover_world("resume-unreadable");
    let hook = world.path("releases/svc/2.0.0/hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o000)).unwrap();
    let refused = world.root_part().start("svc");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(refused.token(), Some("artifact"), "{refused:?}");
    assert_eq!(world.record("svc")["state"]["state"], "pending");
    let started = world.root_part().start("svc");
    assert_eq!(started.verdict, Some(SUCCEEDED), "{started:?}");
}

/// Catches a resume without the params it began with: an unfinished update whose params file is
/// gone is refused (`record`) and nothing runs.
#[test]
fn a_resume_whose_params_are_gone_is_refused() {
    let world = pending_cutover_world("resume-params");
    fs::remove_file(world.path("state/svc/params.toml")).unwrap();
    let steps = world.steps("svc");
    assert_eq!(world.root_part().start("svc").token(), Some("record"));
    assert_eq!(world.steps("svc"), steps);
}

/// Catches a boot entry whose outcome is lost, or one run with nothing to run from: a boot entry
/// that fails is reported `failure`; a declaration with a boot entry and no release installed by
/// an update launches nothing.
#[test]
fn the_boot_entry_reports_how_it_ended_and_needs_an_installed_release() {
    let world = World::new("boot-outcome");
    world.declare("svc", &world.tierstore_shaped());
    let none = world.root_part().start("svc");
    assert_eq!(none.boot, None, "{none:?}");
    assert!(world.launched().is_empty());
    world.stage("svc", "1.0.0", None);
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    world.tell("svc", "restore", "1 {}");
    let failed = world.root_part().start("svc");
    assert_eq!(failed.boot.as_deref(), Some("failure"), "{failed:?}");
}

/// Catches a declaration the load refuses left installed by an update that never began: the root
/// part stopped right after installing one that names an account the host does not have, so the
/// load refuses the service; started as a host starts it, the root part puts the one that was
/// active back from its own bookkeeping before it reads the declarations, and the service is
/// served again.
#[test]
fn a_refused_declaration_installed_for_an_update_that_never_began_goes_back_at_the_start() {
    let world = World::new("began-refused");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    world.stage("svc", "1.0.0", None);
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    let refused = active.replace("user = \"svc-update\"", "user = \"nosuchaccount\"");
    installed_then_stopped(&world, &refused, Some(&active), "1");
    assert!(
        world
            .root_part()
            .refused()
            .iter()
            .any(|r| r.service == "svc"),
        "the load refuses what was installed"
    );
    let answer = serve_once(&world, || {});
    assert!(answer.ok, "{answer:?}");
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        active
    );
    assert!(!world.path("state/svc/declaration.began").exists());
}
