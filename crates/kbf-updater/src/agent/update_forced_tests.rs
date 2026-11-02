//! An operator's force on the agent's side (`docs/design/gamma-cicd.md`, C1): when an order is
//! a force, what the agent does with the helper, the node and the health check, and when an
//! answer's `forced` is not taken as one. Time is a clock a test moves.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_types::ReleasePlatform;

use super::flags::Timing;
use super::local::{Kept, LocalError};
use super::run::{Agent, World};
use super::status::Status;
use super::testing::{
    COMMIT_A, COMMIT_B, Events, FakeCoordinator, FakeDrain, FakeHealth, FakeLocal, ManualClock, VA,
    VB, host, version,
};
use crate::inventory::Role;
use crate::ports::Health;
use crate::testutil::scratch;
use crate::wire::{Health as WireHealth, Outcome, Reply, UpdateResult};

pub(super) const T0: u64 = 10_000;
pub(super) const RETRY: u64 = 900;
pub(super) const ALL: [&str; 3] = ["deferred-v1", "halt-v1", "forced-v1"];

pub(super) struct Rig {
    pub(super) events: Events,
    pub(super) coordinator: FakeCoordinator,
    pub(super) local: FakeLocal,
    pub(super) drain: FakeDrain,
    pub(super) health: FakeHealth,
    pub(super) clock: Arc<ManualClock>,
    pub(super) logs: Arc<Mutex<Vec<String>>>,
    /// The Unix clock the agent reads.
    pub(super) unix: Arc<AtomicU64>,
    pub(super) dir: PathBuf,
}

impl Rig {
    /// A daemon host that runs `VA`, healthy, offered `VB`, whose node holds no lease. The
    /// coordinator advertises every feature and its answers are forced.
    pub(super) fn new(name: &str) -> Self {
        let events = Events::default();
        let health = FakeHealth::new(&events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        let coordinator = FakeCoordinator::default();
        coordinator.advertise(&ALL);
        coordinator.set_forced(true);
        coordinator.reply(Ok(Reply::Update {
            version: version(VB),
        }));
        let local = FakeLocal::new(&events);
        local.world().before = Some(version(VA));
        Self {
            coordinator,
            local,
            drain: FakeDrain::new(&events),
            health,
            clock: Arc::new(ManualClock::default()),
            logs: Arc::default(),
            unix: Arc::new(AtomicU64::new(T0)),
            dir: scratch(name),
            events,
        }
    }

    pub(super) fn timing() -> Timing {
        Timing {
            drain_deadline: Some(Duration::from_secs(60)),
            retry: Duration::from_secs(RETRY),
            ..Timing::DEFAULT
        }
    }

    pub(super) fn agent(&self) -> Agent {
        let (logs, unix) = (Arc::clone(&self.logs), Arc::clone(&self.unix));
        let agent = Agent::new(
            host(Role::Daemon),
            "stable".to_owned(),
            Self::timing(),
            World {
                coordinator: Arc::new(self.coordinator.clone()),
                local: Arc::new(self.local.clone()),
                drain: Arc::new(self.drain.clone()),
                health: Arc::new(self.health.clone()),
                clock: self.clock.clone(),
                unix_now: Arc::new(move || unix.load(Ordering::SeqCst)),
                renewer: None,
                log: Arc::new(move |line| logs.lock().unwrap().push(line.to_owned())),
            },
            Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
        )
        .with_host_updates(self.dir.clone());
        agent.refresh();
        agent
    }

    /// The helper keeps `VB` (and `VA`) among its releases.
    pub(super) fn keeps_vb(&self) {
        self.local.world().state.releases = Some(vec![
            Kept {
                commit: COMMIT_B.to_owned(),
                version: Some(version(VB)),
            },
            Kept {
                commit: COMMIT_A.to_owned(),
                version: Some(version(VA)),
            },
        ]);
    }

    /// A heartbeat, and the order its answer holds, run the way the two threads do it.
    pub(super) fn take_order(&self, agent: &Agent) {
        let order = agent.beat_once().expect("an order");
        agent.order(order.0.as_deref(), &order.1, order.2);
    }

    pub(super) fn steps(&self) -> Vec<String> {
        self.events.of(&[
            "local:install",
            "drain",
            "local:activate",
            "local:switch",
            "local:confirm",
            "local:revert",
            "undrain",
        ])
    }

    pub(super) fn logged(&self, needle: &str) -> bool {
        self.logs.lock().unwrap().iter().any(|l| l.contains(needle))
    }

    pub(super) fn last_update(&self, agent: &Agent) -> Option<UpdateResult> {
        agent.status().last_update
    }
}

pub(super) fn result(target: &str, outcome: Outcome) -> Option<UpdateResult> {
    let mut result = UpdateResult::new(version(target), outcome);
    result.started_unix = Some(T0);
    Some(result)
}

pub(super) fn not_healthy() -> Vec<Result<Health, crate::ports::PortError>> {
    vec![Ok(Health::Waiting("starting".to_owned()))]
}

/// Catches: an agent that does not announce `forced-v1` (the coordinator then never sends it a
/// force), or one that reads the coordinator's `features` for it wrongly: the answer's `forced`
/// makes the order a force only while the last answer advertised the feature.
#[test]
fn the_agent_announces_forced_v1_and_takes_the_flag_only_when_it_is_advertised() {
    let rig = Rig::new("forced-announce");
    let agent = rig.agent();
    let order = agent.beat_once().expect("an order");
    assert_eq!(rig.coordinator.beats()[0].agent_features, ALL);
    assert_eq!(order, (None, version(VB), true));
    assert!(rig.logged("forced-v1: true"));

    for advertised in [&["deferred-v1", "halt-v1"][..], &[][..]] {
        rig.coordinator.advertise(advertised);
        assert_eq!(agent.beat_once(), Some((None, version(VB), false)));
    }
    rig.coordinator.advertise_nothing_at_all();
    assert_eq!(agent.beat_once(), Some((None, version(VB), false)));

    rig.coordinator.advertise(&ALL);
    rig.coordinator.set_forced(false);
    assert_eq!(agent.beat_once(), Some((None, version(VB), false)));
}

/// Catches: a force that fetches again what the host keeps, puts the target on trial, or goes
/// back from it. The helper lists `VB` among what it keeps, so: drain, activate, undrain, and
/// no install, switch, confirm or revert.
#[test]
fn a_forced_order_for_a_kept_release_drains_activates_and_gives_the_node_back() {
    let rig = Rig::new("forced-kept");
    rig.keeps_vb();
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(
        rig.steps(),
        [
            "drain n1 60",
            "local:activate 0.1.7+gbbbbbbbbbbbb-3",
            "undrain"
        ]
    );
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
    assert!(!rig.drain.is_drained());
    assert_eq!(agent.status().version, version(VB));
}

/// Catches: a force that reverts a target that does not run healthy, gives the node back to a
/// release that is not healthy, or reports the force as failed because of the health. Whether
/// the target is waiting or abandoned, the host keeps it, the result is a success, the node
/// stays drained, and the next heartbeat says it is not healthy.
#[test]
fn a_forced_target_that_is_not_healthy_stays_and_the_node_stays_drained() {
    for (name, seen) in [
        ("forced-waiting", Health::Waiting("down".to_owned())),
        ("forced-abandoned", Health::Abandoned("crashes".to_owned())),
    ] {
        let rig = Rig::new(name);
        rig.keeps_vb();
        rig.health.say(VB, vec![Ok(seen)]);
        let agent = rig.agent();
        rig.take_order(&agent);
        assert_eq!(
            rig.steps(),
            ["drain n1 60", "local:activate 0.1.7+gbbbbbbbbbbbb-3"],
            "no revert, no undrain"
        );
        assert!(rig.drain.is_drained());
        assert!(rig.logged("runs but is not healthy"));
        assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
        agent.beat_once();
        let beat = rig.coordinator.beats().pop().unwrap();
        assert_eq!(beat.health, WireHealth::Unhealthy);
        assert_eq!(beat.version, version(VB));
        assert_eq!(beat.last_update, result(VB, Outcome::Succeeded));
    }
}

/// Catches: a force that activates a release the host does not keep, or one that trusts a
/// helper that lists nothing. The target is fetched first (install switches nothing), then
/// drained and activated; a list without the target, and no list (an older helper), both fetch.
#[test]
fn a_forced_order_for_a_release_the_host_does_not_keep_installs_first() {
    for (name, releases) in [
        ("forced-no-list", None),
        (
            "forced-other-list",
            Some(vec![Kept {
                commit: COMMIT_A.to_owned(),
                version: Some(version(VA)),
            }]),
        ),
    ] {
        let rig = Rig::new(name);
        rig.local.world().state.releases = releases;
        let agent = rig.agent();
        rig.take_order(&agent);
        assert_eq!(
            rig.steps(),
            [
                "local:install 0.1.7+gbbbbbbbbbbbb-3 stable",
                "drain n1 60",
                "local:activate 0.1.7+gbbbbbbbbbbbb-3",
                "undrain"
            ]
        );
        assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
    }
}

/// Catches: a kept release known by its commit alone (no version text) not matching the
/// commit prefix its version names, so a kept target is fetched again; and a different commit
/// matching.
#[test]
fn a_kept_release_is_matched_by_the_commit_its_version_names() {
    let rig = Rig::new("forced-by-commit");
    rig.local.world().state.releases = Some(vec![Kept {
        commit: COMMIT_B.to_owned(),
        version: None,
    }]);
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:install"), 0);
    assert_eq!(rig.events.count("local:activate"), 1);

    let rig = Rig::new("forced-by-other-commit");
    rig.local.world().state.releases = Some(vec![Kept {
        commit: COMMIT_A.to_owned(),
        version: None,
    }]);
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:install"), 1);
}

/// Catches: a force that drains and restarts a host that already runs the target, with nothing
/// pending: it is a success at once, with no install, drain or activate.
#[test]
fn a_forced_order_for_the_release_that_runs_is_done_at_once() {
    let rig = Rig::new("forced-running");
    {
        let mut world = rig.local.world();
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
    }
    let agent = rig.agent();
    rig.take_order(&agent);
    assert!(rig.steps().is_empty());
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: a fetch that finds the host already on the target after the install is a force
/// that drains for nothing: `current` with nothing pending is a success with no drain.
#[test]
fn a_forced_order_whose_install_finds_the_target_running_is_done() {
    let rig = Rig::new("forced-current");
    rig.local.world().install = Ok(super::local::Staged {
        commit: COMMIT_B.to_owned(),
        current: true,
    });
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.steps(), ["local:install 0.1.7+gbbbbbbbbbbbb-3 stable"]);
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: a force that drains the node after a fetch that failed (the node is touched only
/// once the release is on the host), or one that reports a refusal of the fetch as a success.
#[test]
fn a_forced_order_whose_install_fails_fails_with_the_node_untouched() {
    let rig = Rig::new("forced-install-fails");
    rig.local.world().install = Err(LocalError::Refused("verify_failed".to_owned()));
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.steps(), ["local:install 0.1.7+gbbbbbbbbbbbb-3 stable"]);
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
    assert!(rig.logged("the host helper refused: verify_failed"));
}

/// Catches: a force that runs when the health check refuses the host, or when the helper does
/// not answer: both are a failure with nothing done to the node.
#[test]
fn a_forced_order_fails_when_the_host_refuses_or_the_helper_is_silent() {
    let rig = Rig::new("forced-refusal");
    let agent = rig.agent();
    *rig.health.refuses.lock().unwrap() = Some("not a daemon host".to_owned());
    rig.take_order(&agent);
    assert!(rig.steps().is_empty());
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));

    let rig = Rig::new("forced-silent");
    let agent = rig.agent();
    rig.local.world().query = Err(LocalError::Unreachable("gone".to_owned()));
    rig.take_order(&agent);
    assert!(rig.steps().is_empty());
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
}

/// Catches: a force that cuts in-flight work or that fails when the operator's drain bound
/// passes: the bound is a deferral as for any update, nothing is activated, and the node stays
/// drained with its leases running on.
#[test]
fn a_forced_order_waits_for_leases_and_a_drain_bound_is_a_deferral() {
    let rig = Rig::new("forced-bound");
    rig.keeps_vb();
    *rig.drain.leases.lock().unwrap() = [2].into();
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:activate"), 0);
    assert!(rig.drain.is_drained());
    agent.beat_once();
    let beat = rig.coordinator.beats().pop().unwrap();
    assert_eq!(beat.deferred.unwrap().reason.as_str(), "drain_bound");
}

/// Catches: a force that puts an older helper's target on trial and then waits for health while
/// the helper's deadline runs (the trial would revert an unhealthy target): with `bad_command`
/// the agent switches and confirms back to back, with no health look between them, and does
/// not revert even though the target is not healthy; the node stays drained.
#[test]
fn an_older_helper_gets_a_switch_and_an_immediate_confirm_and_no_revert() {
    let rig = Rig::new("forced-older");
    rig.local.world().activate = Err(LocalError::Refused("bad_command".to_owned()));
    rig.health.say(VB, not_healthy());
    let agent = rig.agent();
    rig.take_order(&agent);
    let all = rig.events.all();
    let switched = all
        .iter()
        .position(|e| e.starts_with("local:switch"))
        .unwrap();
    assert_eq!(all[switched + 1], "local:confirm 0.1.7+gbbbbbbbbbbbb-3");
    assert_eq!(
        rig.steps(),
        [
            "local:install 0.1.7+gbbbbbbbbbbbb-3 stable",
            "drain n1 60",
            "local:activate 0.1.7+gbbbbbbbbbbbb-3",
            "local:switch 0.1.7+gbbbbbbbbbbbb-3",
            "local:confirm 0.1.7+gbbbbbbbbbbbb-3",
        ],
        "no revert and no undrain"
    );
    assert!(rig.drain.is_drained());
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: an older helper's force that never gives the node back: a target that is healthy
/// after the switch and confirm gets it back, as a kept release does.
#[test]
fn an_older_helper_with_a_healthy_target_gives_the_node_back() {
    let rig = Rig::new("forced-older-healthy");
    rig.local.world().activate = Err(LocalError::Refused("bad_command".to_owned()));
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.steps().last().map(String::as_str), Some("undrain"));
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: an older helper's force that reports success after a switch or a confirm that
/// failed: the helper is asked to go back, the node is given back on the old release, and the
/// result is a failure.
#[test]
fn an_older_helper_whose_switch_or_confirm_fails_goes_back_and_fails() {
    for (name, switch_fails) in [
        ("forced-older-switch", true),
        ("forced-older-confirm", false),
    ] {
        let rig = Rig::new(name);
        {
            let mut world = rig.local.world();
            world.activate = Err(LocalError::Refused("bad_command".to_owned()));
            if switch_fails {
                world.switch = Err(LocalError::Failed("switch_failed".to_owned()));
            } else {
                world.confirm = Err(LocalError::Failed("state_unavailable".to_owned()));
            }
        }
        let agent = rig.agent();
        rig.take_order(&agent);
        assert_eq!(rig.events.count("local:revert"), 1);
        assert_eq!(rig.steps().last().map(String::as_str), Some("undrain"));
        assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
    }
}

/// Catches: an activation that failed taken for a force that worked, or a node given back to
/// a release that is not healthy: the helper is asked to go back (`nothing_pending` is fine),
/// the result is a failure, and the node is given back only if the release that runs is healthy.
#[test]
fn a_failed_activation_is_a_failure_and_the_node_is_given_back_only_if_healthy() {
    for (name, error) in [
        (
            "forced-restart-failed",
            LocalError::Failed("restart_failed".to_owned()),
        ),
        (
            "forced-not-kept",
            LocalError::Refused("not_kept".to_owned()),
        ),
    ] {
        let rig = Rig::new(name);
        rig.keeps_vb();
        rig.local.world().activate = Err(error);
        let agent = rig.agent();
        rig.take_order(&agent);
        assert_eq!(rig.events.count("local:switch"), 0);
        assert_eq!(rig.steps().last().map(String::as_str), Some("undrain"));
        assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
        assert!(rig.logged("the activation failed"));
    }

    let rig = Rig::new("forced-restart-failed-down");
    rig.keeps_vb();
    rig.local.world().activate = Err(LocalError::Failed("restart_failed".to_owned()));
    rig.health.say(VA, not_healthy());
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("undrain"), 0);
    assert!(rig.drain.is_drained());
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
}

/// Catches: an answer of `forced: true` from a coordinator that did not advertise `forced-v1`
/// run as a force: it is an ordinary order, with the trial, and the target that is not healthy
/// is reverted.
#[test]
fn forced_from_a_coordinator_that_did_not_advertise_it_is_an_ordinary_order() {
    let rig = Rig::new("forced-unadvertised");
    rig.coordinator.advertise(&["deferred-v1", "halt-v1"]);
    rig.health.say(VB, not_healthy());
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:activate"), 0);
    assert_eq!(rig.events.count("local:switch"), 1);
    assert_eq!(rig.events.count("local:revert"), 1);
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
}

/// Catches: an ordinary order changed by the force: with no `forced` in the answer, even from
/// a coordinator that advertises `forced-v1`, the A/B still puts the target on trial and goes
/// back from it when it is not healthy.
#[test]
fn an_ordinary_order_still_reverts_a_target_that_is_not_healthy() {
    let rig = Rig::new("forced-ordinary");
    rig.coordinator.set_forced(false);
    rig.health.say(VB, not_healthy());
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:activate"), 0);
    assert_eq!(
        rig.events
            .of(&["local:switch", "local:confirm", "local:revert"]),
        ["local:switch 0.1.7+gbbbbbbbbbbbb-3", "local:revert"]
    );
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
}

/// Catches: a force held back by the agent's own deferral of the same target (the coordinator
/// bypasses a deferral under force), or an ordinary order that stops being declined: after a
/// drain bound the same ordinary order is declined, and the forced one runs.
#[test]
fn a_forced_order_is_not_declined_by_a_deferral_of_the_same_target_but_an_ordinary_one_is() {
    let rig = Rig::new("forced-deferral");
    rig.keeps_vb();
    rig.coordinator.set_forced(false);
    *rig.drain.leases.lock().unwrap() = [2].into();
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(
        rig.events.count("drain n1"),
        1,
        "the drain bound deferred it"
    );

    *rig.drain.leases.lock().unwrap() = [0].into();
    rig.take_order(&agent);
    assert_eq!(
        rig.events.count("drain n1"),
        1,
        "an ordinary order is declined"
    );
    assert!(rig.logged("declined, deferred"));

    rig.coordinator.set_forced(true);
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:activate"), 1, "a forced one runs");
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: a force that is held back for good by a halt it cleared: an agent holding a halt
/// under generation 3 gets an answer with `halts {kbf: 4}`, `forced: true` and an update; the
/// clear lifts the hold in the same answer, before the order, and the force runs.
#[test]
fn a_force_that_clears_a_halt_in_the_same_answer_runs() {
    let rig = Rig::new("forced-halt");
    rig.keeps_vb();
    rig.coordinator.set_forced(false);
    rig.coordinator.set_halts(&[("kbf", 3)]);
    // The helper cannot switch, cannot put the previous release back, and A is down: a halt.
    {
        let mut world = rig.local.world();
        world.switch = Err(LocalError::Refused("nope".to_owned()));
        world.revert = Err(LocalError::Unreachable("gone".to_owned()));
    }
    rig.health.say(VA, not_healthy());
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:switch"), 1);
    // Held: the same order, forced, but under the same generation.
    rig.coordinator.set_forced(true);
    rig.take_order(&agent);
    assert!(rig.logged("held after a halt (rollback_failed)"));
    assert_eq!(rig.events.count("local:activate"), 0);

    // The force: generation 4 in the answer that carries the order.
    rig.coordinator.set_halts(&[("kbf", 4)]);
    {
        let mut world = rig.local.world();
        world.revert = Ok(());
        world.state.version = Some(version(VA));
    }
    rig.health.say(VA, vec![Ok(Health::Healthy)]);
    rig.clock.advance(Duration::from_secs(RETRY));
    rig.take_order(&agent);
    assert!(rig.logged("the halt is lifted"));
    assert_eq!(rig.events.count("local:activate"), 1);
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: a force that fails and is sent again at once drains the node in a hot loop: the
/// same target that failed a moment ago is not run again until the retry time is out, and then
/// it is.
#[test]
fn a_failed_force_is_not_run_again_before_the_retry_time() {
    let rig = Rig::new("forced-backoff");
    rig.keeps_vb();
    rig.local.world().activate = Err(LocalError::Failed("restart_failed".to_owned()));
    let agent = rig.agent();
    rig.take_order(&agent);
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:activate"), 1);
    assert!(rig.logged("failed a moment ago"));
    rig.clock.advance(Duration::from_secs(RETRY));
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:activate"), 2);
}

/// Catches: an activation made while the node still serves, or while a trial is open: the
/// helper restarts the units, so at that moment the node is drained and, on a host that has
/// nothing on trial, nothing is pending.
#[test]
fn the_activation_runs_with_the_node_drained_and_nothing_on_trial() {
    let rig = Rig::new("forced-while-drained");
    rig.keeps_vb();
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let (drain, local, seen) = (rig.drain.clone(), rig.local.clone(), Arc::clone(&seen));
        rig.local.world().on_activate = Some(Arc::new(move || {
            seen.lock()
                .unwrap()
                .push((drain.is_drained(), local.world().state.pending.is_none()));
        }));
    }
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(*seen.lock().unwrap(), [(true, true)]);
}
