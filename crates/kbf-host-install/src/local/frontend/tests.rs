//! The update of a frontend container over a fake runtime and a fake frontend: what the runtime
//! is told, in what order, what is kept, and what happens when a step fails or the host goes
//! down in the middle.

use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::rc::Rc;

use kbf_testkit::dir::TestDir;

use super::config::{FrontendConfig, RunSpec};
use super::fake::{API, Source, World};
use super::record::Record;
use super::runtime::Runtime;
use super::{Frontend, RECORD};
use crate::host::Host;
use crate::local::answer::{Answer, Code, QueryView};
use crate::local::backend::{Backend, Fault, Pending};
use crate::local::core::Ab;
use crate::local::request::LocalRequest;
use crate::request::{Channel, Version};
use crate::trust::Trust;

pub(super) const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub(super) const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
pub(super) const VERSION: &str = "0.1.7+gbbbbbbbbbbbb-3";
pub(super) const OLD_ID: &str = "sha256:old";
pub(super) const NEW_ID: &str = "sha256:new";
pub(super) const WINDOW: u64 = 600;

pub(super) const CONFIG: &str = r#"
[frontend]
name = "kbf-frontend"
restart = "unless-stopped"
network = "bridge"
user = "65532:65532"
ports = ["127.0.0.1:8982:8982", "8980:8980"]
mounts = ["/etc/kbf:/etc/kbf:ro", "/var/lib/kbf:/var/lib/kbf"]
command = ["--config", "/etc/kbf/kbf.toml"]
api = "127.0.0.1:8982"
drain_token_file = "/etc/kbf/operator.token"
"#;

pub(super) fn put(path: &std::path::Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

pub(super) fn spec() -> RunSpec {
    FrontendConfig::parse(CONFIG).unwrap().run
}

/// A host with release `OLD` running as a container, and `NEW` on offer.
pub(super) struct Rig {
    pub(super) dir: TestDir,
    pub(super) world: World,
    pub(super) source: Rc<RefCell<Result<String, Fault>>>,
    pub(super) clock: Rc<Cell<u64>>,
    pub(super) ab: Ab<Frontend<Source, World, World>>,
}

impl Rig {
    pub(super) fn new(name: &str) -> Self {
        Self::with_config(name, CONFIG)
    }

    pub(super) fn with_config(name: &str, config: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), &format!("hi-fe-{name}"));
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        put(&dir.join("frontend.toml"), config);
        let world = World::default();
        let old = world.image(OLD, "old");
        assert_eq!(old, OLD_ID);
        world.container(&spec(), &old, OLD);
        assert_eq!(world.will_load(NEW, "new"), NEW_ID);
        let source = Source::new(&world, NEW);
        let answer = Rc::clone(&source.answer);
        let clock = Rc::new(Cell::new(1000));
        let ab = Self::process(&dir, &world, source, &clock);
        Self {
            dir,
            world,
            source: answer,
            clock,
            ab,
        }
    }

    /// A process of the program over the same host: what it starts with is on disk and in the
    /// runtime.
    pub(super) fn process(
        dir: &TestDir,
        world: &World,
        source: Source,
        clock: &Rc<Cell<u64>>,
    ) -> Ab<Frontend<Source, World, World>> {
        let uid = fs::metadata(&**dir).unwrap().uid();
        let trust = Trust::owned_by(uid, dir);
        let frontend = Self::backend_over(dir, world, source);
        let clock = Rc::clone(clock);
        Ab::new(
            frontend,
            dir.join("state.json"),
            trust,
            WINDOW,
            Box::new(move || clock.get()),
        )
    }

    /// A backend over the host, to be asked directly.
    pub(super) fn backend_over(
        dir: &TestDir,
        world: &World,
        source: Source,
    ) -> Frontend<Source, World, World> {
        let uid = fs::metadata(&**dir).unwrap().uid();
        let host = Host::for_test(dir, Trust::owned_by(uid, dir));
        put(&host.node_config, "[node]\nid = \"fe1\"\n");
        Frontend {
            source,
            runtime: world.clone(),
            probe: world.clone(),
            host,
            config_file: dir.join("frontend.toml"),
            record_file: dir.join(RECORD),
            bound_name: None,
            pace: world.pace(),
        }
    }

    pub(super) fn backend(&self) -> Frontend<Source, World, World> {
        Self::backend_over(&self.dir, &self.world, Source::new(&self.world, NEW))
    }

    /// The program started again: a new process over the same host.
    pub(super) fn restarted(&self) -> Ab<Frontend<Source, World, World>> {
        Self::process(
            &self.dir,
            &self.world,
            Source::new(&self.world, NEW),
            &self.clock,
        )
    }

    pub(super) fn ask(&self, line: &str) -> Answer {
        self.ab.handle(&LocalRequest::parse(line).unwrap())
    }

    pub(super) fn install(&self) -> Answer {
        self.ask(&format!("install kbf={VERSION} from stable"))
    }

    pub(super) fn switch(&self) -> Answer {
        self.ask(&format!("switch kbf={VERSION}"))
    }

    pub(super) fn confirm(&self) -> Answer {
        self.ask(&format!("confirm kbf={VERSION}"))
    }

    /// Install and switch; the switch's answer.
    pub(super) fn update(&self) -> Answer {
        assert!(matches!(self.install(), Answer::Staged { .. }));
        self.switch()
    }

    pub(super) fn query(&self) -> QueryView {
        match self.ask("query") {
            Answer::Queried(view) => view,
            other => panic!("{other:?}"),
        }
    }

    pub(super) fn record(&self) -> Record {
        let uid = fs::metadata(&*self.dir).unwrap().uid();
        Record::load(&Trust::owned_by(uid, &self.dir), &self.dir.join(RECORD)).unwrap()
    }

    /// The events that are not looks at the frontend.
    pub(super) fn steps(&self) -> Vec<String> {
        self.world
            .events()
            .into_iter()
            .filter(|e| !e.starts_with("get:"))
            .collect()
    }

    pub(super) fn config_path(&self) -> PathBuf {
        self.dir.join("frontend.toml")
    }
}

pub(super) fn refused(code: Code) -> impl Fn(&Answer) -> bool {
    move |a| matches!(a, Answer::Refused(c, _) if *c == code)
}

pub(super) fn failed(code: Code) -> impl Fn(&Answer) -> bool {
    move |a| matches!(a, Answer::Failed(c, _) if *c == code)
}

pub(super) const DRAIN: &str =
    "exec:/usr/bin/kbf cluster drain --frontend --token-file /etc/kbf/operator.token";

/// An update verifies and loads the image first (nothing is stopped before the fetch is done),
/// then drains, stops, removes and recreates, in this order, and the container it makes runs
/// the new image; the restart policy is switched off before the drain so that a drained
/// container that ends is not brought back.
#[test]
fn the_order_is_verify_load_drain_stop_recreate_then_ready() {
    let rig = Rig::new("order");
    assert!(matches!(rig.update(), Answer::Switched { commit, .. } if commit == NEW));
    assert_eq!(
        rig.steps(),
        [
            "fetch".to_owned(),
            "load".to_owned(),
            "set_restart:no".to_owned(),
            DRAIN.to_owned(),
            "stop".to_owned(),
            "remove".to_owned(),
            format!("run:{NEW_ID}"),
        ]
    );
    // The readiness is asked of the new container, after it was made.
    let events = rig.world.events();
    let run = events
        .iter()
        .position(|e| e == &format!("run:{NEW_ID}"))
        .unwrap();
    assert!(
        events[run + 1..]
            .iter()
            .any(|e| e == &format!("get:{API}/readyz"))
    );
    assert!(
        events[run + 1..]
            .iter()
            .any(|e| e == &format!("get:{API}/healthz"))
    );
    assert_eq!(rig.world.made().unwrap().image_id, NEW_ID);
}

/// The drain is waited for: the old container is stopped only once it has ended by itself
/// (a drained frontend exits when its work is done).
#[test]
fn a_drain_is_waited_for_until_the_container_ends() {
    let rig = Rig::new("drain-ends");
    rig.world.0.borrow_mut().ends_after = Some(3);
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    assert_eq!(rig.world.0.borrow().drain_looks, 3);
    // Three looks of a second each, not the deadline of 180 s.
    assert!(rig.world.elapsed().as_secs() < 100);
}

/// A container that is not running has nothing to drain: it is replaced without a drain.
#[test]
fn a_container_that_is_not_running_is_replaced_without_a_drain() {
    let rig = Rig::new("not-running");
    rig.world.0.borrow_mut().container.as_mut().unwrap().running = false;
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    assert!(!rig.steps().iter().any(|e| e.starts_with("exec:")));
    assert_eq!(rig.world.made().unwrap().image_id, NEW_ID);
}

/// The new container is made with the arguments of the old one, byte for byte, but for the
/// image; the old one put back is made from the same arguments too.
#[test]
fn the_run_configuration_is_kept_byte_for_byte() {
    let rig = Rig::new("bytes");
    let original = rig.world.made().unwrap().args;
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    let new = rig.world.made().unwrap().args;
    assert_eq!(new.len(), original.len());
    let differing: Vec<usize> = (0..new.len()).filter(|i| new[*i] != original[*i]).collect();
    assert_eq!(differing.len(), 1, "{new:?} against {original:?}");
    assert_eq!(new[differing[0]], NEW_ID);
    assert_eq!(original[differing[0]], OLD_ID);
    // Spelled out, so that a change of the order is a change of the test.
    assert_eq!(
        new,
        [
            "run",
            "--detach",
            "--name",
            "kbf-frontend",
            "--restart",
            "unless-stopped",
            "--network",
            "bridge",
            "--user",
            "65532:65532",
            "--publish",
            "127.0.0.1:8982:8982",
            "--publish",
            "8980:8980",
            "--volume",
            "/etc/kbf:/etc/kbf:ro",
            "--volume",
            "/var/lib/kbf:/var/lib/kbf",
            NEW_ID,
            "--config",
            "/etc/kbf/kbf.toml",
        ]
    );
    // The file changed since; a revert still makes the old container from the configuration
    // the switch began with.
    put(
        &rig.config_path(),
        &CONFIG
            .replace("8980:8980", "9999:9999")
            .replace("bridge", "host"),
    );
    assert!(matches!(rig.ask("revert"), Answer::Reverted { .. }));
    assert_eq!(rig.world.made().unwrap().args, original);
    let runs = rig.world.runs();
    assert_eq!(runs.last().unwrap(), &original);
}

/// The snapshot is in the pending record, written before the first stop.
#[test]
fn the_marker_is_written_before_anything_is_stopped() {
    let rig = Rig::new("marker-first");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    // A stop that fails stops the switch at its first step after the drain: the marker is on
    // disk by then, and the record keeps the configuration and both images.
    rig.world.fail("stop");
    rig.world.fail(&format!("run:{OLD_ID}"));
    assert!(failed(Code::RevertFailed)(&rig.switch()));
    let trial = rig.record().pending.unwrap();
    assert_eq!(trial.commit, NEW);
    assert_eq!(trial.image, NEW_ID);
    assert_eq!(trial.from, OLD);
    assert_eq!(trial.from_image, OLD_ID);
    // Counted from the end of the drain (one look of a second here).
    assert_eq!(trial.deadline_unix, Some(1000 + 1 + WINDOW));
    assert_eq!(trial.config, FrontendConfig::parse(CONFIG).unwrap());
}

/// Whatever step of the switch fails, the container is made again from the old image, waited
/// for, and the switch is reported failed; the marker is gone and the old release is current.
#[test]
fn a_failed_step_puts_the_old_image_back_and_reports_failed() {
    for call in [
        "set_restart".to_owned(),
        "exec".to_owned(),
        "stop".to_owned(),
        format!("run:{NEW_ID}"),
    ] {
        let rig = Rig::new("step-fails");
        let original = rig.world.made().unwrap().args;
        rig.world.fail(&call);
        let answer = rig.update();
        assert!(failed(Code::SwitchFailed)(&answer), "{call}: {answer:?}");
        let now = rig.world.made().unwrap();
        assert_eq!(
            (now.image_id.as_str(), now.running),
            (OLD_ID, true),
            "{call}"
        );
        assert_eq!(now.args, original, "{call}");
        assert_eq!(now.restart, "unless-stopped", "{call}");
        assert!(rig.record().pending.is_none(), "{call}");
        let view = rig.query();
        assert_eq!(view.commit.as_deref(), Some(OLD), "{call}");
        assert!(view.pending.is_none());
        assert_eq!(view.last.unwrap().reason, Some("switch_failed"), "{call}");
    }
}

/// A new frontend that never gets ready, or whose `/healthz` names another commit, is not
/// kept: the old image is made again and waited for.
#[test]
fn a_frontend_that_is_not_ready_on_the_target_commit_is_replaced_by_the_old_one() {
    let rig = Rig::new("not-ready");
    rig.world
        .0
        .borrow_mut()
        .never_ready
        .insert(NEW_ID.to_owned());
    assert!(failed(Code::SwitchFailed)(&rig.update()));
    assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
    // It waited the whole of ready_secs for the new one, then again for the old one.
    assert!(rig.world.elapsed().as_secs() >= 90);
    let steps = rig.steps();
    let tail: Vec<&str> = steps.iter().rev().take(3).map(String::as_str).collect();
    assert_eq!(
        tail,
        [
            format!("run:{OLD_ID}").as_str(),
            "remove",
            &format!("run:{NEW_ID}")
        ]
    );

    let rig = Rig::new("other-commit");
    rig.world.0.borrow_mut().lies_about_commit = Some("cccccccccccc".to_owned());
    // The old release is "ready on cccc..." too, so it cannot be waited for either: the
    // restore fails, and the marker stays for the deadline to retry.
    assert!(failed(Code::RevertFailed)(&rig.update()));
    assert!(rig.record().pending.is_some());

    // A commit that is too short to name one is no name.
    let rig = Rig::new("short-commit");
    rig.world.0.borrow_mut().lies_about_commit = Some("bbbbbbbb".to_owned());
    assert!(failed(Code::RevertFailed)(&rig.update()));
}

/// The previous image was pruned while the release was on trial: a revert cannot make it
/// again, so it fails without touching the container that runs.
#[test]
fn a_revert_with_a_pruned_previous_image_keeps_the_running_container_and_fails() {
    let rig = Rig::new("revert-pruned");
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    rig.world.remove_image(OLD_ID).unwrap();
    let before = rig.steps().len();

    let answer = rig.ask("revert");
    assert!(failed(Code::RevertFailed)(&answer), "{answer:?}");
    assert!(
        matches!(&answer, Answer::Failed(_, why) if why.contains(OLD_ID)),
        "{answer:?}"
    );
    let made = rig.world.made().unwrap();
    assert_eq!((made.image_id.as_str(), made.running), (NEW_ID, true));
    // The image was looked up and that is all: no stop, no remove, no run.
    assert_eq!(rig.steps()[before..], [] as [String; 0]);
    assert!(rig.record().pending.is_some());
}

/// The same when the switch itself fails: the new container is not ready, the old image is
/// gone, and the new container stays up with the failure of the going back reported.
#[test]
fn a_switch_that_is_not_ready_with_a_pruned_previous_image_leaves_the_new_container() {
    let rig = Rig::new("switch-pruned");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    rig.world.remove_image(OLD_ID).unwrap();
    rig.world
        .0
        .borrow_mut()
        .never_ready
        .insert(NEW_ID.to_owned());

    let answer = rig.switch();
    assert!(failed(Code::RevertFailed)(&answer), "{answer:?}");
    let made = rig.world.made().unwrap();
    assert_eq!((made.image_id.as_str(), made.running), (NEW_ID, true));
    // The old container was replaced by the switch itself; after the new one failed there
    // was one `run` and no second `remove`.
    let steps = rig.steps();
    assert_eq!(steps.iter().filter(|e| *e == "remove").count(), 1);
    assert_eq!(
        steps
            .iter()
            .filter(|e| e.starts_with("run:"))
            .collect::<Vec<_>>(),
        [&format!("run:{NEW_ID}")]
    );
    assert_eq!(steps.last().unwrap(), &format!("run:{NEW_ID}"));
    assert!(rig.record().pending.is_some());
}

/// A container that ends as soon as it starts never answers: the switch fails after the time
/// allowed, and the old one is put back.
#[test]
fn a_container_that_exits_at_once_is_not_ready() {
    let rig = Rig::new("crashes");
    {
        let mut world = rig.world.0.borrow_mut();
        world.crashes.insert(NEW_ID.to_owned());
        world.ends_after = Some(1);
    }
    assert!(failed(Code::SwitchFailed)(&rig.update()));
    assert!(
        (90..100).contains(&rig.world.elapsed().as_secs()),
        "{:?}",
        rig.world.elapsed()
    );
    let now = rig.world.made().unwrap();
    assert_eq!((now.image_id.as_str(), now.running), (OLD_ID, true));
}

/// A runtime that stops answering while the drain is waited for fails the switch, and the old
/// container is made again.
#[test]
fn a_runtime_that_stops_answering_during_the_drain_fails_the_switch() {
    let rig = Rig::new("inspect-dies");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    {
        let mut world = rig.world.0.borrow_mut();
        world.inspects = 0;
        world.inspect_fails_from = Some(3);
    }
    let answer = rig.switch();
    rig.world.0.borrow_mut().inspect_fails_from = None;
    assert!(failed(Code::SwitchFailed)(&answer), "{answer:?}");
    assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
}

/// A release counts as loaded only when the record has its image and the runtime still does;
/// a record that cannot be read has none.
#[test]
fn a_release_is_loaded_only_when_the_record_and_the_runtime_agree() {
    let rig = Rig::new("has");
    let backend = rig.backend();
    assert!(!backend.has(NEW), "nothing is recorded yet");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    assert!(backend.has(NEW));
    assert!(!backend.has(OLD));
    fs::write(rig.dir.join(RECORD), "{not json").unwrap();
    assert!(!backend.has(NEW));
}

/// A confirm forgets the old image unless it is the new one; a revert needs a release on trial.
#[test]
fn a_confirm_keeps_an_image_the_old_release_shares_and_a_revert_needs_a_trial() {
    let rig = Rig::new("shared-image");
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    let mut record = rig.record();
    record.pending.as_mut().unwrap().from_image = NEW_ID.to_owned();
    record.save(&rig.dir.join(RECORD)).unwrap();
    let backend = rig.backend();
    let pending = backend.pending().unwrap().unwrap();
    backend.confirm(&pending).unwrap();
    assert!(
        !rig.world
            .events()
            .iter()
            .any(|e| e.starts_with("remove_image:"))
    );
    assert_eq!(rig.record().current.unwrap().commit, NEW);

    let fault = backend.revert(&pending).unwrap_err();
    assert_eq!(fault.code, Code::StateUnavailable);
}

/// The real pace counts time and sleeps.
#[test]
fn the_real_pace_tells_time_and_sleeps() {
    let pace = super::Pace::real();
    assert_eq!(pace.poll, std::time::Duration::from_secs(2));
    let before = (pace.now)();
    (pace.sleep)(std::time::Duration::from_millis(5));
    assert!((pace.now)() > before);
}

/// The frontend of the program is on the host's paths: the record under the private state
/// directory, the configuration under `/etc/kbf`, and the host's own trust and lock.
#[cfg(target_os = "linux")]
#[test]
fn the_production_frontend_is_on_the_hosts_paths() {
    let frontend = super::production(PathBuf::from(super::config::CONFIG), None);
    assert!(frontend.bound_name.is_none() && frontend.source.scratch_tag.is_none());
    assert_eq!(
        frontend.config_file,
        PathBuf::from("/etc/kbf/host-install-frontend.toml")
    );
    assert_eq!(
        frontend.record_file,
        PathBuf::from("/var/lib/kbf-host-install/frontend.json")
    );
    assert_eq!(
        frontend.lock_file(),
        PathBuf::from("/run/kbf-host-install.lock")
    );
    assert_eq!(frontend.pace.poll, std::time::Duration::from_secs(2));
    // With a file of its own, the container's name makes the record and the lock.
    let own = super::production(PathBuf::from("/etc/kbf/fe-b.toml"), Some("kbf-fe-b".into()));
    assert_eq!(own.config_file, PathBuf::from("/etc/kbf/fe-b.toml"));
    assert_eq!(own.source.scratch_tag.as_deref(), Some("kbf-fe-b"));
    assert_eq!(
        own.record_file,
        PathBuf::from("/var/lib/kbf-host-install/frontend-kbf-fe-b.json")
    );
    assert_eq!(
        own.lock_file(),
        PathBuf::from("/run/kbf-host-install-frontend-kbf-fe-b.lock")
    );
}

/// When the old container cannot be made again either, the switch fails with the marker kept,
/// and a later revert (the deadline's, or the updater's) finishes the work.
#[test]
fn a_restore_that_fails_keeps_the_marker_and_a_later_revert_finishes_it() {
    let rig = Rig::new("restore-fails");
    rig.world.fail(&format!("run:{NEW_ID}"));
    rig.world.fail(&format!("run:{OLD_ID}"));
    let answer = rig.update();
    assert!(failed(Code::RevertFailed)(&answer), "{answer:?}");
    assert!(rig.world.made().is_none());
    assert!(rig.record().pending.is_some());
    assert!(rig.query().pending.is_some());

    rig.world.heal(&format!("run:{OLD_ID}"));
    assert!(matches!(rig.ask("revert"), Answer::Reverted { .. }));
    assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
    assert!(rig.record().pending.is_none());
    assert_eq!(rig.query().commit.as_deref(), Some(OLD));

    // And a revert that fails again keeps the marker as it was.
    let rig = Rig::new("revert-fails");
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    rig.world.fail(&format!("run:{OLD_ID}"));
    assert!(failed(Code::RevertFailed)(&rig.ask("revert")));
    assert!(rig.record().pending.is_some());
}

/// A host that goes down after the switch has the new container up and no confirm: when it
/// starts again, the first tick is past the deadline and the old image is made again.
#[test]
fn a_reboot_during_the_switch_reverts_on_the_deadline() {
    let rig = Rig::new("reboot");
    assert!(
        matches!(rig.update(), Answer::Switched { deadline_unix, .. } if deadline_unix == 1000 + 1 + WINDOW)
    );
    assert_eq!(rig.world.made().unwrap().image_id, NEW_ID);

    // The host was down past the deadline.
    rig.clock.set(1000 + WINDOW + 5000);
    let started = rig.restarted();
    let done = started.tick().expect("the deadline has passed");
    assert!(matches!(done, Answer::Reverted { .. }), "{done:?}");
    let now = rig.world.made().unwrap();
    assert_eq!((now.image_id.as_str(), now.running), (OLD_ID, true));
    assert!(rig.record().pending.is_none());
    let last = rig.query().last.unwrap();
    assert_eq!((last.outcome, last.reason), ("failed", Some("timeout")));
}

/// Before the deadline a restart of the program changes nothing: the release stays on trial and
/// can still be confirmed.
#[test]
fn a_restart_before_the_deadline_leaves_the_trial_alone() {
    let rig = Rig::new("restart-early");
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    rig.clock.set(1000 + WINDOW - 1);
    let started = rig.restarted();
    assert!(started.tick().is_none());
    assert_eq!(rig.world.made().unwrap().image_id, NEW_ID);
    assert!(matches!(
        started.handle(&LocalRequest::parse(&format!("confirm kbf={VERSION}")).unwrap()),
        Answer::Confirmed { .. }
    ));
}

/// A host that went down between the marker and the new container has no container at all:
/// the deadline makes the old one.
#[test]
fn a_crash_after_the_marker_and_before_the_new_container_is_reverted_at_the_deadline() {
    let rig = Rig::new("crash-midway");
    rig.world.fail(&format!("run:{NEW_ID}"));
    rig.world.fail(&format!("run:{OLD_ID}"));
    assert!(failed(Code::RevertFailed)(&rig.update()));
    rig.world.heal(&format!("run:{OLD_ID}"));
    rig.world.heal(&format!("run:{NEW_ID}"));
    rig.clock.set(1000 + WINDOW + 1);
    let done = rig.restarted().tick().unwrap();
    assert!(matches!(done, Answer::Reverted { .. }), "{done:?}");
    assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
}

/// A confirm keeps the new release as current, clears the marker and forgets the old image.
#[test]
fn a_confirm_keeps_the_new_container_and_forgets_the_old_image() {
    let rig = Rig::new("confirm");
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    assert!(matches!(rig.confirm(), Answer::Confirmed { .. }));
    assert!(rig.record().pending.is_none());
    assert_eq!(rig.record().current.unwrap().commit, NEW);
    assert!(
        rig.world
            .events()
            .contains(&format!("remove_image:{OLD_ID}"))
    );
    assert_eq!(rig.query().commit.as_deref(), Some(NEW));
    let last = rig.query().last.unwrap();
    assert_eq!(last.outcome, "succeeded");
}

/// A confirm of a release that is not the one on trial is refused by the record.
#[test]
fn a_confirm_of_another_release_than_the_one_on_trial_fails() {
    let rig = Rig::new("confirm-other");
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    let backend = rig.backend();
    let other = Pending {
        commit: OLD.to_owned(),
        from: String::new(),
        deadline_unix: None,
        activation: String::new(),
    };
    let fault = backend.confirm(&other).unwrap_err();
    assert_eq!(fault.code, Code::StateUnavailable);
    assert!(rig.record().pending.is_some());
}

/// Another frontend this host can see that is not ready keeps this one up: nothing is
/// stopped, nothing is written, and the refusal says so.
#[test]
fn a_peer_that_is_not_ready_refuses_the_switch_before_anything_stops() {
    let config = CONFIG.replace(
        "api =",
        "peers = [\"203.0.113.11:8982\", \"203.0.113.12:8982\"]\napi =",
    );
    for second in [Some(503), None] {
        let rig = Rig::with_config("peer", &config);
        {
            let mut world = rig.world.0.borrow_mut();
            world
                .peers
                .insert("203.0.113.11:8982".parse().unwrap(), 200);
            if let Some(status) = second {
                world
                    .peers
                    .insert("203.0.113.12:8982".parse().unwrap(), status);
            }
        }
        assert!(matches!(rig.install(), Answer::Staged { .. }));
        let answer = rig.switch();
        assert!(refused(Code::PeerNotReady)(&answer), "{answer:?}");
        assert_eq!(rig.steps(), ["fetch".to_owned(), "load".to_owned()]);
        assert!(rig.record().pending.is_none());
        assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
    }
    // All peers ready: the switch goes on, and the peers are asked before the marker.
    let rig = Rig::with_config("peers-ready", &config);
    for peer in ["203.0.113.11:8982", "203.0.113.12:8982"] {
        rig.world
            .0
            .borrow_mut()
            .peers
            .insert(peer.parse().unwrap(), 200);
    }
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    let events = rig.world.events();
    let asked = events
        .iter()
        .position(|e| e == "get:203.0.113.12:8982/readyz")
        .unwrap();
    let stopped = events.iter().position(|e| e == "stop").unwrap();
    assert!(asked < stopped);
}

/// A configuration that is not usable refuses the switch (and says what is wrong in the log).
#[test]
fn an_unusable_configuration_refuses_the_switch() {
    let rig = Rig::new("bad-config");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    put(&rig.config_path(), "[frontend]\nname = \"x y\"\n");
    assert!(refused(Code::ConfigInvalid)(&rig.switch()));
    assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
}

/// A switch needs the release loaded, another release than the one that runs, and a container.
#[test]
fn a_switch_without_a_loaded_release_a_different_release_or_a_container_is_refused() {
    let rig = Rig::new("refusals");
    // The updater's request for a release it never installed.
    assert!(refused(Code::NotStaged)(&rig.switch()));

    let backend = rig.backend();
    let refused_with =
        |fault: Fault, code: Code| assert!(fault.refused && fault.code == code, "{fault:?}");
    // The backend asked directly: nothing is recorded for the release.
    refused_with(
        backend.switch(NEW, WINDOW, 1000).unwrap_err(),
        Code::NotStaged,
    );

    // Loaded, then the image is gone from the runtime.
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    rig.world.0.borrow_mut().ids.remove(NEW_ID);
    assert!(!backend.has(NEW));
    refused_with(
        backend.switch(NEW, WINDOW, 1000).unwrap_err(),
        Code::NotStaged,
    );
    rig.world.0.borrow_mut().ids.insert(NEW_ID.to_owned());
    assert!(backend.has(NEW));

    // No container, and nothing recorded of what ran.
    let container = rig.world.made();
    rig.world.0.borrow_mut().container = None;
    refused_with(
        backend.switch(NEW, WINDOW, 1000).unwrap_err(),
        Code::NoCurrent,
    );

    // A record that knows what ran, and a container that is gone.
    let mut record = rig.record();
    record.current = Some(super::record::Slot {
        commit: OLD.to_owned(),
        image: OLD_ID.to_owned(),
    });
    record.save(&rig.dir.join(RECORD)).unwrap();
    refused_with(
        backend.switch(NEW, WINDOW, 1000).unwrap_err(),
        Code::NoCurrent,
    );
    rig.world.0.borrow_mut().container = container;

    // The release that runs is not switched to.
    record.current = Some(super::record::Slot {
        commit: NEW.to_owned(),
        image: NEW_ID.to_owned(),
    });
    record.save(&rig.dir.join(RECORD)).unwrap();
    refused_with(
        backend.switch(NEW, WINDOW, 1000).unwrap_err(),
        Code::NotStaged,
    );
}

/// A runtime that cannot be asked fails the request with its own code; a host with no record
/// and no container knows no release.
#[test]
fn a_runtime_that_cannot_be_asked_fails_with_its_own_code() {
    let rig = Rig::new("runtime-down");
    rig.world.fail("inspect");
    assert!(matches!(
        rig.ask("query"),
        Answer::Failed(Code::RuntimeFailed, _)
    ));
    rig.world.heal("inspect");

    rig.world.fail("image_id");
    assert!(failed(Code::RuntimeFailed)(&rig.install()));
    rig.world.heal("image_id");
    rig.world.fail("load");
    assert!(failed(Code::RuntimeFailed)(&rig.install()));
    assert!(rig.record().images.is_empty());
}

/// Before its first switch a host has no record: what runs is read from the image the
/// container was made with, and a container made from another reference runs no known release.
#[test]
fn before_the_first_switch_the_container_says_what_runs() {
    let rig = Rig::new("infer");
    assert_eq!(rig.query().commit.as_deref(), Some(OLD));
    assert_eq!(rig.query().node_id.as_deref(), Some("fe1"));
    rig.world
        .0
        .borrow_mut()
        .container
        .as_mut()
        .unwrap()
        .image_ref = "nginx:latest".to_owned();
    assert_eq!(rig.query().commit, None);
    assert!(refused(Code::NoCurrent)(&rig.install()));
}

/// An image file the check let through that holds no image of the release is a failure of the
/// stage, and nothing is recorded.
#[test]
fn an_image_file_without_the_releases_image_is_a_stage_failure() {
    let rig = Rig::new("empty-image");
    rig.world.0.borrow_mut().will_load = None;
    assert!(failed(Code::StageFailed)(&rig.install()));
    assert!(rig.record().images.is_empty());
}

/// A record that cannot be written stops a switch before anything changes.
#[test]
fn a_record_that_cannot_be_written_stops_the_switch_before_anything_changes() {
    let rig = Rig::new("record-unwritable");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    fs::set_permissions(&*rig.dir, fs::Permissions::from_mode(0o555)).unwrap();
    let answer = rig.switch();
    fs::set_permissions(&*rig.dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed(Code::StateUnavailable)(&answer), "{answer:?}");
    assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
    assert!(!rig.steps().contains(&"stop".to_owned()));
}

/// A record that is not root's is not believed.
#[test]
fn a_record_that_is_not_trusted_is_not_read() {
    let rig = Rig::new("record-untrusted");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    fs::set_permissions(rig.dir.join(RECORD), fs::Permissions::from_mode(0o666)).unwrap();
    assert!(failed(Code::StateUnavailable)(&rig.switch()));
    fs::write(rig.dir.join(RECORD), "{not json").unwrap();
    fs::set_permissions(rig.dir.join(RECORD), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(failed(Code::StateUnavailable)(&rig.switch()));
}

/// Staging again forgets the images of releases nothing needs.
#[test]
fn only_the_images_a_record_needs_are_kept() {
    let rig = Rig::new("prune");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    let mut record = rig.record();
    record
        .images
        .insert("c".repeat(40), "sha256:stale".to_owned());
    record.save(&rig.dir.join(RECORD)).unwrap();
    rig.world.will_load(NEW, "new");
    // The same version again is a repeat of a staged release; stage another to prune.
    *rig.source.borrow_mut() = Ok(NEW.to_owned());
    let backend = rig.backend();
    backend
        .stage(
            &Version::new(VERSION).unwrap(),
            &Channel::new("stable").unwrap(),
        )
        .unwrap();
    let kept: Vec<String> = rig.record().images.keys().cloned().collect();
    assert_eq!(kept, [NEW.to_owned()]);
}

/// A fetch that is refused, or that fails, changes nothing: the runtime is never asked.
#[test]
fn an_image_that_fails_the_check_changes_nothing() {
    for code in [Code::VerifyFailed, Code::FetchFailed, Code::UnknownRelease] {
        let rig = Rig::new("bad-image");
        *rig.source.borrow_mut() = Err(Fault::refused(code, "no"));
        assert!(refused(code)(&rig.install()), "{code:?}");
        assert!(refused(code)(&rig.install()));
        assert_eq!(rig.world.events(), ["fetch".to_owned(), "fetch".to_owned()]);
        assert!(!rig.dir.join(RECORD).exists());
        let now = rig.world.made().unwrap();
        assert_eq!((now.image_id.as_str(), now.running), (OLD_ID, true));
    }
}
