//! The Mac backend over the kit under a scratch directory, with a fake launchd.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

use super::answer::{Answer, Code};
use super::backend::{Backend, Fault, Pending};
use super::core::Ab;
use super::mac_backend::MacBackend;
use super::request::LocalRequest;
use crate::mac::kit;
use crate::mac::release::{bundle_name, tarball_name};
use crate::mac::tests::fx::{Fx, NEW, NEW_VERSION, OLD, OLD_VERSION, Sim, mkdir, tarball, write};
use crate::request::{Channel, Version};
use crate::runner::{Cmd, Exec, Runner};

const OTHER_VERSION: &str = "0.1.9+gdddddddddddd-4";

/// A runner that is the scripted Mac, kept by the test.
struct Shared(Rc<Sim>);

impl Runner for Shared {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        self.0.run(cmd)
    }
}

fn rig(name: &str) -> (Fx, Rc<Sim>, MacBackend) {
    let fx = Fx::new(name);
    mkdir(&fx.host.work, 0o711);
    let sim = Rc::new(fx.sim());
    let backend = MacBackend::new(fx.host.clone(), Box::new(Shared(Rc::clone(&sim))));
    (fx, sim, backend)
}

fn version(text: &str) -> Version {
    Version::new(text).unwrap()
}

fn stable() -> Channel {
    Channel::new("stable").unwrap()
}

fn current(fx: &Fx) -> Option<String> {
    kit::links(&fx.host).unwrap().current
}

fn previous(fx: &Fx) -> Option<String> {
    kit::links(&fx.host).unwrap().previous
}

fn refused(code: Code) -> impl Fn(&Fault) -> bool {
    move |fault| fault.refused && fault.code == code
}

/// A release is checked end to end by the ssh path's code and laid out beside the one that
/// runs, root's and 0755 with its version beside it; no link moves, the daemon is not touched
/// and the stage is gone.
#[test]
fn a_release_is_laid_out_beside_the_running_one_and_nothing_moves() {
    let (fx, sim, backend) = rig("mb-stage");
    assert_eq!(
        backend.stage(&version(NEW_VERSION), &stable()).unwrap(),
        NEW
    );
    let release = fx.host.release(NEW);
    assert_eq!(fs::read(release.join("kbf")).unwrap(), fx.new_binary);
    assert_eq!(
        fs::read_to_string(release.join("VERSION")).unwrap(),
        format!("{NEW_VERSION}\n")
    );
    assert_eq!(current(&fx).as_deref(), Some(OLD));
    assert_eq!(previous(&fx), None);
    assert_eq!(sim.daemon_bootouts.get() + sim.daemon_bootstraps.get(), 0);
    assert!(!fx.host.stage().exists());
    assert!(backend.has(NEW) && backend.has(OLD));
    assert!(!backend.has("ffffffffffffffffffffffffffffffffffffffff"));
}

/// What cannot be staged is refused with a code, or fails, and leaves the kit as it was: a
/// channel the allowlist does not name, no allowlist, a commit the channel does not offer, a
/// build that is not attested, a kit that is not there, a release laid out with other bytes.
#[test]
fn what_cannot_be_staged_leaves_the_kit_as_it_was() {
    let (fx, _, backend) = rig("mb-refuse");
    let fault = backend
        .stage(&version(NEW_VERSION), &Channel::new("beta").unwrap())
        .unwrap_err();
    assert!(refused(Code::ChannelNotAllowed)(&fault), "{fault:?}");

    let fault = backend
        .stage(&version(OTHER_VERSION), &stable())
        .unwrap_err();
    assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");

    fx.sim().break_("gh");
    let (fx2, sim2, backend2) = rig("mb-unattested");
    sim2.break_("gh");
    let fault = backend2
        .stage(&version(NEW_VERSION), &stable())
        .unwrap_err();
    assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
    assert!(!fx2.host.release(NEW).exists());

    // A release of the same commit with other bytes is never replaced.
    let (fx3, _, backend3) = rig("mb-differs");
    mkdir(&fx3.host.release(NEW), 0o755);
    write(&fx3.host.release(NEW).join("kbf"), 0o755, b"another build");
    let fault = backend3
        .stage(&version(NEW_VERSION), &stable())
        .unwrap_err();
    assert!(
        !fault.refused && fault.code == Code::StageFailed,
        "{fault:?}"
    );
    assert!(fault.detail.is_empty() || !fault.detail.contains("another build"));

    let (fx4, _, backend4) = rig("mb-nokit");
    fs::remove_dir_all(fx4.host.kit.join("releases")).unwrap();
    let fault = backend4
        .stage(&version(NEW_VERSION), &stable())
        .unwrap_err();
    assert!(
        !fault.refused && fault.code == Code::StageFailed,
        "{fault:?}"
    );

    let (fx5, _, backend5) = rig("mb-noallowlist");
    fs::remove_file(&fx5.host.allowlist).unwrap();
    let fault = backend5
        .stage(&version(NEW_VERSION), &stable())
        .unwrap_err();
    assert!(refused(Code::ConfigInvalid)(&fault), "{fault:?}");

    // A channel that names no source is a configuration that does not parse.
    let (fx6, _, backend6) = rig("mb-badconf");
    write(&fx6.host.allowlist, 0o644, b"[channels.stable]\n");
    let fault = backend6
        .stage(&version(NEW_VERSION), &stable())
        .unwrap_err();
    assert!(refused(Code::ConfigInvalid)(&fault), "{fault:?}");
}

/// A switch writes the marker (root's, 0600, with the deadline and the links as they were),
/// then moves `previous` and `current` and restarts the daemon on the new release, which must
/// say it was welcomed. (That the marker comes before the first link moves is the next-but-one
/// test: a marker that cannot be written moves nothing.)
#[test]
fn a_switch_writes_the_marker_first_then_moves_and_restarts() {
    let (fx, sim, backend) = rig("mb-switch");
    backend.stage(&version(NEW_VERSION), &stable()).unwrap();
    let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
    {
        let (seen, marker, host) = (
            Rc::clone(&seen),
            fx.host.work.join("pending.json"),
            fx.host.clone(),
        );
        *sim.before.borrow_mut() = Some(Box::new(move |cmd| {
            if cmd.args.first().map(String::as_str) == Some("bootout") {
                seen.borrow_mut()
                    .push((marker.exists(), kit::links(&host).unwrap().current));
            }
        }));
    }
    let deadline = backend.switch(NEW, 600, 5_000).unwrap();
    assert_eq!(deadline, 5_600);
    assert_eq!(
        *seen.borrow(),
        [(true, Some(NEW.to_owned()))],
        "the marker is there when the daemon is first touched, and the links have moved"
    );
    let marker: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fx.host.work.join("pending.json")).unwrap())
            .unwrap();
    assert_eq!(
        marker,
        serde_json::json!({
            "commit": NEW, "from": OLD, "from_previous": null, "deadline_unix": 5_600
        })
    );
    assert_eq!(current(&fx).as_deref(), Some(NEW));
    assert_eq!(previous(&fx).as_deref(), Some(OLD));
    let marker = fx.host.work.join("pending.json");
    assert_eq!(
        fs::metadata(&marker).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    let pending = backend.pending().unwrap().unwrap();
    assert_eq!(
        (
            pending.commit.as_str(),
            pending.from.as_str(),
            pending.deadline_unix
        ),
        (NEW, OLD, Some(5_600))
    );
    assert_eq!(backend.current().unwrap().as_deref(), Some(NEW));
    assert_eq!(backend.node_id().as_deref(), Some("mac-a"));
    assert_eq!(backend.lock_file(), fx.host.work.join("lock"));
}

/// A daemon that does not come up leaves no marker and the old release running: the links are
/// back and the old release started (`SwitchFailed`); when the old release does not start
/// either, the fault says the host could not be put back (`RevertFailed`); links that cannot
/// be moved fail without a restart.
#[test]
fn a_daemon_that_does_not_come_up_leaves_no_trial() {
    let (fx, sim, backend) = rig("mb-noup");
    backend.stage(&version(NEW_VERSION), &stable()).unwrap();
    // The new daemon is never welcomed; the old one is, when it is started again.
    sim.welcome_from.set(2);
    let fault = backend.switch(NEW, 600, 1).unwrap_err();
    assert_eq!(fault.code, Code::SwitchFailed, "{fault:?}");
    assert!(fault.detail.ends_with(":rolled_back"), "{fault:?}");
    assert!(!fx.host.work.join("pending.json").exists());
    assert_eq!((current(&fx).as_deref(), previous(&fx)), (Some(OLD), None));

    let (fx, sim, backend) = rig("mb-noup2");
    backend.stage(&version(NEW_VERSION), &stable()).unwrap();
    sim.welcome.set(false);
    let fault = backend.switch(NEW, 600, 1).unwrap_err();
    assert_eq!(fault.code, Code::RevertFailed, "{fault:?}");
    assert!(!fx.host.work.join("pending.json").exists());

    // Links that cannot be made: the flip stops before the daemon is touched.
    let (fx, sim, backend) = rig("mb-links");
    backend.stage(&version(NEW_VERSION), &stable()).unwrap();
    fs::set_permissions(&fx.host.kit, fs::Permissions::from_mode(0o555)).unwrap();
    let fault = backend.switch(NEW, 600, 1).unwrap_err();
    fs::set_permissions(&fx.host.kit, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fault.code, Code::SwitchFailed, "{fault:?}");
    assert_eq!(fault.detail, "links_failed");
    assert_eq!(sim.daemon_bootouts.get(), 0);
    assert_eq!(current(&fx).as_deref(), Some(OLD));
}

/// A switch needs a release that runs now, a marker it can write, and links it can read.
#[test]
fn a_switch_needs_a_current_release_a_writable_work_directory_and_sound_links() {
    let (fx, _, backend) = rig("mb-needs");
    backend.stage(&version(NEW_VERSION), &stable()).unwrap();
    fs::remove_file(fx.host.current()).unwrap();
    assert!(refused(Code::NoCurrent)(
        &backend.switch(NEW, 600, 1).unwrap_err()
    ));

    std::os::unix::fs::symlink("/somewhere/else", fx.host.current()).unwrap();
    assert_eq!(
        backend.switch(NEW, 600, 1).unwrap_err().code,
        Code::StateUnavailable
    );
    assert_eq!(backend.current().unwrap_err().code, Code::StateUnavailable);
    fs::remove_file(fx.host.current()).unwrap();
    std::os::unix::fs::symlink(format!("releases/{OLD}"), fx.host.current()).unwrap();

    fs::set_permissions(&fx.host.work, fs::Permissions::from_mode(0o500)).unwrap();
    let fault = backend.switch(NEW, 600, 1).unwrap_err();
    assert_eq!(fault.code, Code::StateUnavailable, "{fault:?}");
    fs::set_permissions(&fx.host.work, fs::Permissions::from_mode(0o711)).unwrap();
    assert_eq!(current(&fx).as_deref(), Some(OLD), "nothing moved");
}

/// A marker that is not ours is an unavailable record, not a guess; a marker that cannot be
/// cleared fails the confirm and leaves the trial.
#[test]
fn a_damaged_marker_is_unavailable_and_an_uncleared_one_fails_the_confirm() {
    let (fx, _, backend) = rig("mb-marker");
    let marker = fx.host.work.join("pending.json");
    write(&marker, 0o600, b"{\"commit\": 7");
    assert_eq!(backend.pending().unwrap_err().code, Code::StateUnavailable);
    fs::remove_file(&marker).unwrap();
    std::os::unix::fs::symlink("/nonexistent", &marker).unwrap();
    assert_eq!(backend.pending().unwrap_err().code, Code::StateUnavailable);
    fs::remove_file(&marker).unwrap();

    // Cleared by removing a directory's worth of file: a marker that is a directory stays.
    mkdir(&marker, 0o755);
    write(&marker.join("x"), 0o644, b"x");
    let nothing = Pending {
        commit: NEW.into(),
        from: OLD.into(),
        deadline_unix: Some(1),
        activation: String::new(),
    };
    assert_eq!(
        backend.confirm(&nothing).unwrap_err().code,
        Code::StateUnavailable
    );
}

/// A confirm clears the marker; a revert puts both links back as they were and starts the old
/// release without the new one running; a revert of the old release that does not start keeps
/// the marker so the next try finds it; with no marker there is nothing to revert.
#[test]
fn a_confirm_clears_the_marker_and_a_revert_puts_the_links_and_the_daemon_back() {
    let (fx, sim, backend) = rig("mb-revert");
    backend.stage(&version(NEW_VERSION), &stable()).unwrap();
    backend.switch(NEW, 600, 1).unwrap();
    let pending = backend.pending().unwrap().unwrap();
    backend.confirm(&pending).unwrap();
    assert!(backend.pending().unwrap().is_none());
    assert_eq!(current(&fx).as_deref(), Some(NEW));
    backend.revert(&pending).unwrap();
    assert_eq!(
        current(&fx).as_deref(),
        Some(NEW),
        "no marker, nothing to revert"
    );

    let (fx, sim2, backend) = rig("mb-revert2");
    backend.stage(&version(NEW_VERSION), &stable()).unwrap();
    backend.switch(NEW, 600, 1).unwrap();
    let pending = backend.pending().unwrap().unwrap();
    let boots = sim2.daemon_bootstraps.get();
    backend.revert(&pending).unwrap();
    assert_eq!((current(&fx).as_deref(), previous(&fx)), (Some(OLD), None));
    assert!(!fx.host.work.join("pending.json").exists());
    assert_eq!(
        sim2.daemon_bootstraps.get(),
        boots + 1,
        "the old release was started"
    );

    let (fx, sim3, backend) = rig("mb-revert3");
    backend.stage(&version(NEW_VERSION), &stable()).unwrap();
    backend.switch(NEW, 600, 1).unwrap();
    let pending = backend.pending().unwrap().unwrap();
    sim3.welcome.set(false);
    let fault = backend.revert(&pending).unwrap_err();
    assert_eq!(fault.code, Code::RevertFailed, "{fault:?}");
    assert!(
        fx.host.work.join("pending.json").exists(),
        "the next try finds it"
    );
    drop(sim);
}

/// The whole update through the core on this backend: a release that is never confirmed is put
/// back at its deadline, with the links back and the old daemon running again, and the host
/// then tells the update failed by timeout; confirmed in time, it stays.
#[test]
fn an_unconfirmed_release_is_reverted_by_the_tick_and_a_confirmed_one_stays() {
    let (fx, sim, backend) = rig("mb-e2e");
    let now = Rc::new(std::cell::Cell::new(100_u64));
    let clock = Rc::clone(&now);
    let ab = Ab::new(
        backend,
        fx.host.work.join("state.json"),
        fx.host.trust.clone(),
        600,
        Box::new(move || clock.get()),
    );
    let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
    let install = format!("install kbf={NEW_VERSION} from stable");
    assert!(matches!(
        ask(&install),
        Answer::Staged { current: false, .. }
    ));
    assert!(matches!(
        ask(&format!("switch kbf={NEW_VERSION}")),
        Answer::Switched {
            deadline_unix: 700,
            ..
        }
    ));
    now.set(699);
    assert_eq!(ab.tick(), None);
    now.set(700);
    let boots = sim.daemon_bootstraps.get();
    assert!(matches!(ab.tick(), Some(Answer::Reverted { .. })));
    assert_eq!(current(&fx).as_deref(), Some(OLD));
    assert_eq!(sim.daemon_bootstraps.get(), boots + 1);
    let Answer::Queried(view) = ask("query") else {
        panic!("not a query")
    };
    assert_eq!(view.last.unwrap().reason, Some("timeout"));
    assert_eq!(
        view.version, None,
        "A's version is not known to the host until it is told"
    );

    now.set(1_000);
    assert!(matches!(ask(&install), Answer::Staged { .. }));
    assert!(matches!(
        ask(&format!("switch kbf={NEW_VERSION}")),
        Answer::Switched { .. }
    ));
    assert!(matches!(
        ask(&format!("confirm kbf={NEW_VERSION}")),
        Answer::Confirmed { .. }
    ));
    now.set(50_000);
    assert_eq!(ab.tick(), None);
    assert_eq!(current(&fx).as_deref(), Some(NEW));
    let _ = OLD_VERSION;
}

/// GitHub holds `NEW` as release `v1.0`.
fn github(fx: &Fx, sim: &Sim) {
    write(
        &fx.host.allowlist,
        0o644,
        b"[channels.stable]\nsource = \"github\"\n",
    );
    let release = format!(
        "{{\"tag_name\":\"v1.0\",\"draft\":false,\"assets\":[\
         {{\"name\":\"{}\",\"size\":100}},{{\"name\":\"{}\",\"size\":10}}]}}",
        tarball_name(NEW),
        bundle_name(NEW)
    );
    sim.pages.borrow_mut().push(format!("[{release}]"));
    let tar = tarball(&[("kbf", tar::EntryType::Regular, &fx.new_binary)]);
    let mut assets = sim.assets.borrow_mut();
    let files = assets.entry("v1.0".to_owned()).or_default();
    files.insert(tarball_name(NEW), tar);
    files.insert(bundle_name(NEW), b"{}\n".to_vec());
}

/// What the release source and the configuration say is passed on as the closed code of the
/// socket: a release the channel does not have, two that start the same, a listing that cannot
/// be fetched, an allowlist or a tool that is not root's.
#[test]
fn what_the_release_source_and_the_configuration_say_is_passed_on() {
    let (fx, sim, backend) = rig("mb-github");
    github(&fx, &sim);
    assert_eq!(
        backend.stage(&version(NEW_VERSION), &stable()).unwrap(),
        NEW
    );

    let (fx, sim, backend) = rig("mb-github-unknown");
    github(&fx, &sim);
    sim.pages.borrow_mut()[0] = "[]".to_owned();
    let fault = backend.stage(&version(NEW_VERSION), &stable()).unwrap_err();
    assert!(refused(Code::UnknownRelease)(&fault), "{fault:?}");

    let (fx, sim, backend) = rig("mb-github-twin");
    github(&fx, &sim);
    let twin = format!("{}{}", &NEW[..12], "c".repeat(28));
    let release = format!(
        "{{\"tag_name\":\"v2.0\",\"draft\":false,\"assets\":[\
         {{\"name\":\"{}\",\"size\":100}},{{\"name\":\"{}\",\"size\":10}}]}}",
        tarball_name(&twin),
        bundle_name(&twin)
    );
    let first = sim.pages.borrow()[0].clone();
    sim.pages.borrow_mut()[0] = format!("{},{}]", &first[..first.len() - 1], release);
    let fault = backend.stage(&version(NEW_VERSION), &stable()).unwrap_err();
    assert!(refused(Code::AmbiguousRelease)(&fault), "{fault:?}");

    let (fx, sim, backend) = rig("mb-github-net");
    github(&fx, &sim);
    sim.break_("list-net");
    let fault = backend.stage(&version(NEW_VERSION), &stable()).unwrap_err();
    assert!(refused(Code::FetchFailed)(&fault), "{fault:?}");

    let (fx, _, backend) = rig("mb-unsafe-allowlist");
    fs::set_permissions(&fx.host.allowlist, fs::Permissions::from_mode(0o666)).unwrap();
    let fault = backend.stage(&version(NEW_VERSION), &stable()).unwrap_err();
    assert!(refused(Code::ConfigUnsafe)(&fault), "{fault:?}");

    let (fx, sim, backend) = rig("mb-unsafe-tool");
    github(&fx, &sim);
    fs::set_permissions(&fx.host.gh, fs::Permissions::from_mode(0o666)).unwrap();
    let fault = backend.stage(&version(NEW_VERSION), &stable()).unwrap_err();
    assert!(refused(Code::ConfigUnsafe)(&fault), "{fault:?}");
}

/// A work directory that is not a directory makes the marker unreadable, not absent; and a
/// marker that is not there is cleared without complaint.
#[test]
fn a_work_directory_that_is_not_one_and_an_absent_marker() {
    let (fx, _, backend) = rig("mb-work");
    let nothing = Pending {
        commit: NEW.into(),
        from: OLD.into(),
        deadline_unix: Some(1),
        activation: String::new(),
    };
    backend.confirm(&nothing).unwrap();
    fs::remove_dir_all(&fx.host.work).unwrap();
    write(&fx.host.work, 0o644, b"a file, not a directory");
    assert_eq!(backend.pending().unwrap_err().code, Code::StateUnavailable);
}

#[path = "mac_backend_retain_tests.rs"]
mod retain;
