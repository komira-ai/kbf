//! The pin on the agent's side: which file is a pin, and what a pinned agent does with the
//! coordinator's orders and says in its heartbeats.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use kbf_types::ReleasePlatform;

use super::flags::Timing;
use super::hold::{Fit, fit};
use super::local::Trial;
use super::pin::{Look, PIN_FILE, PinFile, ROOT};
use super::run::{Agent, ServicePorts, Stop, World};
use super::status::Status;
use super::testing::{
    COMMIT_A, COMMIT_B, Events, FakeCoordinator, FakeDrain, FakeHealth, FakeLocal, ManualClock, VA,
    VB, host, version,
};
use crate::inventory::Role;
use crate::ports::Health;
use crate::testutil::scratch;
use crate::wire::{Commit, PinReport, Reply};

/// The uid the test's own files have: the owner a pin must have in these tests.
fn me(dir: &Path) -> u32 {
    fs::metadata(dir).unwrap().uid()
}

fn pinned_to(commit: &str) -> Look {
    Look::Pinned {
        report: PinReport {
            commit: Commit::parse(commit),
        },
        problem: None,
    }
}

fn hold_text(commit: &str) -> String {
    format!("{{\"commit\":\"{commit}\",\"version\":null,\"at_unix\":5,\"by\":\"activate\"}}")
}

/// Catches: a pin read from a file the agent's own account could have put there (a link,
/// another owner's file, a directory, a socket); a root's file that is not read whole being
/// taken for "not pinned"; a pin whose commit is lost.
#[test]
fn the_hold_file_is_a_pin_only_when_it_is_the_owners_regular_file() {
    let dir = scratch("pin-look");
    let uid = me(&dir);
    let path = dir.join(PIN_FILE);
    let pin = PinFile::new(&path, uid);
    let shown = path.display().to_string();
    assert_eq!(pin.look(), Look::Absent);

    fs::write(&path, hold_text(COMMIT_B)).unwrap();
    assert_eq!(pin.look(), pinned_to(COMMIT_B));
    assert_eq!(
        pin.look().line(),
        format!("pinned to {COMMIT_B}: no order for kbf runs")
    );
    assert_eq!(
        PinFile::new(&path, uid + 1).look(),
        Look::NotAPin(format!("{shown} is owned by uid {uid}, not {}", uid + 1))
    );

    let unknown = |problem: String| Look::Pinned {
        report: PinReport { commit: None },
        problem: Some(problem),
    };
    fs::write(&path, "x".repeat(4097)).unwrap();
    assert_eq!(pin.look(), unknown(format!("{shown}: it is too long")));
    fs::write(&path, "{\"commit\":\"BBBB\"}").unwrap();
    let look = pin.look();
    assert_eq!(
        look,
        unknown(format!("{shown}: its commit is not 40 hex digits"))
    );
    assert_eq!(
        look.line(),
        format!(
            "pinned, the commit unknown ({shown}: its commit is not 40 hex digits): no order for \
             kbf runs"
        )
    );
    fs::write(&path, "not json").unwrap();
    assert!(matches!(
        pin.look(),
        Look::Pinned { report: PinReport { commit: None }, problem: Some(p) }
            if p.starts_with(&format!("{shown}: expected"))
    ));

    fs::remove_file(&path).unwrap();
    fs::write(dir.join("elsewhere"), hold_text(COMMIT_B)).unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere"), &path).unwrap();
    let look = pin.look();
    assert_eq!(look, Look::NotAPin(format!("{shown} is a link")));
    assert_eq!(
        look.line(),
        format!("the hold file is not a pin: {shown} is a link")
    );
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(pin.look(), Look::NotAPin(format!("{shown} is a directory")));
    fs::remove_dir(&path).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
    assert_eq!(
        pin.look(),
        Look::NotAPin(format!("{shown} is not a regular file"))
    );
    fs::remove_file(&path).unwrap();

    // Not readable by this account: the owner's file pins all the same, with no commit. (Root
    // reads it whatever its mode, and finds the commit.)
    fs::write(&path, hold_text(COMMIT_B)).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let expected = if fs::File::open(&path).is_ok() {
        pinned_to(COMMIT_B)
    } else {
        unknown(format!("{shown}: Permission denied (os error 13)"))
    };
    assert_eq!(pin.look(), expected);
    assert_eq!(Look::Absent.line(), "not pinned: following the coordinator");
    assert_eq!(
        Look::Pinned {
            report: PinReport { commit: None },
            problem: None
        }
        .line(),
        "pinned: no order for kbf runs"
    );
    // The production form: `pin.json` in the directory, a pin only when root owns it.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let expected = if uid == ROOT {
        pinned_to(COMMIT_B)
    } else {
        Look::NotAPin(format!("{shown} is owned by uid {uid}, not 0"))
    };
    assert_eq!(PinFile::in_dir(&dir).look(), expected);
}

struct Rig {
    events: Events,
    coordinator: FakeCoordinator,
    local: FakeLocal,
    logs: Arc<Mutex<Vec<String>>>,
    dir: PathBuf,
}

impl Rig {
    fn new(name: &str) -> Self {
        let events = Events::default();
        Self {
            coordinator: FakeCoordinator::default(),
            local: FakeLocal::new(&events),
            logs: Arc::default(),
            dir: scratch(name),
            events,
        }
    }

    fn health(&self) -> FakeHealth {
        let health = FakeHealth::new(&self.events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        health
    }

    fn world(&self) -> World {
        let logs = Arc::clone(&self.logs);
        World {
            coordinator: Arc::new(self.coordinator.clone()),
            local: Arc::new(self.local.clone()),
            drain: Arc::new(FakeDrain::new(&self.events)),
            health: Arc::new(self.health()),
            clock: Arc::new(ManualClock::default()),
            unix_now: Arc::new({
                let unix = AtomicU64::new(10_000);
                move || unix.load(std::sync::atomic::Ordering::SeqCst)
            }),
            renewer: None,
            log: Arc::new(move |line| logs.lock().unwrap().push(line.to_owned())),
        }
    }

    fn hold(&self) -> PathBuf {
        self.dir.join(PIN_FILE)
    }

    /// An agent that honours the hold file in the rig's directory, owned by the test's account.
    fn agent(&self) -> Agent {
        let agent = Agent::new(
            host(Role::Daemon),
            "stable".to_owned(),
            Timing::DEFAULT,
            self.world(),
            Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
        )
        .with_pin(PinFile::new(self.hold(), me(&self.dir)));
        agent.refresh();
        agent
    }

    fn pin(&self, commit: &str) {
        fs::write(self.hold(), hold_text(commit)).unwrap();
    }

    fn unpin(&self) {
        fs::remove_file(self.hold()).unwrap();
    }

    /// The coordinator desires `VB`: every reply orders it.
    fn desire_vb(&self) {
        self.coordinator.reply(Ok(Reply::Update {
            version: version(VB),
        }));
    }

    fn installs(&self) -> usize {
        self.events.count("local:install")
    }

    fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().clone()
    }

    fn last_pinned(&self) -> Option<PinReport> {
        self.coordinator.beats().pop().expect("a heartbeat").pinned
    }
}

fn report(commit: &str) -> Option<PinReport> {
    Some(PinReport {
        commit: Commit::parse(commit),
    })
}

/// A pinned host is not re-driven: while the coordinator desires another version, every
/// heartbeat says `pinned` with the commit, and the order its reply holds runs nothing, beat
/// after beat. The change is logged once.
///
/// Catches: an agent that re-drives the desired version over an operator's pin; a pin missing
/// from a heartbeat.
#[test]
fn a_pinned_host_is_not_redriven_by_the_next_heartbeat() {
    let rig = Rig::new("pin-held");
    rig.pin(COMMIT_A);
    rig.desire_vb();
    let agent = rig.agent();
    for _ in 0..3 {
        let order = agent.beat_once().expect("the coordinator orders VB");
        assert_eq!(rig.last_pinned(), report(COMMIT_A));
        agent.order(order.0.as_deref(), &order.1);
    }
    assert_eq!(rig.installs(), 0, "nothing was fetched or switched");
    assert_eq!(agent.status().version, version(VA));
    let logs = rig.logs();
    let held = format!("update to {VB}: not run, the host is pinned to {COMMIT_A}");
    assert_eq!(logs.iter().filter(|l| **l == held).count(), 3);
    let seen = format!("pinned to {COMMIT_A}: no order for kbf runs");
    assert_eq!(logs.iter().filter(|l| **l == seen).count(), 1);
}

/// `kbf-host-install unpin` removes the hold file: the next heartbeat carries no pin, and the
/// order the reply holds runs (the release is fetched and switched to).
///
/// Catches: an agent that stays pinned after the file is gone, or remembers the pin.
#[test]
fn unpin_restores_following_desired() {
    let rig = Rig::new("pin-unpin");
    rig.pin(COMMIT_A);
    rig.desire_vb();
    let agent = rig.agent();
    let order = agent.beat_once().unwrap();
    agent.order(None, &order.1);
    assert_eq!(rig.installs(), 0);

    rig.unpin();
    let order = agent.beat_once().unwrap();
    assert_eq!(rig.last_pinned(), None);
    agent.order(order.0.as_deref(), &order.1);
    assert_eq!(rig.installs(), 1);
    assert_eq!(rig.events.count(&format!("local:switch {VB}")), 1);
    assert!(
        rig.logs()
            .contains(&"not pinned: following the coordinator".to_owned())
    );
}

/// The file is looked at again before an order runs: a pin written after the heartbeat holds
/// the order its reply carried. A hold file that is no pin holds nothing.
///
/// Catches: an order run on the strength of a look taken before the pin was written; a pin
/// taken from a file another account owns.
#[test]
fn a_pin_written_after_the_heartbeat_holds_its_order_and_a_file_that_is_no_pin_does_not() {
    let rig = Rig::new("pin-late");
    rig.desire_vb();
    let agent = rig.agent();
    let order = agent.beat_once().unwrap();
    assert_eq!(rig.last_pinned(), None);
    rig.pin(COMMIT_B);
    agent.order(None, &order.1);
    assert_eq!(rig.installs(), 0);

    rig.unpin();
    let other = Agent::new(
        host(Role::Daemon),
        "stable".to_owned(),
        Timing::DEFAULT,
        rig.world(),
        Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
    )
    .with_pin(PinFile::new(rig.hold(), me(&rig.dir) + 1));
    other.refresh();
    rig.pin(COMMIT_B);
    let order = other.beat_once().unwrap();
    assert_eq!(rig.last_pinned(), None);
    other.order(None, &order.1);
    assert_eq!(rig.installs(), 1);
}

/// The pin names a `kbf` release: an order for another service the host runs still runs.
///
/// Catches: a pin that stops the services' updates, which the hold file does not name.
#[test]
fn the_pin_holds_kbf_and_not_the_other_services() {
    let rig = Rig::new("pin-service");
    rig.pin(COMMIT_A);
    let db_events = Events::default();
    let db = FakeLocal::new(&db_events);
    let agent = rig.agent().with_services(vec![ServicePorts {
        name: "db".to_owned(),
        channel: "db-ch".to_owned(),
        local: Arc::new(db),
        drain: Arc::new(FakeDrain::new(&db_events)),
        health: Arc::new(rig.health()),
    }]);
    agent.order(None, &version(VB));
    assert_eq!(rig.installs(), 0);
    agent.order(Some("db"), &version(VB));
    assert_eq!(db_events.count(&format!("local:install {VB} db-ch")), 1);
}

/// An agent restarted in the middle of an A/B finishes the trial first, unless the host is
/// pinned: then it is left to the helper (whose deadline puts it back), and nothing is
/// confirmed.
///
/// Catches: a trial confirmed over a pin at start.
#[test]
fn a_trial_found_at_start_is_not_resumed_while_pinned() {
    let rig = Rig::new("pin-resume");
    rig.local.world().state.pending = Some(Trial {
        version: Some(version(VB)),
        commit: COMMIT_B.to_owned(),
        deadline_unix: Some(20_000),
    });
    rig.pin(COMMIT_A);
    let agent = rig.agent();
    let stop = Stop::default();
    stop.stop();
    agent.run(&stop);
    assert_eq!(rig.events.count("local:confirm"), 0);
    assert!(
        rig.logs()
            .contains(&format!("update to {VB}: not resumed, the host is pinned"))
    );
}

/// The pin is kept when a heartbeat is trimmed to fit: without it the coordinator would count
/// the host as one to order.
///
/// Catches: `fit` dropping `pinned` with the optional facts.
#[test]
fn a_trimmed_heartbeat_keeps_the_pin() {
    let mut beat = Status::unknown(ReleasePlatform::LinuxX86_64, version(VA)).heartbeat();
    beat.agent_features = vec!["deferred-v1".to_owned(), "halt-v1".to_owned()];
    beat.pinned = report(COMMIT_A);
    let len = serde_json::to_vec(&beat).unwrap().len();
    let Fit::Trimmed(bare) = fit(beat, len - 1) else {
        panic!("one byte over is trimmed");
    };
    assert!(bare.agent_features.is_empty());
    assert_eq!(bare.pinned, report(COMMIT_A));
}
