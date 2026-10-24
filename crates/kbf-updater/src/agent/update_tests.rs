//! One host's A/B, step by step, over fakes of the helper, the drain and the health check.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::flags::Timing;
use super::local::{LocalError, Trial};
use super::status::{Shared, Status};
use super::testing::{
    COMMIT_A, COMMIT_B, Events, FakeDrain, FakeHealth, FakeLocal, ManualClock, VA, VB, host,
    version,
};
use super::update::{Ports, Update};
use crate::inventory::Role;
use crate::ports::{Health, PortError};
use crate::wire::{Health as WireHealth, Outcome, UpdateResult};
use kbf_types::ReleasePlatform;

const NOW: u64 = 10_000;

fn timing() -> Timing {
    Timing {
        health_timeout: Duration::from_secs(300),
        ..Timing::DEFAULT
    }
}

struct Rig {
    events: Events,
    local: FakeLocal,
    drain: FakeDrain,
    health: FakeHealth,
    clock: Arc<ManualClock>,
    status: Shared,
    logs: Arc<Mutex<Vec<String>>>,
    now: u64,
}

impl Rig {
    /// A daemon host that runs `VA`, whose node is serving, offered `VB`, which turns out
    /// healthy.
    fn new() -> Self {
        let events = Events::default();
        let health = FakeHealth::new(&events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        let status = Shared::new(Status::unknown(ReleasePlatform::LinuxX86_64, version(VA)));
        Self {
            local: FakeLocal::new(&events),
            drain: FakeDrain::new(&events),
            health,
            clock: Arc::new(ManualClock::default()),
            status,
            logs: Arc::default(),
            now: NOW,
            events,
        }
    }

    fn run(&self, target: &str) -> UpdateResult {
        let host = host(Role::Daemon);
        let logs = Arc::clone(&self.logs);
        let log = move |line: &str| logs.lock().unwrap().push(line.to_owned());
        let now = self.now;
        let unix_now = move || now;
        let update = Update {
            ports: Ports {
                local: &self.local,
                drain: &self.drain,
                health: &self.health,
                clock: &*self.clock,
                unix_now: &unix_now,
                log: &log,
            },
            host: &host,
            channel: "stable",
            timing: timing(),
            status: &self.status,
        };
        update.run(&version(target))
    }

    fn steps(&self) -> Vec<String> {
        self.events.of(&[
            "local:install",
            "drain",
            "local:switch",
            "local:confirm",
            "local:revert",
            "undrain",
        ])
    }

    fn logged(&self, needle: &str) -> bool {
        self.logs.lock().unwrap().iter().any(|l| l.contains(needle))
    }
}

fn succeeded(target: &str) -> UpdateResult {
    UpdateResult {
        target: version(target),
        outcome: Outcome::Succeeded,
    }
}

fn failed(target: &str) -> UpdateResult {
    UpdateResult {
        target: version(target),
        outcome: Outcome::Failed,
    }
}

/// "Update to X" runs fetch and verify, drain, switch, health, confirm and undrain, in that
/// order: the release is fetched through the helper with the version and the channel alone,
/// the node is drained for the configured deadline before anything is switched, health is asked
/// of the release on trial while the node is held drained, B is confirmed before the node is
/// given back, and the node is given back last. The status says `updating` the whole time and
/// the result at the end.
#[test]
fn an_update_runs_fetch_verify_drain_switch_health_confirm_undrain_in_order() {
    let rig = Rig::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let (status, seen) = (rig.status.clone(), Arc::clone(&seen));
        rig.local.world().on_install = Some(Arc::new(move || {
            seen.lock().unwrap().push(status.get().updating);
        }));
    }
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(
        rig.steps(),
        [
            &format!("local:install {VB} stable"),
            "drain n1 600",
            &format!("local:switch {VB}"),
            &format!("local:confirm {VB}"),
            "undrain",
        ]
    );
    let all = rig.events.all();
    let at = |needle: &str| all.iter().position(|e| e.starts_with(needle)).unwrap();
    assert!(at("local:install") < at("drain"));
    assert!(at("drain") < at("leases"));
    assert!(at("leases") < at("local:switch"));
    assert!(at("local:switch") < at(&format!("health:held {VB}")));
    assert!(at(&format!("health:held {VB}")) < at("local:confirm"));
    assert!(at("local:confirm") < at("undrain"));
    assert_eq!(
        *seen.lock().unwrap(),
        [Some(version(VB))],
        "updating from the first step"
    );
    let status = rig.status.get();
    assert_eq!(status.updating, None);
    assert_eq!(status.last_update, Some(succeeded(VB)));
    assert!(!rig.drain.is_drained());
}

/// A build that is not attested, is of another commit, or is not on the channel is refused by
/// the helper's check on the host, and the node is never touched: no drain, no switch, no
/// undrain. The failure is the result.
#[test]
fn an_unattested_or_wrong_commit_build_is_refused_before_the_drain() {
    for code in [
        "verify_failed",
        "unknown_release",
        "ambiguous_release",
        "fetch_failed",
        "channel_not_allowed",
    ] {
        let rig = Rig::new();
        rig.local.world().install = Err(LocalError::Refused(code.to_owned()));
        assert_eq!(rig.run(VB), failed(VB), "{code}");
        assert_eq!(
            rig.steps(),
            [format!("local:install {VB} stable")],
            "{code}"
        );
        assert!(!rig.drain.is_drained());
        assert!(rig.logged(code), "{code}");
    }
    let rig = Rig::new();
    rig.local.world().install = Err(LocalError::Unreachable("socket".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("drain"), 0);
}

/// A health check that is told not to touch a host stops the update after the helper is asked what
/// runs and before anything is fetched, drained or switched; a helper that does not answer stops
/// it sooner.
#[test]
fn a_refusal_of_the_health_check_or_a_silent_helper_stops_it_at_once() {
    let rig = Rig::new();
    *rig.health.refuses.lock().unwrap() = Some("not a pool member".to_owned());
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.all(), ["local:query"]);
    assert!(rig.logged("not a pool member"));

    let rig = Rig::new();
    rig.local.world().query = Err(LocalError::Unreachable("no socket".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(
        rig.events.of(&["drain", "local:install"]),
        Vec::<String>::new()
    );
}

/// A release the host runs already is nothing to do, and a release the helper says is the one
/// that runs (it learned its version's text) is done without a drain.
#[test]
fn a_release_that_already_runs_is_done_without_touching_the_node() {
    let rig = Rig::new();
    rig.local.world().state.version = Some(version(VB));
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(
        rig.events.of(&["local:install", "drain"]),
        Vec::<String>::new()
    );

    let rig = Rig::new();
    rig.local.world().install = Ok(super::local::Staged {
        commit: COMMIT_A.to_owned(),
        current: true,
    });
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(rig.events.count("drain"), 0);
    assert_eq!(rig.events.count("local:switch"), 0);
}

/// B never shows healthy: the update waits for the health timeout and no longer, asks the helper
/// to go back, looks at A, and gives the node back because A is healthy. The result is a failure.
#[test]
fn a_failed_health_check_reverts_and_undrains_when_a_is_healthy() {
    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Waiting("not connected".to_owned()))]);
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(
        rig.steps(),
        [
            &format!("local:install {VB} stable"),
            "drain n1 600",
            &format!("local:switch {VB}"),
            "local:revert",
            "undrain",
        ]
    );
    let polls = rig.events.count(&format!("health:held {VB}"));
    assert!(
        (55..=62).contains(&polls),
        "polled for the 300 s the timeout allows: {polls}"
    );
    assert!(
        rig.events.count(&format!("health:held {VA}")) >= 1,
        "A is looked at"
    );
    assert!(!rig.drain.is_drained());
    assert_eq!(rig.status.get().last_update, Some(failed(VB)));
    assert!(rig.logged("not connected"));
}

/// ... and when A is not healthy either the node stays drained: the update does not hand a
/// node that does not serve back to the scheduler.
#[test]
fn a_failed_health_check_stays_drained_when_a_is_unhealthy() {
    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Abandoned("wrong build".to_owned()))]);
    rig.health
        .say(VA, vec![Ok(Health::Waiting("not connected".to_owned()))]);
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("undrain"), 0);
    assert!(rig.drain.is_drained());
    assert_eq!(
        rig.events.count(&format!("health:held {VB}")),
        1,
        "abandoned is not waited out"
    );
    assert!(rig.logged("the node stays drained"));
}

/// Answers of the health check that are errors are waited out like "not yet", and the wait ends
/// at the bound: a node whose frontend cannot be asked is not healthy, and not drained back.
#[test]
fn health_errors_are_waited_out_and_end_at_the_bound() {
    let rig = Rig::new();
    rig.health.say(
        VB,
        vec![
            Err(PortError::unreachable("the operator API")),
            Err(PortError::other("503")),
            Ok(Health::Healthy),
        ],
    );
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(rig.events.count(&format!("health:held {VB}")), 3);

    let rig = Rig::new();
    rig.health
        .say(VB, vec![Err(PortError::unreachable("the operator API"))]);
    rig.health
        .say(VA, vec![Err(PortError::unreachable("the operator API"))]);
    assert_eq!(rig.run(VB), failed(VB));
    assert!(rig.drain.is_drained());
}

/// The wait for B's health never runs past the helper's deadline less the margin: with 30
/// seconds of the window left the update looks for 20 and then goes back, rather than wait for
/// the 300 the timeout allows.
#[test]
fn the_health_wait_ends_before_the_helpers_deadline() {
    let mut rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Waiting("no".to_owned()))]);
    rig.local.world().switch = Ok(NOW + 30);
    rig.now = NOW;
    assert_eq!(rig.run(VB), failed(VB));
    let polls = rig.events.count(&format!("health:held {VB}"));
    assert!((4..=5).contains(&polls), "{polls}");

    // Already past the deadline: one look, then back.
    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Waiting("no".to_owned()))]);
    rig.local.world().switch = Ok(NOW - 1);
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count(&format!("health:held {VB}")), 1);
}

/// A switch the helper could not do is a failure at that step: it went back by itself, and the
/// update asks it to be sure, finds nothing on trial, and looks at A before the node is given
/// back.
#[test]
fn a_switch_that_fails_goes_back_and_looks_at_a() {
    let rig = Rig::new();
    rig.local.world().switch = Err(LocalError::Failed("switch_failed".to_owned()));
    rig.local.world().revert = Err(LocalError::Refused("nothing_pending".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("health:held"), 1);
    assert!(rig.events.all().contains(&format!("health:held {VA}")));
    assert_eq!(rig.events.count("undrain"), 1);
    assert!(
        !rig.logged("the revert failed"),
        "nothing to revert is not worth a line"
    );

    // A refusal of the revert that is not "nothing on trial" is said like any other failure.
    let rig = Rig::new();
    rig.local.world().switch = Err(LocalError::Failed("switch_failed".to_owned()));
    rig.local.world().revert = Err(LocalError::Refused("busy".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert!(rig.logged("the revert failed"));

    // A revert that really failed is said, and the node still waits on A's health.
    let rig = Rig::new();
    rig.local.world().switch = Err(LocalError::Failed("revert_failed".to_owned()));
    rig.local.world().revert = Err(LocalError::Failed("revert_failed".to_owned()));
    rig.health
        .say(VA, vec![Ok(Health::Waiting("down".to_owned()))]);
    assert_eq!(rig.run(VB), failed(VB));
    assert!(rig.logged("the revert failed"));
    assert!(rig.drain.is_drained());
}

/// A healthy B whose confirm the helper refuses is not kept: it is reverted like any other
/// release that did not work.
#[test]
fn a_confirm_that_fails_is_reverted() {
    let rig = Rig::new();
    rig.local.world().confirm = Err(LocalError::Failed("state_unavailable".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("local:revert"), 1);
    assert_eq!(rig.events.count("undrain"), 1);
    assert!(rig.logged("the confirm failed"));
}

/// When the helper knows only A's commit, A's health is asked by that commit and the node is
/// given back if A is healthy; when it cannot say what runs, A cannot be shown healthy and the
/// node stays drained.
#[test]
fn after_a_revert_a_is_known_by_its_version_or_its_commit() {
    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Waiting("no".to_owned()))]);
    // The host never learned A's version text (a release placed by hand): once B is reverted,
    // the helper knows A by its commit alone.
    rig.local.world().state.version = None;
    rig.local.world().before = None;
    rig.health
        .say("0.1.0+gaaaaaaaaaaaa", vec![Ok(Health::Healthy)]);
    assert_eq!(rig.run(VB), failed(VB));
    assert!(
        rig.events
            .all()
            .contains(&"health:held 0.1.0+gaaaaaaaaaaaa".to_owned())
    );
    assert_eq!(rig.events.count("undrain"), 1, "A is healthy by its commit");

    // A helper that cannot say what runs after the revert: A cannot be shown healthy.
    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Abandoned("no".to_owned()))]);
    rig.local.world().mute_after_revert = true;
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("undrain"), 0);
    assert!(rig.drain.is_drained());
    assert!(rig.logged("does not say what runs"));
}

/// The node's drain is waited out: the leases are looked at until none is left, and when they
/// never end the update goes on at the deadline, leases or not.
#[test]
fn the_leases_are_waited_for_until_the_deadline() {
    let rig = Rig::new();
    *rig.drain.leases.lock().unwrap() = [3, 2, 1, 0].into();
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(rig.events.count("leases"), 4);

    let rig = Rig::new();
    *rig.drain.leases.lock().unwrap() = [2].into();
    assert_eq!(rig.run(VB), succeeded(VB));
    let looks = rig.events.count("leases");
    assert!((120..=122).contains(&looks), "600 s in steps of 5: {looks}");
}

/// A drain that fails stops the update before the switch and leaves the node as it was; a node
/// whose leases cannot be read is given back (it was drained here), and the update fails.
#[test]
fn a_drain_that_fails_stops_the_update_before_the_switch() {
    let rig = Rig::new();
    *rig.drain.drain_error.lock().unwrap() = Some(PortError::other("404"));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("local:switch"), 0);
    assert_eq!(rig.events.count("undrain"), 0);
    assert!(rig.logged("could not be drained"));

    let rig = Rig::new();
    *rig.drain.leases_error.lock().unwrap() = Some(PortError::other("serving again"));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("local:switch"), 0);
    assert_eq!(rig.events.count("undrain"), 1);
}

/// B is confirmed and healthy but the node cannot be given back: the update has not finished
/// and says so, and the node stays drained for a person to see.
#[test]
fn an_undrain_that_fails_after_a_confirm_fails_the_update() {
    let rig = Rig::new();
    *rig.drain.undrain_error.lock().unwrap() = Some(PortError::unreachable("the operator API"));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("local:confirm"), 1);
    assert!(rig.drain.is_drained());
    assert!(rig.logged("stays drained"));

    // Back on A, the same: the node stays drained and it is said.
    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Abandoned("no".to_owned()))]);
    *rig.drain.undrain_error.lock().unwrap() = Some(PortError::other("503"));
    assert_eq!(rig.run(VB), failed(VB));
    assert!(rig.logged("back on A, and the node stays drained"));
}

/// An updater restarted in the middle of an A/B finds the release on trial in the helper's
/// state and finishes it from the health step: nothing is fetched, drained or switched a
/// second time, and the result is for the release that was on trial.
#[test]
fn a_restart_in_the_middle_does_not_install_twice() {
    let rig = Rig::new();
    {
        let mut world = rig.local.world();
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.pending = Some(Trial {
            version: Some(version(VB)),
            commit: COMMIT_B.to_owned(),
            deadline_unix: Some(NOW + 400),
        });
        world.before = Some(version(VA));
    }
    // Drained by the updater that was here before.
    *rig.drain.drained.lock().unwrap() = true;
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(
        rig.steps(),
        [&format!("local:confirm {VB}"), "undrain"],
        "no fetch, no drain, no switch"
    );
    assert_eq!(rig.events.count("local:install"), 0);
    assert!(!rig.drain.is_drained());

    // Asked for another version than the one on trial: the one on trial is finished, and it is
    // the one reported.
    let rig = Rig::new();
    {
        let mut world = rig.local.world();
        world.state.pending = Some(Trial {
            version: Some(version(VB)),
            commit: COMMIT_B.to_owned(),
            deadline_unix: Some(NOW + 400),
        });
    }
    assert_eq!(rig.run("0.1.9+gdddddddddddd-5"), succeeded(VB));
    assert_eq!(rig.status.get().last_update, Some(succeeded(VB)));
    assert_eq!(rig.events.count("local:install"), 0);

    // A trial of a release this host never named is not one to keep.
    let rig = Rig::new();
    rig.local.world().state.pending = Some(Trial {
        version: None,
        commit: COMMIT_B.to_owned(),
        deadline_unix: Some(NOW + 400),
    });
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("local:revert"), 1);
    assert_eq!(rig.events.count("local:confirm"), 0);
    assert!(rig.logged("not staged by it"));
}

/// The host's health in the heartbeat is the wire's two words; a health value maps to them.
#[test]
fn the_wire_health_is_two_words() {
    assert_eq!(
        serde_json::to_string(&WireHealth::Healthy).unwrap(),
        "\"healthy\""
    );
    assert_eq!(
        serde_json::to_string(&WireHealth::Unhealthy).unwrap(),
        "\"unhealthy\""
    );
}
