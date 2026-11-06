//! What a pinned agent does with the coordinator's orders and says in its heartbeats; what an
//! unpin gives back, and what it does not.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use kbf_types::ReleasePlatform;

use super::flags::Timing;
use super::hold::{Deferral, Fit, Halt, Marks, fit};
use super::local::{LocalError, Trial};
use super::pin::PinFile;
use super::pin_tests::{hold_text, me, put};
use super::run::{Agent, ServicePorts, Stop, World};
use super::status::Status;
use super::testing::{
    COMMIT_A, COMMIT_B, Events, FakeCoordinator, FakeDrain, FakeHealth, FakeLocal, ManualClock, VA,
    VB, host, version,
};
use crate::inventory::Role;
use crate::ports::Health;
use crate::testutil::scratch;
use crate::wire::{Commit, PinReport, Reply, Token};

const NOW: u64 = 10_000;

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
                let unix = AtomicU64::new(NOW);
                move || unix.load(std::sync::atomic::Ordering::SeqCst)
            }),
            renewer: None,
            log: Arc::new(move |line| logs.lock().unwrap().push(line.to_owned())),
        }
    }

    fn hold(&self) -> PathBuf {
        self.dir.join("pin.json")
    }

    fn bare(&self) -> Agent {
        Agent::new(
            host(Role::Daemon),
            "stable".to_owned(),
            Timing::DEFAULT,
            self.world(),
            Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
        )
    }

    /// An agent that honours the hold file in the rig's directory, owned by the test's account.
    fn agent(&self) -> Agent {
        self.with_pin(self.bare())
    }

    fn with_pin(&self, agent: Agent) -> Agent {
        let agent = agent.with_pin(PinFile::new(self.hold(), me(&self.dir)));
        agent.refresh();
        agent
    }

    fn pin(&self, commit: &str) {
        put(&self.hold(), hold_text(commit));
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

    fn logged(&self, line: &str) -> usize {
        self.logs().iter().filter(|l| *l == line).count()
    }

    fn last_pinned(&self) -> Option<PinReport> {
        self.coordinator.beats().pop().expect("a heartbeat").pinned
    }

    /// A heartbeat, then the order its reply holds, the way the two threads do it.
    fn round(&self, agent: &Agent) {
        let order = agent.beat_once().expect("the coordinator orders VB");
        agent.order(order.0.as_deref(), &order.1);
    }
}

fn report(commit: &str) -> Option<PinReport> {
    Some(PinReport {
        commit: Commit::parse(commit),
    })
}

fn no_commit() -> Option<PinReport> {
    Some(PinReport { commit: None })
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
        rig.round(&agent);
        assert_eq!(rig.last_pinned(), report(COMMIT_A));
    }
    assert_eq!(rig.installs(), 0, "nothing was fetched or switched");
    assert_eq!(agent.status().version, version(VA));
    let held = format!("update to {VB}: not run, the host is pinned to {COMMIT_A}");
    assert_eq!(rig.logged(&held), 3);
    assert_eq!(
        rig.logged(&format!("pinned to {COMMIT_A}: no order for kbf runs")),
        1
    );
}

/// Root's hold file whose commit cannot be read, and a hold file that cannot be looked at, hold
/// the host the same: `pinned: {}` in every beat, no order run; once it is gone the agent
/// follows the coordinator again.
///
/// Catches: a malformed pin, or an I/O error on the hold file, that lets the desired version
/// run.
#[test]
fn a_pin_whose_commit_is_unknown_holds_the_host_until_it_is_removed() {
    let rig = Rig::new("pin-unknown");
    rig.desire_vb();
    put(&rig.hold(), "not json");
    let agent = rig.agent();
    rig.round(&agent);
    assert_eq!(rig.last_pinned(), no_commit());
    assert_eq!(rig.installs(), 0);
    assert_eq!(
        rig.logged(&format!("update to {VB}: not run, the host is pinned")),
        1
    );

    put(&rig.dir.join("a-file"), "x");
    let blind = rig
        .bare()
        .with_pin(PinFile::new(rig.dir.join("a-file/pin.json"), me(&rig.dir)));
    blind.refresh();
    rig.round(&blind);
    assert_eq!(rig.last_pinned(), no_commit());
    assert_eq!(rig.installs(), 0);

    rig.unpin();
    rig.round(&agent);
    assert_eq!(rig.last_pinned(), None);
    assert_eq!(rig.installs(), 1);
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
    rig.round(&agent);
    assert_eq!(rig.installs(), 0);

    rig.unpin();
    rig.round(&agent);
    assert_eq!(rig.coordinator.beats()[1].pinned, None);
    assert_eq!(rig.installs(), 1);
    assert_eq!(rig.events.count(&format!("local:switch {VB}")), 1);
    assert_eq!(rig.logged("not pinned: following the coordinator"), 1);
}

/// The file is looked at again before an order runs: a pin written after the heartbeat holds
/// the order its reply carried, and a pin replaced by another is reported with the new commit
/// from the next beat. A hold file that is no pin holds nothing.
///
/// Catches: an order run on the strength of a look taken before the pin was written; a stale
/// commit in the heartbeat; a pin taken from a file another account owns.
#[test]
fn the_pin_is_looked_at_before_each_order_and_each_beat() {
    let rig = Rig::new("pin-late");
    rig.desire_vb();
    let agent = rig.agent();
    let order = agent.beat_once().unwrap();
    assert_eq!(rig.last_pinned(), None);
    rig.pin(COMMIT_B);
    agent.order(None, &order.1);
    assert_eq!(rig.installs(), 0);
    rig.pin(COMMIT_A);
    rig.round(&agent);
    assert_eq!(rig.last_pinned(), report(COMMIT_A));
    assert_eq!(
        rig.logged(&format!("pinned to {COMMIT_A}: no order for kbf runs")),
        1
    );
    assert_eq!(rig.installs(), 0);

    let other = rig
        .bare()
        .with_pin(PinFile::new(rig.hold(), me(&rig.dir) + 1));
    other.refresh();
    rig.round(&other);
    assert_eq!(rig.last_pinned(), None);
    assert_eq!(rig.installs(), 1);
}

/// A pin written while an update runs does not cut it: the release on trial is switched to and
/// confirmed. The pin holds from the next order on, and the next beat says it.
///
/// Catches: a pin that aborts an A/B in the middle, or one that the update in flight forgets.
#[test]
fn an_update_already_running_ends_and_the_pin_holds_the_next_order() {
    let rig = Rig::new("pin-inflight");
    rig.desire_vb();
    let agent = rig.agent();
    let hold = rig.hold();
    rig.local.world().on_install = Some(Arc::new(move || put(&hold, hold_text(COMMIT_A))));
    rig.round(&agent);
    assert_eq!(rig.events.count(&format!("local:switch {VB}")), 1);
    assert_eq!(rig.events.count(&format!("local:confirm {VB}")), 1);
    rig.local.world().on_install = None;
    rig.local.world().state.version = Some(version(VA));
    rig.round(&agent);
    assert_eq!(rig.last_pinned(), report(COMMIT_A));
    assert_eq!(rig.installs(), 1, "the next order is held");
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
/// pinned: then it is left to the helper (whose deadline puts it back), nothing is confirmed,
/// and the host is not reported as updating. Once the helper has put the trial back and the
/// host is unpinned, the next order runs, with no restart of the agent.
///
/// Catches: a trial confirmed over a pin at start; an `updating` that outlives the trial and
/// holds the host's slot for good.
#[test]
fn a_trial_found_at_start_is_left_alone_while_pinned_and_the_host_follows_after_unpin() {
    let rig = Rig::new("pin-resume");
    rig.local.world().state.pending = Some(Trial {
        version: Some(version(VB)),
        commit: COMMIT_B.to_owned(),
        deadline_unix: Some(20_000),
    });
    rig.pin(COMMIT_A);
    rig.desire_vb();
    let agent = rig.agent();
    let stop = Stop::default();
    stop.stop();
    agent.run(&stop);
    assert_eq!(rig.events.count("local:confirm"), 0);
    assert_eq!(
        rig.logged(&format!("update to {VB}: not resumed, the host is pinned")),
        1
    );
    assert_eq!(agent.status().updating, None);
    rig.round(&agent);
    let beat = rig.coordinator.beats().pop().unwrap();
    assert_eq!((beat.updating, beat.pinned), (None, report(COMMIT_A)));

    // The helper's deadline put the release back; root unpins.
    rig.local.world().state.pending = None;
    rig.unpin();
    rig.round(&agent);
    assert_eq!(rig.installs(), 1);
    assert_eq!(rig.events.count(&format!("local:confirm {VB}")), 1);
}

/// Unpin lifts the pin and nothing else: a target that failed a moment ago is still left alone
/// until the retry time, a deferral still declines until its retry time, and a halt still holds
/// the unit until the coordinator clears it. Each is lifted by its own way, as without a pin.
///
/// Catches: an unpin that wipes the agent's other holds and re-drives a release that failed or
/// halted the host.
#[test]
fn unpin_lifts_the_pin_and_leaves_the_other_holds() {
    // A failure a moment ago.
    let rig = Rig::new("pin-after-failure");
    rig.desire_vb();
    rig.local.world().install = Err(LocalError::Unreachable("down".to_owned()));
    let agent = rig.agent();
    rig.round(&agent);
    assert_eq!(rig.installs(), 1);
    rig.pin(COMMIT_A);
    rig.round(&agent);
    rig.unpin();
    rig.round(&agent);
    assert_eq!(rig.installs(), 1);
    assert_eq!(
        rig.logged(&format!(
            "update to {VB}: failed a moment ago; not again yet"
        )),
        1
    );

    // A deferral and a halt, kept on disk, under a coordinator that advertises both.
    for (name, held) in [
        (
            "pin-after-deferral",
            format!("update to {VB}: declined, deferred for 500 s more"),
        ),
        (
            "pin-after-halt",
            format!("update to {VB}: not run, the service is held after a halt (rollback_failed)"),
        ),
    ] {
        let rig = Rig::new(name);
        rig.desire_vb();
        rig.coordinator.advertise(&["deferred-v1", "halt-v1"]);
        let marks = Marks::new(&rig.dir);
        if name.ends_with("deferral") {
            let deferral = Deferral {
                target: version(VB),
                reason: Token::parse("drain_bound").unwrap(),
                retry_after_unix: NOW + 500,
            };
            marks.save("kbf", &deferral).unwrap();
        } else {
            let halt = Halt {
                reason: Token::parse("rollback_failed").unwrap(),
                generation: Some(0),
            };
            marks.save_halt("kbf", &halt).unwrap();
        }
        let agent = rig.with_pin(rig.bare().with_host_updates(rig.dir.clone()));
        rig.pin(COMMIT_A);
        rig.round(&agent);
        rig.unpin();
        rig.round(&agent);
        assert_eq!(rig.installs(), 0, "{name}");
        assert_eq!(rig.logged(&held), 1, "{name}: {:?}", rig.logs());
    }
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
