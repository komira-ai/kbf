//! A service that runs as a container, updated through the same five requests as the
//! frontend: over the fake runtime, with the descriptors read from a root-owned channels file
//! by the hub, so that the record, the state and the lock of the service are its own.

use std::cell::Cell;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::rc::Rc;

use kbf_testkit::dir::TestDir;

use super::Frontend;
use super::fake::{Source, World};
use super::record::Record;
use super::tests::{NEW, OLD, failed, put, refused};
use crate::host::Host;
use crate::local::answer::{Answer, Code};
use crate::local::backend::Backend;
use crate::local::hub::{Hub, describe};
use crate::local::request::Addressed;
use crate::local::services::Services;
use crate::trust::Trust;

const OLD_ID: &str = "sha256:old";
const NEW_ID: &str = "sha256:new";
const WINDOW: u64 = 600;
const VERSION: &str = "0.1.7+gbbbbbbbbbbbb-3";
const API: &str = "127.0.0.1:8080";

/// The descriptor of `js-server`, with `extra` lines added to its container table.
fn channels(extra: &str) -> String {
    format!(
        r#"trusted_workflows = ["owner/js/.github/workflows/release.yml"]
[services.js-server]
kind = "container"
artifact = "js-server"
tree = "js-server"
[services.js-server.container]
name = "js-server"
restart = "unless-stopped"
network = "bridge"
user = "65532:65532"
group_add = ["990"]
read_only = true
tmpfs = ["/tmp:rw,size=64m"]
cap_drop = ["ALL"]
security_opt = ["no-new-privileges"]
ports = ["127.0.0.1:8080:8080"]
mounts = ["/etc/js:/etc/js:ro"]
command = ["--config", "/etc/js/server.toml"]
ready_url = "http://127.0.0.1:8080/readyz"
{extra}
[channels.js-stable]
service = "js-server"
source = "github"
repository = "owner/js"
signer_workflow = "owner/js/.github/workflows/release.yml"
artifact_prefix = "js-server"
"#
    )
}

const DRAIN_EXTRA: &str = "drain_token_file = \"/etc/js/operator.token\"\nversion_url = \"http://127.0.0.1:8080/version\"";

/// A host with `js-server` release `OLD` running as a container and `NEW` on offer.
struct Rig {
    dir: TestDir,
    world: World,
    clock: Rc<Cell<u64>>,
    window: u64,
}

impl Rig {
    fn new(name: &str) -> Self {
        Self::with(name, "", WINDOW)
    }

    fn with(name: &str, extra: &str, window: u64) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), &format!("hi-svc-{name}"));
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        put(&dir.join("releases.toml"), &channels(extra));
        let world = World::default();
        world.repository("js-server");
        world.serving("js-server", API);
        let rig = Self {
            dir,
            world,
            clock: Rc::new(Cell::new(1000)),
            window,
        };
        let spec = rig.descriptor().container.unwrap().run;
        let old = rig.world.image(OLD, "old");
        assert_eq!(old, OLD_ID);
        rig.world.container(&spec, &old, OLD);
        assert_eq!(rig.world.will_load(NEW, "new"), NEW_ID);
        rig
    }

    fn trust(&self) -> Trust {
        Trust::owned_by(fs::metadata(&*self.dir).unwrap().uid(), &self.dir)
    }

    fn descriptor(&self) -> crate::service::Descriptor {
        describe(&self.trust(), &self.dir.join("releases.toml"))
            .unwrap()
            .remove(0)
    }

    fn backend(&self) -> Frontend<Source, World, World> {
        let trust = self.trust();
        let mut host = Host::for_test(&self.dir, trust);
        host.install_lock = self.dir.join("js-server.lock");
        put(&host.node_config, "[node]\nid = \"js1\"\n");
        Frontend {
            source: Source::new(&self.world, NEW),
            runtime: self.world.clone(),
            probe: self.world.clone(),
            host,
            config_file: self.dir.join("unused.toml"),
            record_file: self.dir.join("container-js-server.json"),
            bound_name: None,
            described: self.descriptor().container,
            pace: self.world.pace(),
        }
    }

    fn hub(&self) -> Hub<'_> {
        let (reader, path) = (self.trust(), self.dir.join("releases.toml"));
        let clock = Rc::clone(&self.clock);
        Hub {
            describe: Box::new(move || describe(&reader, &path)),
            make: Box::new(move |_| Ok(Box::new(self.backend()))),
            state_dir: self.dir.to_path_buf(),
            trust: self.trust(),
            window_secs: self.window,
            clock: Rc::new(move || clock.get()),
        }
    }

    fn ask(&self, line: &str) -> Answer {
        let addressed = Addressed::parse(line).unwrap();
        self.hub().handle(&addressed.service, &addressed.request)
    }

    fn install(&self) -> Answer {
        self.ask(&format!("install js-server={VERSION} from js-stable"))
    }

    fn switch(&self) -> Answer {
        self.ask(&format!("switch js-server={VERSION}"))
    }

    fn update(&self) -> Answer {
        assert!(matches!(self.install(), Answer::Staged { .. }));
        self.switch()
    }

    fn record(&self) -> Record {
        Record::load(&self.trust(), &self.dir.join("container-js-server.json")).unwrap()
    }

    fn steps(&self) -> Vec<String> {
        self.world
            .events()
            .into_iter()
            .filter(|e| !e.starts_with("get:"))
            .collect()
    }
}

/// A container service installs (the image is fetched, checked and loaded as
/// `js-server:<commit>`), switches with no drain, is ready when its probe answers 2xx, and
/// confirms: the order is load, stop, remove, run, and the old image is forgotten at confirm.
/// Its record and state are its own files, and the frontend's are not made.
#[test]
fn a_container_service_installs_switches_without_a_drain_and_confirms() {
    let rig = Rig::new("happy");
    // Before any switch the container says what runs: its image is `js-server:<commit>`.
    match rig.ask("query js-server") {
        Answer::Queried(view) => assert_eq!(view.commit.as_deref(), Some(OLD)),
        other => panic!("{other:?}"),
    }
    assert!(matches!(rig.install(), Answer::Staged { commit, .. } if commit == NEW));
    assert_eq!(rig.steps(), ["fetch", "load"]);
    assert_eq!(
        rig.record().images.get(NEW).map(String::as_str),
        Some(NEW_ID)
    );

    assert!(matches!(rig.switch(), Answer::Switched { commit, .. } if commit == NEW));
    assert_eq!(
        rig.steps(),
        [
            "fetch".to_owned(),
            "load".to_owned(),
            "set_restart:no".to_owned(),
            "stop".to_owned(),
            "remove".to_owned(),
            format!("run:{NEW_ID}"),
        ],
        "no drain: nothing is exec'd in the container"
    );
    let events = rig.world.events();
    assert!(events.contains(&format!("get:{API}/readyz")));
    assert!(
        !events.iter().any(|e| e.contains("/healthz")),
        "the service has no version URL, so no version is asked"
    );
    assert_eq!(rig.world.made().unwrap().image_id, NEW_ID);
    assert_eq!(rig.record().pending.unwrap().commit, NEW);

    let confirmed = rig.ask(&format!("confirm js-server={VERSION}"));
    assert!(
        matches!(confirmed, Answer::Confirmed { .. }),
        "{confirmed:?}"
    );
    let record = rig.record();
    assert!(record.pending.is_none());
    assert_eq!(record.current.unwrap().commit, NEW);
    assert!(
        rig.world
            .events()
            .contains(&format!("remove_image:{OLD_ID}"))
    );

    assert!(rig.dir.join("state-js-server.json").exists());
    assert!(!rig.dir.join("state.json").exists());
    assert!(!rig.dir.join("frontend.json").exists());
}

/// The container is made with the run configuration of the descriptor, hardening keys
/// included, spelled out; the same arguments make the container again after a revert.
#[test]
fn the_service_container_is_made_from_its_descriptors_run_configuration() {
    let rig = Rig::new("args");
    let original = rig.world.made().unwrap().args;
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    assert_eq!(
        rig.world.made().unwrap().args,
        [
            "run",
            "--detach",
            "--name",
            "js-server",
            "--restart",
            "unless-stopped",
            "--network",
            "bridge",
            "--user",
            "65532:65532",
            "--group-add",
            "990",
            "--read-only",
            "--tmpfs",
            "/tmp:rw,size=64m",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--publish",
            "127.0.0.1:8080:8080",
            "--volume",
            "/etc/js:/etc/js:ro",
            NEW_ID,
            "--config",
            "/etc/js/server.toml",
        ]
    );
    let reverted = rig.ask("revert js-server");
    assert!(matches!(reverted, Answer::Reverted { .. }), "{reverted:?}");
    assert_eq!(rig.world.runs().last().unwrap(), &original);
}

/// A readiness probe that never answers 2xx puts the old image back, with the same run
/// configuration, and reports the switch failed; the marker is gone and the old release is
/// current again.
#[test]
fn a_service_that_is_not_ready_is_replaced_by_the_old_image() {
    let rig = Rig::new("not-ready");
    let original = rig.world.made().unwrap().args;
    rig.world
        .0
        .borrow_mut()
        .never_ready
        .insert(NEW_ID.to_owned());
    let answer = rig.update();
    assert!(failed(Code::SwitchFailed)(&answer), "{answer:?}");
    let now = rig.world.made().unwrap();
    assert_eq!((now.image_id.as_str(), now.running), (OLD_ID, true));
    assert_eq!(now.args, original);
    assert!(
        rig.world.elapsed().as_secs() >= 90,
        "the whole wait was given"
    );
    let record = rig.record();
    assert!(record.pending.is_none());
    assert_eq!(record.current.unwrap().commit, OLD);
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
}

/// Ready is a status from 200 to 299: 204 passes, and a redirect or a 404 is not ready.
#[test]
fn only_a_2xx_status_is_ready() {
    let rig = Rig::new("status-ok");
    rig.world.ready_status(204);
    assert!(matches!(rig.update(), Answer::Switched { .. }));

    for status in [199, 301, 404, 300] {
        let rig = Rig::new("status-bad");
        rig.world.ready_status(status);
        let answer = rig.update();
        // The old container answers the same status, so it cannot be waited for either.
        assert!(failed(Code::RevertFailed)(&answer), "{status}: {answer:?}");
        assert!(rig.record().pending.is_some(), "{status}");
    }
}

/// A service that exposes a version is not ready until it names the release; one that names
/// another release (or one too short to name any) is replaced by the old image.
#[test]
fn a_version_url_must_name_the_release() {
    let rig = Rig::with("version-ok", DRAIN_EXTRA, WINDOW);
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    assert!(rig.world.events().contains(&format!("get:{API}/version")));

    for lie in ["cccccccccccc", "bbbbbbbb"] {
        let rig = Rig::with("version-lie", DRAIN_EXTRA, WINDOW);
        rig.world.0.borrow_mut().lies_about_commit = Some(lie.to_owned());
        let answer = rig.update();
        assert!(failed(Code::RevertFailed)(&answer), "{lie}: {answer:?}");
        assert!(rig.record().pending.is_some());
    }
}

/// A service with an operator drain is drained first, in its container, and waited for until
/// it ends or the drain deadline is out.
#[test]
fn a_service_with_a_drain_token_is_drained_before_it_is_stopped() {
    let rig = Rig::with("drain", DRAIN_EXTRA, WINDOW);
    rig.world.0.borrow_mut().ends_after = Some(3);
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    assert_eq!(
        rig.steps(),
        [
            "fetch".to_owned(),
            "load".to_owned(),
            "set_restart:no".to_owned(),
            "exec:/usr/bin/kbf cluster drain --frontend --token-file /etc/js/operator.token"
                .to_owned(),
            "stop".to_owned(),
            "remove".to_owned(),
            format!("run:{NEW_ID}"),
        ]
    );
    assert_eq!(rig.world.0.borrow().drain_looks, 3);
}

/// A release on trial that is not confirmed by its deadline is put back by the tick, with the
/// old image and the run configuration of the switch.
#[test]
fn an_unconfirmed_service_release_is_reverted_at_its_deadline() {
    let rig = Rig::new("deadline");
    let original = rig.world.made().unwrap().args;
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    rig.clock.set(1000 + WINDOW - 1);
    assert!(rig.hub().tick().is_empty(), "not yet");
    rig.clock.set(1000 + WINDOW);
    let ticks = rig.hub().tick();
    assert_eq!(ticks.len(), 1);
    assert_eq!(ticks[0].0.as_str(), "js-server");
    assert!(
        matches!(ticks[0].1, Answer::Reverted { .. }),
        "{:?}",
        ticks[0].1
    );
    let now = rig.world.made().unwrap();
    assert_eq!((now.image_id.as_str(), now.args), (OLD_ID, original));
    let record = rig.record();
    assert!(record.pending.is_none());
    assert_eq!(record.current.unwrap().commit, OLD);
}

/// A switch that cannot finish inside the trial window is refused before anything is
/// stopped: the stop and the readiness wait (and the drain, with one) have to fit.
#[test]
fn a_window_too_short_for_the_switch_is_refused() {
    let rig = Rig::with("window", "", 100);
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    let answer = rig.switch();
    assert!(refused(Code::ConfigInvalid)(&answer), "{answer:?}");
    assert!(!rig.steps().contains(&"stop".to_owned()));
}

/// A container service whose descriptor has no probe is not a descriptor: the channels file is
/// `config_invalid` as a whole and nothing runs.
#[test]
fn a_descriptor_without_a_probe_is_config_invalid() {
    let rig = Rig::new("no-probe");
    let without = channels("").replace("ready_url = \"http://127.0.0.1:8080/readyz\"\n", "");
    put(&rig.dir.join("releases.toml"), &without);
    assert!(!without.contains("ready_url"));
    let answer = rig.install();
    assert!(refused(Code::ConfigInvalid)(&answer), "{answer:?}");
    assert!(rig.steps().is_empty());
}

/// What a switch needs of the service is still true when it runs: a service that is not
/// described is refused, and a container that is not there is no current release.
#[test]
fn a_switch_needs_the_container_to_exist() {
    let rig = Rig::new("no-container");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    rig.world.0.borrow_mut().container = None;
    let answer = rig.switch();
    assert!(refused(Code::NoCurrent)(&answer), "{answer:?}");
    let other = rig.ask("query js-other");
    assert!(refused(Code::ServiceNotConfigured)(&other), "{other:?}");
}

/// The container service's record has the shape the frontend's has, plus the description of
/// the service, and the backend's lock is the host's.
#[test]
fn the_record_of_a_service_names_its_probe_and_the_frontends_does_not() {
    let rig = Rig::new("record");
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    let text = fs::read_to_string(rig.dir.join("container-js-server.json")).unwrap();
    assert!(text.contains(r#""service":{"repository":"js-server","ready_path":"/readyz","version_path":null,"drain":false}"#), "{text}");

    // The frontend's record, from the same code, has no such key and is byte for byte what it
    // was.
    let front = super::tests::Rig::new("record-frontend");
    assert!(matches!(front.update(), Answer::Switched { .. }));
    let text = fs::read_to_string(front.dir.join("frontend.json")).unwrap();
    assert!(!text.contains("service"), "{text}");
    assert_eq!(text, FRONTEND_RECORD);

    let backend = rig.backend();
    assert_eq!(backend.lock_file(), rig.dir.join("js-server.lock"));
}

const FRONTEND_RECORD: &str = r#"{"images":{"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb":"sha256:new"},"current":{"commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","image":"sha256:new"},"pending":{"commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","image":"sha256:new","from":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","from_image":"sha256:old","deadline_unix":1600,"config":{"run":{"name":"kbf-frontend","restart":"unless-stopped","network":"bridge","user":"65532:65532","ports":["127.0.0.1:8982:8982","8980:8980"],"mounts":["/etc/kbf:/etc/kbf:ro","/var/lib/kbf:/var/lib/kbf"],"command":["--config","/etc/kbf/kbf.toml"]},"api":"127.0.0.1:8982","drain_token_file":"/etc/kbf/operator.token","peers":[],"drain_secs":180,"ready_secs":90}}}"#;
