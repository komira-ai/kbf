//! `activate` on a Mac: the kit under a scratch directory, launchd played by the test, the
//! channel's release files gone.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use kbf_retain::{CommitId, Retained};

use super::{Shared, current, previous, rig};
use crate::local::activate_cli::{self, Command};
use crate::local::answer::{Answer, Code};
use crate::local::backend::Backend;
use crate::local::core::Ab;
use crate::local::core::activate::{How, LOCK_TAKES};
use crate::local::mac_backend::MacBackend;
use crate::local::request::Target;
use crate::mac::layout::APPLY_LABEL;
use crate::mac::status::{self, ApplyState};
use crate::mac::tests::fx::{Fx, NEW, NEW_VERSION, OLD, OLD_VERSION, Sim, binary, mkdir, write};
use crate::mac::{retain, tools};

/// A process id no process has.
const GONE: u32 = i32::MAX as u32;
const NOW: u64 = 2_000;

/// Lay `commit` out in the kit as `version`, newer than the release that runs; no link moves.
fn keep(fx: &Fx, commit: &str, version: &str, at: u64) {
    let dir = fx.host.release(commit);
    mkdir(&dir, 0o755);
    write(&dir.join("kbf"), 0o755, &binary(commit));
    write(
        &dir.join("VERSION"),
        0o644,
        format!("{version}\n").as_bytes(),
    );
    for (c, t) in [(OLD, 1), (commit, at)] {
        fs::File::open(fx.host.release(c))
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t))
            .unwrap();
    }
}

/// The channel's release files and the allowlist are gone: nothing could be fetched.
fn cut_off(fx: &Fx) {
    fs::remove_dir_all(&fx.channel_dir).unwrap();
    fs::remove_file(&fx.host.allowlist).unwrap();
}

fn core(fx: &Fx, sim: &Rc<Sim>) -> Ab<MacBackend> {
    let agent = fx.dir.join("agent");
    mkdir(&agent, 0o700);
    Ab::new(
        MacBackend::new(fx.host.clone(), Box::new(Shared(Rc::clone(sim)))),
        fx.host.work.join("state.json"),
        fx.host.trust.clone(),
        600,
        Box::new(|| NOW),
    )
    .holding(agent.join("pin.json"))
}

fn target(text: &str) -> Target {
    Target::parse(text).unwrap()
}

fn activated(commit: &str, version: &str, from: &str, pinned: bool, taken: bool) -> Answer {
    Answer::Activated {
        commit: commit.to_owned(),
        version: Some(version.to_owned()),
        from: Some(from.to_owned()),
        pinned,
        lock_taken: taken,
    }
}

/// The programs the swap ran, by name.
fn programs(sim: &Sim) -> Vec<String> {
    sim.calls
        .borrow()
        .iter()
        .map(|c| c.program.clone())
        .collect()
}

/// With the channel gone, `activate` by version moves `current` to the kept release and
/// `previous` to the one that ran, and restarts the daemon's job on it (out, in, welcomed,
/// running it); only launchd and `lsof` are asked, nothing is listed or fetched.
///
/// Catches: a Mac activate that fetches, or that leaves `previous` as it was.
#[test]
fn mac_activate_swaps_to_a_kept_release_with_the_release_source_unreachable() {
    let (fx, sim, _) = rig("mba-swap");
    keep(&fx, NEW, NEW_VERSION, 2);
    cut_off(&fx);
    let ab = core(&fx, &sim);
    assert_eq!(
        ab.activate(&target(NEW_VERSION), How::default()),
        activated(NEW, NEW_VERSION, OLD, false, false)
    );
    assert_eq!(
        (current(&fx).as_deref(), previous(&fx).as_deref()),
        (Some(NEW), Some(OLD))
    );
    assert_eq!(
        (sim.daemon_bootouts.get(), sim.daemon_bootstraps.get()),
        (1, 1)
    );
    assert!(
        programs(&sim)
            .iter()
            .all(|p| p == tools::LAUNCHCTL || p == tools::LSOF),
        "{:?}",
        programs(&sim)
    );
}

/// A release the kit does not keep is refused with the kept ones, newest first; nothing moves
/// and the daemon is not touched.
///
/// Catches: a Mac refusal without the listing.
#[test]
fn mac_activate_refuses_a_release_not_kept_and_lists_the_kept_ones() {
    let (fx, sim, _) = rig("mba-unkept");
    keep(&fx, NEW, NEW_VERSION, 2);
    let ab = core(&fx, &sim);
    let answer = ab.activate(&target("0.2.0+gcccccccccccc-1"), How::default());
    assert_eq!(
        answer.json_line(),
        format!(
            "{{\"result\":\"refused\",\"reason\":\"not_kept\",\"releases\":[\
             {{\"commit\":\"{NEW}\",\"version\":\"{NEW_VERSION}\"}},\
             {{\"commit\":\"{OLD}\",\"version\":\"{OLD_VERSION}\",\"why\":[\"current\"]}}]}}"
        )
    );
    assert_eq!(current(&fx).as_deref(), Some(OLD));
    assert!(sim.calls.borrow().is_empty());
}

/// The swap executes nothing of the release it leaves: launchd and `lsof` are the only
/// programs, and none is a file under `releases/<OLD>`, nor names one; the daemon's own check
/// (which runs a release) is not run.
///
/// Catches: a swap that runs the release it leaves.
#[test]
fn mac_activate_executes_nothing_under_the_release_it_leaves() {
    let (fx, sim, _) = rig("mba-noexec");
    keep(&fx, NEW, NEW_VERSION, 2);
    cut_off(&fx);
    let ab = core(&fx, &sim);
    assert!(matches!(
        ab.activate(&target(NEW), How::default()),
        Answer::Activated { .. }
    ));
    let left = fx.host.release(OLD).display().to_string();
    for call in sim.calls.borrow().iter() {
        assert!(
            call.program == tools::LAUNCHCTL || call.program == tools::LSOF,
            "{call:?}"
        );
        assert!(
            !call.program.contains(&left) && !call.args.iter().any(|a| a.contains(OLD)),
            "{call:?} names the release left"
        );
    }
}

/// The lock file at `path` held by an open file of the test's, whose content is `pid`.
fn hold_as(path: &std::path::Path, pid: &str) -> fs::File {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.try_lock().unwrap();
    file.write_all(pid.as_bytes()).unwrap();
    file
}

/// A live holder of the install lock is refused and named; the ssh path's apply job, which runs
/// without the lock, is refused too while it runs. Nothing moves either time, and nothing is
/// pinned.
///
/// Catches: a Mac activate that runs beside an install or an apply.
#[test]
fn mac_activate_refuses_a_live_lock_holder_and_a_running_apply() {
    let (fx, sim, _) = rig("mba-busy");
    keep(&fx, NEW, NEW_VERSION, 2);
    let ab = core(&fx, &sim);
    let held = crate::host::take_lock(&fx.host.lock_file()).unwrap();
    let answer = ab.activate(&target(NEW), How::default());
    assert_eq!(
        answer.log_detail().unwrap(),
        format!(
            "the install lock {} is held by pid {}, which is alive: let it end or stop it, or \
             take it from a stuck holder with --force-lock",
            fx.host.lock_file().display(),
            std::process::id()
        )
    );
    drop(held);
    crate::locks::wait_lockable(&fx.host.lock_file());

    status::write(&fx.host, &ApplyState::Running).unwrap();
    sim.loaded.borrow_mut().insert(APPLY_LABEL.to_owned());
    sim.apply_running.set(true);
    let pin = How {
        pin: true,
        force_lock: false,
    };
    let answer = ab.activate(&target(NEW), pin);
    assert_eq!(
        answer,
        Answer::Refused(
            Code::Busy,
            format!("the ssh path's apply job ({APPLY_LABEL}) is running")
        )
    );
    assert!(!fx.dir.join("agent/pin.json").exists(), "nothing is pinned");
    assert_eq!(current(&fx).as_deref(), Some(OLD));
    assert_eq!(sim.daemon_bootouts.get(), 0);
}

/// `--force-lock` takes a dead holder's lock, records the take beside the state in the work
/// directory, and swaps.
///
/// Catches: a Mac take of the lock that is not recorded.
#[test]
fn mac_force_lock_takes_a_dead_holders_lock_and_records_the_take() {
    let (fx, sim, _) = rig("mba-force");
    keep(&fx, NEW, NEW_VERSION, 2);
    let ab = core(&fx, &sim);
    let _dead = hold_as(&fx.host.lock_file(), &format!("{GONE}\n"));
    let force = How {
        pin: false,
        force_lock: true,
    };
    assert_eq!(
        ab.activate(&target(NEW_VERSION), force),
        activated(NEW, NEW_VERSION, OLD, false, true)
    );
    let record = fx.host.work.join(LOCK_TAKES);
    assert_eq!(
        fs::read_to_string(&record).unwrap(),
        format!(
            "{{\"at_unix\":{NOW},\"by\":\"activate\",\"target\":\"{NEW_VERSION}\",\
             \"holder_pid\":{GONE},\"holder_alive\":false}}\n"
        )
    );
    assert_eq!(fs::metadata(&record).unwrap().mode() & 0o7777, 0o600);
}

/// `--pin` writes the hold file and pins the release in the work directory's `retain.json`, so
/// a prune keeps it.
///
/// Catches: a Mac pin that writes no hold file, or that a prune ignores.
#[test]
fn mac_pin_writes_the_hold_file_and_pins_the_release() {
    let (fx, sim, _) = rig("mba-pin");
    keep(&fx, NEW, NEW_VERSION, 2);
    let ab = core(&fx, &sim);
    let pin = How {
        pin: true,
        force_lock: false,
    };
    assert_eq!(
        ab.activate(&target(NEW), pin),
        activated(NEW, NEW_VERSION, OLD, true, false)
    );
    let hold = fx.dir.join("agent/pin.json");
    assert_eq!(
        fs::read_to_string(&hold).unwrap(),
        format!(
            "{{\"commit\":\"{NEW}\",\"version\":\"{NEW_VERSION}\",\"at_unix\":{NOW},\
             \"by\":\"activate\"}}"
        )
    );
    assert_eq!(fs::metadata(&hold).unwrap().mode() & 0o7777, 0o644);
    let pinned = Retained::read(&retain::file(&fx.host)).unwrap().pinned;
    assert_eq!(
        pinned.into_iter().collect::<Vec<_>>(),
        [CommitId::new(NEW).unwrap()]
    );
}

/// An activate during the socket's trial clears the marker, so no deadline puts the old
/// release back; a daemon that does not come up on the release named leaves the kit on it
/// (`restart_failed`, the release left not started); links that cannot be moved stay as they
/// were and keep a trial's marker (`switch_failed`); a marker that cannot be cleared puts the
/// links back and the daemon is not touched.
///
/// Catches: a marker that outlives the activate; a failed restart answered by going back.
#[test]
fn mac_activate_ends_a_trial_and_a_failed_restart_does_not_go_back() {
    let (fx, sim, _) = rig("mba-trial");
    keep(&fx, NEW, NEW_VERSION, 2);
    let ab = core(&fx, &sim);
    let backend = MacBackend::new(fx.host.clone(), Box::new(Shared(Rc::clone(&sim))));
    backend.switch(NEW, 600, 1).unwrap();
    assert!(backend.pending().unwrap().is_some());
    assert!(matches!(
        ab.activate(&target(OLD), How::default()),
        Answer::Activated { .. }
    ));
    assert!(backend.pending().unwrap().is_none(), "the trial is over");
    assert_eq!(
        (current(&fx).as_deref(), previous(&fx).as_deref()),
        (Some(OLD), Some(NEW))
    );

    let (fx, sim, _) = rig("mba-noup");
    keep(&fx, NEW, NEW_VERSION, 2);
    sim.welcome.set(false);
    let ab = core(&fx, &sim);
    let answer = ab.activate(&target(NEW), How::default());
    assert_eq!(
        answer.json_line(),
        "{\"result\":\"failed\",\"reason\":\"restart_failed\"}"
    );
    assert_eq!(
        answer.log_detail().unwrap(),
        "the kit runs bbbbbbbbbbbb now, but: not_welcomed"
    );
    assert_eq!(current(&fx).as_deref(), Some(NEW));
    assert_eq!(
        sim.daemon_bootstraps.get(),
        1,
        "the release left is not started"
    );

    // A trial's marker stays when the links cannot move: the trial is not lost.
    let (fx, sim, _) = rig("mba-links");
    keep(&fx, NEW, NEW_VERSION, 2);
    let ab = core(&fx, &sim);
    let marker = format!(
        "{{\"commit\":\"{OLD}\",\"from\":\"{NEW}\",\"from_previous\":null,\"deadline_unix\":5}}"
    );
    write(&fx.host.pending_marker(), 0o600, marker.as_bytes());
    fs::set_permissions(&fx.host.kit, fs::Permissions::from_mode(0o555)).unwrap();
    let answer = ab.activate(&target(NEW), How::default());
    fs::set_permissions(&fx.host.kit, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        answer,
        Answer::Failed(Code::SwitchFailed, "links_failed".to_owned())
    );
    assert_eq!(current(&fx).as_deref(), Some(OLD));
    assert_eq!(
        fs::read_to_string(fx.host.pending_marker()).unwrap(),
        marker
    );

    let (fx, sim, _) = rig("mba-marker");
    keep(&fx, NEW, NEW_VERSION, 2);
    let ab = core(&fx, &sim);
    mkdir(&fx.host.pending_marker(), 0o755);
    write(&fx.host.pending_marker().join("x"), 0o644, b"in the way");
    let answer = ab.activate(&target(NEW), How::default());
    assert!(
        matches!(&answer, Answer::Failed(Code::StateUnavailable, _)),
        "{answer:?}"
    );
    assert_eq!(current(&fx).as_deref(), Some(OLD));
    assert_eq!(sim.daemon_bootouts.get(), 0, "the daemon is not touched");
}

/// A trial's marker that cannot be removed once the links moved (the work directory cannot be
/// written) puts the links back: the release on trial stays, its marker with it, and the daemon is
/// not touched.
///
/// Catches: links left on a release whose marker's deadline would then put the old one back.
#[test]
fn mac_activate_puts_the_links_back_when_the_marker_cannot_be_cleared() {
    let (fx, sim, _) = rig("mba-stuckmarker");
    keep(&fx, NEW, NEW_VERSION, 2);
    let ab = core(&fx, &sim);
    let marker = format!(
        "{{\"commit\":\"{OLD}\",\"from\":\"{NEW}\",\"from_previous\":null,\"deadline_unix\":5}}"
    );
    write(&fx.host.pending_marker(), 0o600, marker.as_bytes());
    drop(crate::host::take_lock(&fx.host.lock_file()).unwrap());
    crate::locks::wait_lockable(&fx.host.lock_file());
    fs::set_permissions(&fx.host.work, fs::Permissions::from_mode(0o500)).unwrap();
    let answer = ab.activate(&target(NEW), How::default());
    fs::set_permissions(&fx.host.work, fs::Permissions::from_mode(0o711)).unwrap();
    assert!(
        matches!(&answer, Answer::Failed(Code::StateUnavailable, d) if d.starts_with("pending.json: ")),
        "{answer:?}"
    );
    assert_eq!(current(&fx).as_deref(), Some(OLD));
    assert_eq!(previous(&fx), None);
    assert_eq!(
        fs::read_to_string(fx.host.pending_marker()).unwrap(),
        marker
    );
    assert_eq!(sim.daemon_bootouts.get(), 0);
}

/// Links that do not name releases cannot be swapped from: the activate fails with
/// `state_unavailable` and nothing moves.
///
/// Catches: an activate that guesses the release that runs.
#[test]
fn mac_activate_needs_links_it_can_read() {
    let (fx, sim, _) = rig("mba-badlink");
    keep(&fx, NEW, NEW_VERSION, 2);
    let backend = MacBackend::new(fx.host.clone(), Box::new(Shared(Rc::clone(&sim))));
    std::os::unix::fs::symlink("/somewhere/else", fx.host.previous()).unwrap();
    let fault = backend.activate(NEW).unwrap_err();
    assert_eq!(fault.code, Code::StateUnavailable, "{fault:?}");
    assert_eq!(
        fs::read_link(fx.host.current()).unwrap(),
        std::path::Path::new(&format!("releases/{OLD}")),
        "nothing moved"
    );
}

/// The command as root runs it on a Mac: `--restart-unit` is refused, a work directory that
/// cannot be used fails, and otherwise the swap is done and audited.
///
/// Catches: a Mac command that is not audited.
#[test]
fn the_command_audits_what_it_did_on_a_mac() {
    let (fx, sim, _) = rig("mba-cli");
    keep(&fx, NEW, NEW_VERSION, 2);
    let hold = fx.dir.join("agent/pin.json");
    mkdir(&fx.dir.join("agent"), 0o700);
    let command = |line: &str| {
        let args: Vec<String> = line.split(' ').map(str::to_owned).collect();
        Command::parse(&args).unwrap()
    };
    let run = |line: &str| {
        activate_cli::on_mac(
            &command(line),
            fx.host.clone(),
            Box::new(Shared(Rc::clone(&sim))),
            hold.clone(),
        )
    };
    assert_eq!(run("kbf=bbbbbbbbbbbb --restart-unit kbf-daemon.service"), 2);
    assert_eq!(run("kbf=bbbbbbbbbbbb --pin"), 0);
    assert_eq!(current(&fx).as_deref(), Some(NEW));
    assert!(hold.exists());
    let audit = fs::read_to_string(&fx.host.audit).unwrap();
    let line: serde_json::Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
    assert_eq!(line["verb"], "activate");
    assert_eq!(line["result"], format!("{NEW} from {OLD}; pinned"));

    let mut host = fx.host.clone();
    host.work = fx.dir.join("not-a-directory");
    write(&host.work, 0o644, b"a file");
    let code = activate_cli::on_mac(
        &command("kbf=bbbbbbbbbbbb"),
        host,
        Box::new(Shared(Rc::clone(&sim))),
        hold.clone(),
    );
    assert_eq!(code, 3);
}
