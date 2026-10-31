//! `Deferred` and `halt` on the agent's side (`docs/design/host-updates.md`, 3.8): what an
//! agent does with and without the features a coordinator advertises, what survives a restart,
//! and what a heartbeat that has grown too large sends. Time is two clocks a test moves: the
//! agent's (the drain wait) and the Unix one (`retry_after`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_types::ReleasePlatform;

use super::coordinator::CoordinatorError;
use super::flags::Timing;
use super::hold::{Deferral, Features, Fit, MAX_RETRY_SECS, Marks, fit};
use super::local::LocalError;
use super::run::{Agent, World};
use super::status::Status;
use super::testing::{
    Events, FakeCoordinator, FakeDrain, FakeHealth, FakeLocal, ManualClock, VA, VB, host, version,
};
use crate::inventory::Role;
use crate::ports::Health;
use crate::testutil::scratch;
use crate::wire::{
    Health as WireHealth, Heartbeat, Outcome, Phase, Reply, ServiceBeat, StepName, StepRun, Token,
    UpdateResult,
};

const T0: u64 = 10_000;
const RETRY: u64 = 900;
const OTHER: &str = "0.1.9+gcccccccccccc-4";

struct Rig {
    events: Events,
    coordinator: FakeCoordinator,
    local: FakeLocal,
    drain: FakeDrain,
    health: FakeHealth,
    clock: Arc<ManualClock>,
    unix: Arc<AtomicU64>,
    logs: Arc<Mutex<Vec<String>>>,
    dir: PathBuf,
}

impl Rig {
    /// A daemon host that runs `VA`, offered `VB`, whose node holds two leases and whose
    /// operator bounded the drain at a minute.
    fn new(name: &str) -> Self {
        let events = Events::default();
        let health = FakeHealth::new(&events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        let drain = FakeDrain::new(&events);
        *drain.leases.lock().unwrap() = [2].into();
        Self {
            coordinator: FakeCoordinator::default(),
            local: FakeLocal::new(&events),
            drain,
            health,
            clock: Arc::new(ManualClock::default()),
            unix: Arc::new(AtomicU64::new(T0)),
            logs: Arc::default(),
            dir: scratch(name),
            events,
        }
    }

    fn timing() -> Timing {
        Timing {
            drain_deadline: Some(Duration::from_secs(60)),
            retry: Duration::from_secs(RETRY),
            ..Timing::DEFAULT
        }
    }

    fn world(&self) -> World {
        let (logs, unix) = (Arc::clone(&self.logs), Arc::clone(&self.unix));
        World {
            coordinator: Arc::new(self.coordinator.clone()),
            local: Arc::new(self.local.clone()),
            drain: Arc::new(self.drain.clone()),
            health: Arc::new(self.health.clone()),
            clock: self.clock.clone(),
            unix_now: Arc::new(move || unix.load(Ordering::SeqCst)),
            renewer: None,
            log: Arc::new(move |line| logs.lock().unwrap().push(line.to_owned())),
        }
    }

    /// An agent that announces nothing: the agent as it was before.
    fn plain(&self) -> Agent {
        let agent = Agent::new(
            host(Role::Daemon),
            "stable".to_owned(),
            Self::timing(),
            self.world(),
            Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
        );
        agent.refresh();
        agent
    }

    /// An agent that announces `deferred-v1` and `halt-v1` and keeps its deferrals in the rig's
    /// directory. A new one on the same rig is the agent after a restart.
    fn announcing(&self) -> Agent {
        self.announcing_in(self.dir.clone())
    }

    fn announcing_in(&self, dir: PathBuf) -> Agent {
        let agent = Agent::new(
            host(Role::Daemon),
            "stable".to_owned(),
            Self::timing(),
            self.world(),
            Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
        )
        .with_host_updates(dir);
        agent.refresh();
        agent
    }

    fn order_is(&self, target: &str) {
        self.coordinator.reply(Ok(Reply::Update {
            version: version(target),
        }));
    }

    /// The heartbeat the agent sent last.
    fn last_beat(&self) -> Heartbeat {
        self.coordinator
            .beats()
            .pop()
            .expect("a heartbeat was sent")
    }

    /// A heartbeat, and then the order its reply holds, the way the two threads do it.
    fn take_order(&self, agent: &Agent) {
        self.order_is(VB);
        let order = agent.beat_once().expect("an order");
        agent.order(order.0.as_deref(), &order.1);
    }

    fn installs(&self) -> usize {
        self.events.count("local:install")
    }

    fn logged(&self, needle: &str) -> bool {
        self.logs.lock().unwrap().iter().any(|l| l.contains(needle))
    }

    fn advance_unix(&self, secs: u64) {
        self.unix.fetch_add(secs, Ordering::SeqCst);
    }

    /// The helper cannot switch, cannot put the previous release back, and the previous release
    /// does not run healthy: the one halt today's update path knows.
    fn cannot_go_back(&self) {
        *self.drain.leases.lock().unwrap() = [0].into();
        let mut world = self.local.world();
        world.switch = Err(LocalError::Refused("nope".to_owned()));
        world.revert = Err(LocalError::Unreachable("gone".to_owned()));
        drop(world);
        self.health
            .say(VA, vec![Ok(Health::Waiting("down".to_owned()))]);
    }
}

fn failed(target: &str) -> Option<UpdateResult> {
    Some(UpdateResult::new(version(target), Outcome::Failed))
}

fn token(text: &str) -> Token {
    Token::parse(text).expect("a token")
}

/// Catches: an agent that announces nothing changing a byte of what it sends or does. It sends
/// no `agent_features`, a drain bound is `Failed` with no phase, class or reason and no
/// `deferred`, and the same order runs again once the retry time is out: nothing is declined
/// for a deferral.
#[test]
fn an_agent_that_announces_nothing_fails_a_drain_bound_as_before() {
    let rig = Rig::new("hold-plain");
    let agent = rig.plain();
    rig.take_order(&agent);
    agent.beat_once();
    let beat = rig.last_beat();
    assert!(beat.agent_features.is_empty());
    assert_eq!(beat.last_update, failed(VB));
    assert_eq!(beat.deferred, None);
    assert_eq!(beat.updating, None);
    assert_eq!(rig.installs(), 1);
    // The failure's own backoff, as ever.
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1);
    rig.clock.advance(Duration::from_secs(RETRY));
    rig.advance_unix(RETRY);
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 2);
}

/// Catches: a deferral reported, or an order declined, because an agent announced. A
/// coordinator that advertises nothing (it knows the field and supports none, or it predates
/// it and says nothing) leaves the agent as it was: `Failed`, no phase, no `deferred`, and the
/// next order after the retry time runs. The beat does announce.
#[test]
fn an_agent_that_sees_no_features_never_defers_or_declines() {
    for advertised in [Some(&[][..]), None] {
        let rig = Rig::new("hold-none");
        match advertised {
            Some(names) => rig.coordinator.advertise(names),
            None => rig.coordinator.advertise_nothing_at_all(),
        }
        let agent = rig.announcing();
        rig.take_order(&agent);
        agent.beat_once();
        let beat = rig.last_beat();
        assert_eq!(beat.agent_features, ["deferred-v1", "halt-v1"]);
        assert_eq!(beat.last_update, failed(VB));
        assert_eq!(beat.deferred, None);
        assert!(!rig.dir.join("defer-kbf").exists());
        rig.clock.advance(Duration::from_secs(RETRY));
        rig.advance_unix(RETRY);
        rig.take_order(&agent);
        assert_eq!(rig.installs(), 2);
    }
}

/// Catches: a halt without `halt-v1`: the helper could not go back and the old release is down,
/// and with no feature advertised that is a plain `Failed` that the agent retries after the
/// retry time, as it always has; or with only `deferred-v1`, the same.
#[test]
fn without_halt_v1_a_failed_rollback_is_a_plain_failure_that_is_retried() {
    for names in [&[][..], &["deferred-v1"][..]] {
        let rig = Rig::new("hold-nohalt");
        rig.coordinator.advertise(names);
        rig.cannot_go_back();
        let agent = rig.announcing();
        rig.take_order(&agent);
        agent.beat_once();
        assert_eq!(rig.last_beat().last_update, failed(VB));
        rig.clock.advance(Duration::from_secs(RETRY));
        rig.take_order(&agent);
        assert_eq!(rig.installs(), 2);
    }
}

/// Catches: a drain bound that is a failure when the coordinator takes deferrals, or one that
/// still holds the host's slot. With `deferred-v1` advertised the bound is a `deferred` of the
/// target with the reason `drain_bound` and the seconds left to `retry_after`, the last result
/// is not a `Failed` of the target and nothing is updating; the seconds count down with the
/// clock and the report ends when the time has come.
#[test]
fn with_deferred_v1_a_drain_bound_reports_deferred_and_holds_no_slot() {
    let rig = Rig::new("hold-deferred");
    rig.coordinator.advertise(&["deferred-v1"]);
    let agent = rig.announcing();
    rig.take_order(&agent);
    agent.beat_once();
    let beat = rig.last_beat();
    let report = beat.deferred.expect("a deferral");
    assert_eq!(report.target, version(VB));
    assert_eq!(report.reason, token("drain_bound"));
    assert_eq!(report.retry_after_secs, RETRY);
    assert_eq!(beat.last_update, None);
    assert_eq!(beat.updating, None);
    assert_eq!(beat.health, WireHealth::Healthy);
    rig.advance_unix(100);
    agent.beat_once();
    assert_eq!(
        rig.last_beat().deferred.unwrap().retry_after_secs,
        RETRY - 100
    );
    rig.advance_unix(RETRY - 100);
    agent.beat_once();
    assert_eq!(rig.last_beat().deferred, None);
}

/// Catches: a deferral that a later failure or success leaves behind in the result: the last
/// result an earlier update had is kept through a deferral.
#[test]
fn a_deferral_leaves_the_last_result_as_it_was() {
    let rig = Rig::new("hold-lastresult");
    rig.coordinator.advertise(&["deferred-v1"]);
    let agent = rig.announcing();
    // A first update that works.
    *rig.drain.leases.lock().unwrap() = [0].into();
    rig.take_order(&agent);
    agent.beat_once();
    assert_eq!(
        rig.last_beat().last_update,
        Some(UpdateResult::new(version(VB), Outcome::Succeeded))
    );
    // A second one that defers.
    *rig.drain.leases.lock().unwrap() = [2].into();
    rig.order_is(OTHER);
    let order = agent.beat_once().unwrap();
    agent.order(None, &order.1);
    agent.beat_once();
    let beat = rig.last_beat();
    assert_eq!(beat.deferred.unwrap().target, version(OTHER));
    assert_eq!(
        beat.last_update,
        Some(UpdateResult::new(version(VB), Outcome::Succeeded))
    );
}

/// Catches: a bound reported as a deferral by an agent whose coordinator advertises only
/// `halt-v1`: the bound is a plain `Failed`.
#[test]
fn a_bound_is_a_failure_when_only_halt_is_advertised() {
    let rig = Rig::new("hold-halt-only");
    rig.coordinator.advertise(&["halt-v1"]);
    let agent = rig.announcing();
    rig.take_order(&agent);
    agent.beat_once();
    let beat = rig.last_beat();
    assert_eq!(beat.last_update, failed(VB));
    assert_eq!(beat.deferred, None);
}

/// Catches: a `retry_after` lost in a restart. The agent that defers is dropped; a new one on
/// the same directory, told by its first reply that deferrals are taken, reports what is left
/// of the time and declines the order that the same reply brings.
#[test]
fn retry_after_survives_a_restart_and_is_honoured() {
    let rig = Rig::new("hold-restart");
    rig.coordinator.advertise(&["deferred-v1"]);
    let agent = rig.announcing();
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1);
    drop(agent);
    assert!(rig.dir.join("defer-kbf").exists());
    rig.advance_unix(300);
    let restarted = rig.announcing();
    rig.take_order(&restarted);
    assert_eq!(rig.installs(), 1, "the order is declined");
    assert!(rig.logged("declined"));
    restarted.beat_once();
    let report = rig.last_beat().deferred.expect("kept through the restart");
    assert_eq!(report.target, version(VB));
    assert_eq!(report.retry_after_secs, RETRY - 300);
}

/// Catches: a re-order declined too long or not at all: one second before `retry_after` it is
/// declined and nothing is fetched; at `retry_after` it runs, the mark on disk is gone, and a
/// second bound defers again from the new time. An order for another target is never declined.
#[test]
fn a_reorder_before_retry_after_is_declined_and_one_after_it_runs() {
    let rig = Rig::new("hold-reorder");
    rig.coordinator.advertise(&["deferred-v1"]);
    let agent = rig.announcing();
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1);
    rig.advance_unix(RETRY - 1);
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1);
    assert!(rig.logged("declined, deferred for 1 s more"));
    rig.advance_unix(1);
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 2);
    agent.beat_once();
    assert_eq!(
        rig.last_beat().deferred.unwrap().retry_after_secs,
        RETRY,
        "a new deferral from the new time"
    );
    // Another target is not the one that was deferred.
    rig.order_is(OTHER);
    let order = agent.beat_once().unwrap();
    agent.order(None, &order.1);
    assert_eq!(rig.installs(), 3);
    let kept = Marks::new(&rig.dir).load("kbf").unwrap().unwrap();
    assert_eq!(kept.target, version(OTHER));
}

/// Catches: an agent that declines on a deferral the coordinator no longer takes: once the
/// reply stops advertising `deferred-v1` the same order runs before `retry_after`.
#[test]
fn a_deferral_is_declined_only_while_it_is_advertised() {
    let rig = Rig::new("hold-withdrawn");
    rig.coordinator.advertise(&["deferred-v1"]);
    let agent = rig.announcing();
    rig.take_order(&agent);
    rig.coordinator.advertise(&[]);
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 2);
    assert!(rig.logged("advertises deferred-v1: false"));
}

/// Catches: the features forgotten when a heartbeat fails: an unreachable coordinator says
/// nothing about what it supports.
#[test]
fn a_failed_heartbeat_keeps_what_was_advertised() {
    let rig = Rig::new("hold-unreachable");
    rig.coordinator.advertise(&["deferred-v1"]);
    let agent = rig.announcing();
    rig.take_order(&agent);
    rig.coordinator
        .reply(Err(CoordinatorError::Unreachable("down".to_owned())));
    assert_eq!(agent.beat_once(), None);
    assert!(rig.logged("heartbeat: the coordinator is unreachable"));
    rig.advance_unix(10);
    rig.order_is(VB);
    agent.order(None, &version(VB));
    assert_eq!(rig.installs(), 1);
}

/// Catches: a deferral file that cannot be used stopping the agent or a deferral that cannot be
/// recorded being lost: a garbage file is ignored and logged and the order runs; a directory
/// that takes no file still declines in memory and says it could not record; a deferral file
/// that cannot be removed when the order runs is logged and the update goes on.
#[test]
fn a_deferral_on_disk_that_cannot_be_used_or_kept_is_logged_and_does_not_stop_updates() {
    let rig = Rig::new("hold-disk");
    rig.coordinator.advertise(&["deferred-v1"]);
    std::fs::write(rig.dir.join("defer-kbf"), "not json").unwrap();
    let agent = rig.announcing();
    assert!(rig.logged("a deferral on disk is ignored"));
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1);
    // The file is now a good one; make its removal impossible.
    std::fs::remove_file(rig.dir.join("defer-kbf")).unwrap();
    std::fs::create_dir(rig.dir.join("defer-kbf")).unwrap();
    rig.advance_unix(RETRY);
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 2);
    assert!(rig.logged("the deferral on disk could not be removed"));

    let rig = Rig::new("hold-nodir");
    rig.coordinator.advertise(&["deferred-v1"]);
    let agent = rig.announcing_in(rig.dir.join("missing"));
    rig.take_order(&agent);
    assert!(rig.logged("the deferral could not be recorded on disk"));
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1, "still declined in memory");
}

/// Catches: a halt that is a plain failure, or one that is retried. With `halt-v1` advertised
/// a rollback the helper cannot do, with the old release down, ends `Failed` with the phase
/// `halt` and the reason `rollback_failed`; the service is held: the same order after the retry
/// time, and any other, is not run; and a halt where the old release is healthy is no halt.
#[test]
fn with_halt_v1_a_failed_rollback_halts_and_holds_the_service() {
    let rig = Rig::new("hold-halt");
    rig.coordinator.advertise(&["halt-v1"]);
    rig.cannot_go_back();
    let agent = rig.announcing();
    rig.take_order(&agent);
    agent.beat_once();
    let beat = rig.last_beat();
    let mut want = UpdateResult::new(version(VB), Outcome::Failed);
    want.phase = Some(Phase::Halt);
    want.reason = Some(token("rollback_failed"));
    assert_eq!(beat.last_update, Some(want));
    rig.clock.advance(Duration::from_secs(RETRY));
    rig.advance_unix(RETRY);
    rig.take_order(&agent);
    rig.order_is(OTHER);
    let order = agent.beat_once().unwrap();
    agent.order(None, &order.1);
    assert_eq!(rig.installs(), 1);
    assert!(rig.logged("held after a halt (rollback_failed)"));

    // The previous release is healthy: the revert failed, but nothing is stuck.
    let rig = Rig::new("hold-halt-healthy");
    rig.coordinator.advertise(&["halt-v1"]);
    rig.cannot_go_back();
    rig.health.say(VA, vec![Ok(Health::Healthy)]);
    let agent = rig.announcing();
    rig.take_order(&agent);
    agent.beat_once();
    assert_eq!(rig.last_beat().last_update, failed(VB));
}

/// Catches: a heartbeat the coordinator cannot read being sent: a beat within the limit goes as
/// it is, one over it by the optional facts alone goes without them and the log says so, and
/// one over it even then is not sent.
#[test]
fn a_heartbeat_over_the_limit_is_trimmed_or_not_sent() {
    let rig = Rig::new("hold-size");
    let agent = rig.announcing();
    agent.beat_once();
    let full = rig.last_beat();
    let mut bare = full.clone();
    bare.agent_features.clear();
    let bytes = |beat: &Heartbeat| serde_json::to_vec(beat).unwrap().len();
    assert!(bytes(&bare) < bytes(&full));

    let trimmed = rig.announcing().with_beat_limit(bytes(&bare));
    trimmed.beat_once();
    assert!(rig.last_beat().agent_features.is_empty());
    assert!(rig.logged("sent without them"));

    let sent = rig.coordinator.beats().len();
    let refused = rig.announcing().with_beat_limit(10);
    assert_eq!(refused.beat_once(), None);
    assert_eq!(rig.coordinator.beats().len(), sent);
    assert!(rig.logged("even without the optional facts; not sent"));
}

fn rich() -> Heartbeat {
    let result = |target: &str| {
        let mut result = UpdateResult::new(version(target), Outcome::Failed);
        result.phase = Some(Phase::Halt);
        result.reason = Some(token("rollback_failed"));
        Some(result)
    };
    let service = ServiceBeat {
        name: "db".to_owned(),
        version: version(VA),
        health: WireHealth::Healthy,
        last_update: result(VB),
        updating: None,
        step: Some(StepRun {
            name: StepName::parse("cutover").unwrap(),
            attempt: 1,
        }),
        commit: None,
        step_secs: [(StepName::parse("prepare").unwrap(), 3)].into(),
        promotion_pending: true,
        deferred: None,
    };
    Heartbeat {
        platform: ReleasePlatform::LinuxX86_64,
        version: version(VA),
        health: WireHealth::Healthy,
        last_update: result(VB),
        updating: None,
        services: Some(vec![service]),
        agent_features: vec!["deferred-v1".to_owned()],
        step: None,
        commit: None,
        step_secs: [(StepName::parse("fetch").unwrap(), 1)].into(),
        promotion_pending: true,
        deferred: None,
    }
}

/// Catches: `fit` dropping what the coordinator needs or keeping what it can do without. Within
/// the limit nothing changes; over it, the announced features, durations, step, pending flag and
/// the phase and reason of a result go, in `kbf` and in every service, and the platform, the
/// versions, the health, the target and outcome of a result and the service's name stay.
#[test]
fn fit_drops_the_optional_facts_and_only_them() {
    let beat = rich();
    let len = serde_json::to_vec(&beat).unwrap().len();
    assert_eq!(fit(beat.clone(), len), Fit::Whole(beat.clone()));
    let Fit::Trimmed(bare) = fit(beat.clone(), len - 1) else {
        panic!("one byte over is trimmed");
    };
    assert!(bare.agent_features.is_empty() && bare.step_secs.is_empty());
    assert!(!bare.promotion_pending && bare.step.is_none());
    assert_eq!(bare.last_update, failed(VB));
    let service = &bare.services.as_ref().unwrap()[0];
    assert_eq!(service.name, "db");
    assert_eq!(service.last_update, failed(VB));
    assert!(service.step.is_none() && service.step_secs.is_empty() && !service.promotion_pending);
    assert_eq!(
        (bare.platform, &bare.version, bare.health),
        (beat.platform, &beat.version, beat.health)
    );
    assert_eq!(fit(beat, 5), Fit::Oversized);
}

/// Catches: a deferral file that is not whole being read, or one not written whole: the file
/// round-trips, an absent one is none, and a file too long, not JSON, or not readable at all
/// is an error that names it; a directory that takes no file fails the save; forgetting what
/// is not there is fine and forgetting what cannot be removed is an error.
#[test]
fn deferrals_on_disk_round_trip_and_bad_files_are_errors() {
    let dir = scratch("hold-marks");
    let marks = Marks::new(&dir);
    assert_eq!(marks.load("kbf"), Ok(None));
    let deferral = Deferral {
        target: version(VB),
        reason: token("drain_bound"),
        retry_after_unix: 5,
    };
    marks.save("kbf", &deferral).unwrap();
    assert_eq!(marks.load("kbf"), Ok(Some(deferral.clone())));
    assert!(marks.load("db").unwrap().is_none(), "one file per unit");
    marks.clear("kbf").unwrap();
    marks.clear("kbf").unwrap();
    assert_eq!(marks.load("kbf"), Ok(None));

    std::fs::write(dir.join("defer-kbf"), "x".repeat(5000)).unwrap();
    assert!(marks.load("kbf").unwrap_err().contains("too long"));
    std::fs::write(dir.join("defer-kbf"), "{}").unwrap();
    assert!(marks.load("kbf").is_err());
    std::fs::remove_file(dir.join("defer-kbf")).unwrap();
    std::fs::create_dir(dir.join("defer-kbf")).unwrap();
    assert!(marks.load("kbf").is_err());
    assert!(marks.clear("kbf").is_err());
    assert!(
        Marks::new(dir.join("missing"))
            .save("kbf", &deferral)
            .is_err()
    );
}

/// Catches: a deferral that reports or declines at its own time: at `retry_after` it is over.
#[test]
fn a_deferral_is_over_at_its_retry_time() {
    let deferral = Deferral {
        target: version(VB),
        reason: token("drain_bound"),
        retry_after_unix: 50,
    };
    assert_eq!(deferral.report(49).unwrap().retry_after_secs, 1);
    assert!(deferral.report(50).is_none());
    assert!(deferral.declines(&version(VB), 49));
    assert!(!deferral.declines(&version(VB), 50));
    assert!(!deferral.declines(&version(VA), 49));
}

/// Catches: a feature taken from a name the agent does not know, or a known one missed.
#[test]
fn features_are_the_known_names_advertised() {
    let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    assert_eq!(Features::of(&names(&[])), Features::default());
    assert_eq!(
        Features::of(&names(&["halt-v1", "future-v9"])),
        Features {
            deferred: false,
            halt: true
        }
    );
    assert_eq!(
        Features::of(&names(&["deferred-v1", "halt-v1"])),
        Features {
            deferred: true,
            halt: true
        }
    );
}

/// Catches: a clamp that drifts from the step runner's 3600 s bound.
#[test]
fn the_retry_bound_is_the_step_runners() {
    assert_eq!(MAX_RETRY_SECS, 3600);
}

/// Catches: a retry hint above the bound that freezes a host. An agent whose retry is two
/// hours reports and persists one hour.
#[test]
fn a_retry_above_the_bound_is_reported_and_persisted_at_the_bound() {
    let rig = Rig::new("hold-clamp-new");
    rig.coordinator.advertise(&["deferred-v1"]);
    let agent = Agent::new(
        host(Role::Daemon),
        "stable".to_owned(),
        Timing {
            retry: Duration::from_secs(2 * MAX_RETRY_SECS),
            ..Rig::timing()
        },
        rig.world(),
        Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
    )
    .with_host_updates(rig.dir.clone());
    agent.refresh();
    rig.take_order(&agent);
    agent.beat_once();
    assert_eq!(
        rig.last_beat().deferred.unwrap().retry_after_secs,
        MAX_RETRY_SECS
    );
    let kept = Marks::new(&rig.dir).load("kbf").unwrap().unwrap();
    assert_eq!(kept.retry_after_unix, T0 + MAX_RETRY_SECS);
}

/// Catches: an older agent's file with a far deadline that keeps a host frozen. It is read as
/// at most now + 3600: reported so, declining until then, and the order runs after.
#[test]
fn a_persisted_deadline_beyond_the_bound_is_read_at_the_bound() {
    let rig = Rig::new("hold-clamp-old");
    rig.coordinator.advertise(&["deferred-v1"]);
    Marks::new(&rig.dir)
        .save(
            "kbf",
            &Deferral {
                target: version(VB),
                reason: token("drain_bound"),
                retry_after_unix: T0 + 100 * MAX_RETRY_SECS,
            },
        )
        .unwrap();
    let agent = rig.announcing();
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 0, "declined");
    agent.beat_once();
    assert_eq!(
        rig.last_beat().deferred.unwrap().retry_after_secs,
        MAX_RETRY_SECS
    );
    rig.advance_unix(MAX_RETRY_SECS);
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1, "the bound has passed");
}

/// An agent that halted on a rollback it could not do, under the clear generation `seen` (the
/// generation of `kbf` in the last reply before the halt; none: no `halts` at all).
fn halted_under(rig: &Rig, seen: Option<u64>) -> Agent {
    rig.coordinator.advertise(&["halt-v1"]);
    if let Some(generation) = seen {
        rig.coordinator.set_halts(&[("kbf", generation)]);
    }
    rig.cannot_go_back();
    let agent = rig.announcing();
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1);
    agent
}

/// Catches: a reply that carries `halts` and is refused whole (the answer reader refuses a key
/// it does not know), or a map that is lost, or a bad one that is taken.
#[test]
fn an_answer_with_halts_is_read() {
    let text = r#"{"reply":"nothing","features":["halt-v1"],"halts":{"kbf":2,"db":1}}"#;
    let answer: crate::wire::Answer = serde_json::from_str(text).expect("read");
    assert_eq!(answer.halts.get("kbf"), Some(&2));
    assert_eq!(answer.halts.get("db"), Some(&1));
    for bad in [
        r#"{"reply":"nothing","halts":{"Kbf":1}}"#,
        r#"{"reply":"nothing","halts":{"kbf":-1}}"#,
        r#"{"reply":"nothing","halts":["kbf"]}"#,
    ] {
        assert!(
            serde_json::from_str::<crate::wire::Answer>(bad).is_err(),
            "{bad}"
        );
    }
    let many: std::collections::BTreeMap<String, u64> =
        (0..18).map(|n| (format!("s{n}"), 1)).collect();
    let over = serde_json::json!({"reply": "nothing", "halts": many}).to_string();
    assert!(serde_json::from_str::<crate::wire::Answer>(&over).is_err());
}

/// Catches: a hold lifted by a reply that carries `halts` and no newer generation: the
/// generation is the one seen before the halt, or the service is not in the map.
#[test]
fn a_reply_with_halts_but_no_newer_generation_keeps_the_hold() {
    let rig = Rig::new("hold-halts-same");
    let agent = halted_under(&rig, Some(3));
    agent.beat_once();
    rig.take_order(&agent);
    assert_eq!(
        rig.installs(),
        1,
        "equal to the generation seen before the halt"
    );
    rig.coordinator.set_halts(&[("kbf", 2), ("db", 9)]);
    rig.take_order(&agent);
    assert_eq!(
        rig.installs(),
        1,
        "older, and another service's is not ours"
    );
    rig.coordinator.set_halts(&[]);
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1, "absent is 0");
    assert!(rig.logged("held after a halt (rollback_failed)"));
}

/// Catches: a hold that is never lifted: a clear (a newer generation) after the halt lifts it,
/// and the next order runs.
#[test]
fn a_newer_generation_lifts_the_hold() {
    let rig = Rig::new("hold-halts-newer");
    let agent = halted_under(&rig, Some(3));
    rig.coordinator.set_halts(&[("kbf", 4)]);
    agent.beat_once();
    assert!(rig.logged("lifted"));
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 2, "the order runs");
}

/// Catches: the race of a halt under a stale generation: an agent that halted having seen no
/// `halts` (generation 0) is not lifted by a reply that says 0, and is by 1.
#[test]
fn a_generation_equal_to_the_one_seen_before_the_halt_does_not_lift() {
    let rig = Rig::new("hold-halts-equal");
    let agent = halted_under(&rig, None);
    rig.coordinator.set_halts(&[("kbf", 0)]);
    agent.beat_once();
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 1);
    rig.coordinator.set_halts(&[("kbf", 1)]);
    agent.beat_once();
    rig.take_order(&agent);
    assert_eq!(rig.installs(), 2);
}

/// Catches: `halts` changing an agent that never halted: its orders run, and nothing is
/// lifted or logged as lifted.
#[test]
fn an_agent_that_never_halted_ignores_halts() {
    let rig = Rig::new("hold-halts-never");
    rig.coordinator.advertise(&["halt-v1"]);
    rig.coordinator.set_halts(&[("kbf", 5)]);
    *rig.drain.leases.lock().unwrap() = [0].into();
    let agent = rig.announcing();
    rig.take_order(&agent);
    agent.beat_once();
    assert_eq!(rig.installs(), 1);
    assert_eq!(
        rig.last_beat().last_update,
        Some(UpdateResult::new(version(VB), Outcome::Succeeded))
    );
    assert!(!rig.logged("lifted"));
    assert!(!rig.dir.join("halt-kbf").exists());
}

/// Catches: a hold, or the generation it was made under, lost in a restart: the restarted
/// agent still holds, is not lifted by the generation it halted under, and is by a newer one.
#[test]
fn the_hold_survives_a_restart_with_its_generation() {
    let rig = Rig::new("hold-halts-restart");
    drop(halted_under(&rig, Some(3)));
    let restarted = rig.announcing();
    rig.take_order(&restarted);
    assert_eq!(rig.installs(), 1, "still held");
    rig.coordinator.set_halts(&[("kbf", 4)]);
    restarted.beat_once();
    rig.take_order(&restarted);
    assert_eq!(rig.installs(), 2, "lifted by a clear after the halt");
}

/// Catches: a halt file that does not round-trip, or a bad one that is taken: an unreadable
/// one is an error that the agent logs and goes on with.
#[test]
fn halts_on_disk_round_trip_and_bad_files_are_errors() {
    use super::hold::Halt;
    let dir = scratch("hold-halt-marks");
    let marks = Marks::new(&dir);
    assert_eq!(marks.load_halt("kbf"), Ok(None));
    let halt = Halt {
        reason: token("rollback_failed"),
        generation: 7,
    };
    marks.save_halt("kbf", &halt).unwrap();
    assert_eq!(marks.load_halt("kbf"), Ok(Some(halt.clone())));
    marks.clear_halt("kbf").unwrap();
    marks.clear_halt("kbf").unwrap();
    assert_eq!(marks.load_halt("kbf"), Ok(None));
    std::fs::write(dir.join("halt-kbf"), "{}").unwrap();
    assert!(marks.load_halt("kbf").is_err());
    std::fs::remove_file(dir.join("halt-kbf")).unwrap();
    std::fs::create_dir(dir.join("halt-kbf")).unwrap();
    assert!(marks.clear_halt("kbf").is_err());
    assert!(
        Marks::new(dir.join("missing"))
            .save_halt("kbf", &halt)
            .is_err()
    );

    let rig = Rig::new("hold-halt-bad-file");
    std::fs::write(rig.dir.join("halt-kbf"), "not json").unwrap();
    let _agent = rig.announcing();
    assert!(rig.logged("a halt on disk is ignored"));
}
