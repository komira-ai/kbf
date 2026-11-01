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
use crate::ports::{Clock, Health, PortError};
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
    timing: Timing,
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
            timing: timing(),
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
            timing: self.timing,
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
    UpdateResult::new(version(target), Outcome::Succeeded)
}

fn failed(target: &str) -> UpdateResult {
    UpdateResult::new(version(target), Outcome::Failed)
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
            "drain n1 none",
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

/// A health check that is told not to touch a host (a role this build does not update) stops the
/// update before the helper is asked anything; so does a helper that does not answer.
#[test]
fn a_refusal_of_the_health_check_or_a_silent_helper_stops_it_at_once() {
    let rig = Rig::new();
    *rig.health.refuses.lock().unwrap() = Some("not a pool member".to_owned());
    assert_eq!(rig.run(VB), failed(VB));
    assert!(rig.events.all().is_empty());
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
            "drain n1 none",
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

/// With no drain deadline (the default) the leases are looked at until none is left, however
/// long that takes: 200 looks 5 s apart is 1000 s, past the 600 s the old default gave up at,
/// and the host is switched only after the look that reads 0.
#[test]
fn the_leases_are_waited_for_however_long_they_take() {
    let rig = Rig::new();
    *rig.drain.leases.lock().unwrap() = [3, 2, 1, 0].into();
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(rig.events.count("leases"), 4);

    let rig = Rig::new();
    let mut held: std::collections::VecDeque<usize> = std::iter::repeat_n(2, 200).collect();
    held.push_back(0);
    *rig.drain.leases.lock().unwrap() = held;
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(rig.events.count("leases"), 201);
    assert!(rig.clock.now() > Duration::from_secs(600));
    let all = rig.events.all();
    let at = |needle: &str| all.iter().position(|e| e.starts_with(needle)).unwrap();
    assert!(all.iter().rposition(|e| e == "leases").unwrap() < at("local:switch"));
}

/// A drain deadline the operator set that passes with a lease still held does not switch: the
/// update fails saying why, the node is left drained (no undrain), and the helper is never
/// asked to switch. The same order, tried again after the retry time, drains and waits again.
#[test]
fn a_drain_deadline_that_passes_switches_nothing_and_leaves_the_node_drained() {
    let mut rig = Rig::new();
    rig.timing.drain_deadline = Some(Duration::from_secs(60));
    *rig.drain.leases.lock().unwrap() = [2].into();
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("local:switch"), 0);
    assert_eq!(rig.events.count("undrain"), 0);
    assert!(rig.drain.is_drained());
    assert_eq!(rig.events.count("drain n1 60"), 1);
    assert!(rig.logged("still holds 2 leases after the drain deadline of 60 s"));
    assert!(rig.logged("left drained"));
    assert_eq!(rig.clock.now(), Duration::from_secs(60));

    // The leases end later; the same order, run again, drains again and switches.
    *rig.drain.leases.lock().unwrap() = [0].into();
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(rig.events.count("drain n1 60"), 2);
    assert!(!rig.drain.is_drained());
}

/// A switch that takes longer than the 10 minutes the call used to be limited to (the helper
/// waits for the frontend's drain as long as it takes) is not a failure: the update waits for it
/// and succeeds, and the default limit on a switch is none.
#[test]
fn a_switch_that_takes_longer_than_ten_minutes_is_waited_for() {
    let rig = Rig::new();
    let clock = Arc::clone(&rig.clock);
    rig.local.world().on_switch = Some(Arc::new(move || {
        clock.advance(Duration::from_secs(3 * 3600));
    }));
    assert_eq!(rig.run(VB), succeeded(VB));
    assert!(rig.clock.now() >= Duration::from_secs(3 * 3600));
    assert_eq!(rig.events.count("local:switch"), 1);
    assert_eq!(rig.events.count("local:revert"), 0);
    assert!(!rig.logged("switch failed"));
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

const LEFT_OUT: &str = "node was out of placement before the roll; left out";

/// A node an operator took out of placement before the roll is drained and waited for as any
/// other (a restart cuts no lease off), and after the confirm it is not uncordoned: the update
/// succeeds, `undrain` is never called, the node ends out of placement, and the log says it was
/// left out. The node is read before it is drained.
#[test]
fn a_node_out_of_placement_before_the_roll_is_not_given_back_after_the_confirm() {
    let rig = Rig::new();
    rig.drain.out_of_placement();
    *rig.drain.leases.lock().unwrap() = [2, 0].into();
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(
        rig.steps(),
        [
            &format!("local:install {VB} stable"),
            "drain n1 none",
            &format!("local:switch {VB}"),
            &format!("local:confirm {VB}"),
        ]
    );
    assert_eq!(rig.events.count("leases"), 2, "its leases are waited for");
    let all = rig.events.all();
    let at = |needle: &str| all.iter().position(|e| e.starts_with(needle)).unwrap();
    assert!(at("placed") < at("drain"), "{all:?}");
    assert_eq!(rig.events.count("undrain"), 0);
    assert!(rig.drain.is_drained());
    assert!(rig.logged(LEFT_OUT));
}

/// ... and on every path that used to give the node back, a node that was out stays out: B
/// unhealthy with A healthy, a switch the helper could not do, a confirm the helper refused,
/// and a node whose leases could not be read.
#[test]
fn a_node_out_of_placement_before_the_roll_is_not_given_back_on_any_revert_path() {
    // B never shows healthy; A does.
    let rig = Rig::new();
    rig.drain.out_of_placement();
    rig.health
        .say(VB, vec![Ok(Health::Abandoned("wrong build".to_owned()))]);
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("local:revert"), 1);
    assert_eq!(rig.events.count("undrain"), 0, "back on A");
    assert!(rig.drain.is_drained());
    assert!(rig.logged(LEFT_OUT));

    // The switch fails and the helper has nothing to revert; A is healthy.
    let rig = Rig::new();
    rig.drain.out_of_placement();
    rig.local.world().switch = Err(LocalError::Failed("switch_failed".to_owned()));
    rig.local.world().revert = Err(LocalError::Refused("nothing_pending".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("undrain"), 0, "switch failed");
    assert!(rig.drain.is_drained());
    assert!(rig.logged(LEFT_OUT));

    // The confirm is refused, so B is reverted.
    let rig = Rig::new();
    rig.drain.out_of_placement();
    rig.local.world().confirm = Err(LocalError::Failed("state_unavailable".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("local:revert"), 1);
    assert_eq!(rig.events.count("undrain"), 0, "confirm refused");
    assert!(rig.drain.is_drained());
    assert!(rig.logged(LEFT_OUT));

    // The leases cannot be read: nothing is switched, and the node is not uncordoned.
    let rig = Rig::new();
    rig.drain.out_of_placement();
    *rig.drain.leases_error.lock().unwrap() = Some(PortError::other("serving again"));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("local:switch"), 0);
    assert_eq!(rig.events.count("undrain"), 0, "leases unreadable");
    assert!(rig.drain.is_drained());
    assert!(rig.logged(LEFT_OUT));
}

/// A node in placement before the roll is given back as ever: after the confirm, and after a
/// revert to a healthy A; and the log does not say it was left out.
#[test]
fn a_node_in_placement_before_the_roll_is_given_back() {
    let rig = Rig::new();
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(rig.events.count("placed"), 1);
    assert_eq!(rig.events.count("undrain"), 1);
    assert!(!rig.drain.is_drained());
    assert!(!rig.logged(LEFT_OUT));

    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Abandoned("wrong build".to_owned()))]);
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(rig.events.count("undrain"), 1);
    assert!(!rig.drain.is_drained());
    assert!(!rig.logged(LEFT_OUT));
}

/// A node that cannot be read is not guessed at: the update fails before the drain, and
/// nothing is drained, switched or given back.
#[test]
fn a_node_that_cannot_be_read_stops_the_update_before_the_drain() {
    let rig = Rig::new();
    *rig.drain.placed_error.lock().unwrap() = Some(PortError::unreachable("the operator API"));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(
        rig.steps(),
        [format!("local:install {VB} stable")],
        "no drain, no switch, no undrain"
    );
    assert!(rig.logged("could not be drained"));
}

/// A restart with B on trial reads the node as the first run does: one an operator drained is
/// finished the same way and left out, with the log saying so; one in placement is given back.
///
/// Catches: a resume that takes every node for the updater's own and uncordons an operator's.
#[test]
fn a_restart_in_the_middle_does_not_give_back_a_node_an_operator_drained() {
    for (out, undrains) in [(true, 0), (false, 1)] {
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
        if out {
            rig.drain.out_of_placement();
        } else {
            *rig.drain.drained.lock().unwrap() = true;
        }
        assert_eq!(rig.run(VB), succeeded(VB));
        assert_eq!(rig.events.count("undrain"), undrains, "out: {out}");
        assert_eq!(rig.events.count("drain "), 0, "out: {out}");
        assert_eq!(rig.logged(LEFT_OUT), out, "out: {out}");
    }
}

// ---- goldens of the call order (docs/design/host-updates.md, section 8, step 1) ----------
//
// Each test below writes out every call the update makes on the fake helper, drain and health
// check, in order, for one path. Runs of the same call are written once with a count; the
// counts come from the fake clock and the default polls (5 s), never from real time. The
// kbf hook (migration step 5) must make the same calls in the same order.

/// Every call, in order, one per line; a run of the same call is `call (xN)`.
fn calls(rig: &Rig) -> Vec<String> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for event in rig.events.all() {
        match out.last_mut() {
            Some((last, n)) if *last == event => *n += 1,
            _ => out.push((event, 1)),
        }
    }
    out.into_iter()
        .map(|(event, n)| {
            if n > 1 {
                format!("{event} (x{n})")
            } else {
                event
            }
        })
        .collect()
}

/// The defaults the update runs on when a flag does not say otherwise.
#[test]
fn the_default_timing_is_that_of_today() {
    let t = Timing::DEFAULT;
    assert_eq!(
        format!(
            "heartbeat={:?} drain_deadline={:?} drain_poll={:?} health_timeout={:?} \
             health_poll={:?} retry={:?} margin={:?}",
            t.heartbeat,
            t.drain_deadline,
            t.drain_poll,
            t.health_timeout,
            t.health_poll,
            t.retry,
            t.margin
        ),
        "heartbeat=15s drain_deadline=None drain_poll=5s health_timeout=300s \
         health_poll=5s retry=900s margin=10s"
    );
}

/// A normal update: the helper is asked what runs, the release is fetched and verified, the
/// node's placement is read, it is drained and its leases looked at once, the helper switches,
/// B's health is asked once, B is confirmed, and the node is given back.
#[test]
fn golden_call_order_of_a_normal_update() {
    let rig = Rig::new();
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(
        calls(&rig),
        [
            "local:query",
            &format!("local:install {VB} stable"),
            "placed",
            "drain n1 none",
            "leases",
            &format!("local:switch {VB}"),
            &format!("health:held {VB}"),
            &format!("local:confirm {VB}"),
            "undrain",
        ]
    );
}

/// A normal update on a node that holds leases: the leases are looked at again after each 5 s
/// poll until none is left, and only then is the helper asked to switch.
#[test]
fn golden_call_order_when_leases_drain_away() {
    let rig = Rig::new();
    *rig.drain.leases.lock().unwrap() = [3, 1, 0].into();
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(
        calls(&rig),
        [
            "local:query",
            &format!("local:install {VB} stable"),
            "placed",
            "drain n1 none",
            "leases (x3)",
            &format!("local:switch {VB}"),
            &format!("health:held {VB}"),
            &format!("local:confirm {VB}"),
            "undrain",
        ]
    );
}

/// B never shows healthy: it is looked at 61 times over the 300 s the timeout allows (every 5 s,
/// the first at 0 and the last at 300), the helper is asked to revert, asked what runs, A's
/// health is asked once, and the node is given back.
#[test]
fn golden_call_order_of_a_failed_verify_that_reverts() {
    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Waiting("not connected".to_owned()))]);
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(
        calls(&rig),
        [
            "local:query",
            &format!("local:install {VB} stable"),
            "placed",
            "drain n1 none",
            "leases",
            &format!("local:switch {VB}"),
            &format!("health:held {VB} (x61)"),
            "local:revert",
            "local:query",
            &format!("health:held {VA}"),
            "undrain",
        ]
    );
}

/// B is abandoned at its first look and A does not come back either: A is looked at 61 times,
/// and the node is never given back.
#[test]
fn golden_call_order_of_a_revert_onto_an_unhealthy_a() {
    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Abandoned("wrong build".to_owned()))]);
    rig.health
        .say(VA, vec![Ok(Health::Waiting("not connected".to_owned()))]);
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(
        calls(&rig),
        [
            "local:query",
            &format!("local:install {VB} stable"),
            "placed",
            "drain n1 none",
            "leases",
            &format!("local:switch {VB}"),
            &format!("health:held {VB}"),
            "local:revert",
            "local:query",
            &format!("health:held {VA} (x61)"),
        ]
    );
}

/// A healthy B whose confirm the helper refuses is reverted like any other release that did
/// not work.
#[test]
fn golden_call_order_of_a_refused_confirm() {
    let rig = Rig::new();
    rig.local.world().confirm = Err(LocalError::Failed("state_unavailable".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(
        calls(&rig),
        [
            "local:query",
            &format!("local:install {VB} stable"),
            "placed",
            "drain n1 none",
            "leases",
            &format!("local:switch {VB}"),
            &format!("health:held {VB}"),
            &format!("local:confirm {VB}"),
            "local:revert",
            "local:query",
            &format!("health:held {VA}"),
            "undrain",
        ]
    );
}

/// A switch the helper cannot do: the helper is asked to revert and says nothing is on trial,
/// A's health is asked, and the node is given back.
#[test]
fn golden_call_order_of_a_failed_switch() {
    let rig = Rig::new();
    rig.local.world().switch = Err(LocalError::Failed("switch_failed".to_owned()));
    rig.local.world().revert = Err(LocalError::Refused("nothing_pending".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(
        calls(&rig),
        [
            "local:query",
            &format!("local:install {VB} stable"),
            "placed",
            "drain n1 none",
            "leases",
            &format!("local:switch {VB}"),
            "local:revert",
            "local:query",
            &format!("health:held {VA}"),
            "undrain",
        ]
    );
}

/// A drain deadline of 60 s with a lease still held: the leases are looked at every 5 s, 13
/// times from 0 to 60 s, and then the update stops. Nothing is switched, reverted or given back.
#[test]
fn golden_call_order_of_a_drain_deadline() {
    let mut rig = Rig::new();
    rig.timing.drain_deadline = Some(Duration::from_secs(60));
    *rig.drain.leases.lock().unwrap() = [2].into();
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(
        calls(&rig),
        [
            "local:query",
            &format!("local:install {VB} stable"),
            "placed",
            "drain n1 60",
            "leases (x13)",
        ]
    );
}

/// An updater restarted with B on trial: it reads the helper's state, reads the node's
/// placement, and finishes from the health step. Nothing is fetched, drained or switched.
#[test]
fn golden_call_order_of_a_resumed_trial() {
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
    *rig.drain.drained.lock().unwrap() = true;
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(
        calls(&rig),
        [
            "local:query",
            "placed",
            &format!("health:held {VB}"),
            &format!("local:confirm {VB}"),
            "undrain",
        ]
    );
}

/// A node an operator had drained is waited for like any other but never given back: the
/// calls are those of a normal update without the `undrain`.
#[test]
fn golden_call_order_on_a_node_that_was_out_of_placement() {
    let rig = Rig::new();
    rig.drain.out_of_placement();
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(
        calls(&rig),
        [
            "local:query",
            &format!("local:install {VB} stable"),
            "placed",
            "drain n1 none",
            "leases",
            &format!("local:switch {VB}"),
            &format!("health:held {VB}"),
            &format!("local:confirm {VB}"),
        ]
    );
}

// ---- goldens of the failure paths (docs/design/host-updates.md, section 8, step 1) ---------

/// What the first four calls of an update leave behind, up to the one that fails.
fn head(upto: usize) -> Vec<String> {
    [
        "local:query".to_owned(),
        format!("local:install {VB} stable"),
        "placed".to_owned(),
        "drain n1 none".to_owned(),
    ]
    .into_iter()
    .take(upto)
    .collect()
}

/// A fetch the helper cannot do, or a release it refuses to verify (attestation, the digest,
/// an unknown build), or an install that fails: the helper is asked what runs and to install,
/// and nothing else is called. The node is never read, drained or switched.
#[test]
fn golden_call_order_of_a_failed_fetch_or_verify() {
    for error in [
        LocalError::Refused("fetch_failed".to_owned()),
        LocalError::Refused("verify_failed".to_owned()),
        LocalError::Refused("sha256_mismatch".to_owned()),
        LocalError::Failed("install_failed".to_owned()),
        LocalError::Unreachable("socket".to_owned()),
    ] {
        let rig = Rig::new();
        rig.local.world().install = Err(error.clone());
        assert_eq!(rig.run(VB), failed(VB), "{error:?}");
        assert_eq!(calls(&rig), head(2), "{error:?}");
    }
}

/// A helper that cannot say what runs: that is the only call.
#[test]
fn golden_call_order_of_a_failed_query() {
    let rig = Rig::new();
    rig.local.world().query = Err(LocalError::Unreachable("no socket".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(calls(&rig), head(1));
}

/// A placement that cannot be read stops the update after the install, before the drain.
#[test]
fn golden_call_order_of_a_failed_placement_read() {
    let rig = Rig::new();
    *rig.drain.placed_error.lock().unwrap() = Some(PortError::other("503"));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(calls(&rig), head(3));
}

/// A drain that fails: nothing is looked at, switched or given back.
#[test]
fn golden_call_order_of_a_failed_drain() {
    let rig = Rig::new();
    *rig.drain.drain_error.lock().unwrap() = Some(PortError::other("404"));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(calls(&rig), head(4));
}

/// Leases that cannot be read after the drain: the node is given back, nothing is switched.
#[test]
fn golden_call_order_of_a_failed_lease_read() {
    let rig = Rig::new();
    *rig.drain.leases_error.lock().unwrap() = Some(PortError::other("serving again"));
    assert_eq!(rig.run(VB), failed(VB));
    let mut want = head(4);
    want.extend(["leases".to_owned(), "undrain".to_owned()]);
    assert_eq!(calls(&rig), want);
}

/// A switch the helper refuses (not a failure of the switch itself) whose revert then succeeds
/// and has A back: the same road as any release that did not work.
#[test]
fn golden_call_order_of_a_refused_switch_and_a_revert_that_works() {
    let rig = Rig::new();
    rig.local.world().switch = Err(LocalError::Refused("busy".to_owned()));
    rig.local.world().before = Some(version(VA));
    assert_eq!(rig.run(VB), failed(VB));
    let mut want = head(4);
    want.extend([
        "leases".to_owned(),
        format!("local:switch {VB}"),
        "local:revert".to_owned(),
        "local:query".to_owned(),
        format!("health:held {VA}"),
        "undrain".to_owned(),
    ]);
    assert_eq!(calls(&rig), want);
}

/// A revert that itself fails: the helper still says B runs, B is looked at again (it is
/// abandoned), and the node stays drained.
#[test]
fn golden_call_order_of_a_revert_that_fails() {
    let rig = Rig::new();
    rig.health
        .say(VB, vec![Ok(Health::Abandoned("wrong build".to_owned()))]);
    rig.local.world().revert = Err(LocalError::Failed("revert_failed".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    let mut want = head(4);
    want.extend([
        "leases".to_owned(),
        format!("local:switch {VB}"),
        format!("health:held {VB}"),
        "local:revert".to_owned(),
        "local:query".to_owned(),
        format!("health:held {VB}"),
    ]);
    assert_eq!(calls(&rig), want);
    assert!(rig.drain.is_drained());
}

/// When the heartbeat's `updating` is set and cleared: set before the first port call, held
/// through the install, the switch and the confirm, and cleared only when the result is
/// written, with the result in the same step. A trial found on the helper replaces the asked
/// version as the one reported. Read from the status at each hook.
#[test]
fn golden_updating_in_the_status_through_an_update() {
    fn seen(status: &Shared) -> String {
        let s = status.get();
        format!(
            "updating={} last={}",
            s.updating.map_or("none".to_owned(), |v| v.to_string()),
            s.last_update.map_or("none".to_owned(), |r| format!(
                "{} {:?}",
                r.target, r.outcome
            )),
        )
    }
    let watch = |rig: &Rig| {
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut guard = rig.local.world();
        let world = &mut *guard;
        for (tag, slot) in [
            ("install", &mut world.on_install),
            ("switch", &mut world.on_switch),
            ("confirm", &mut world.on_confirm),
        ] {
            let (status, log) = (rig.status.clone(), Arc::clone(&log));
            *slot = Some(Arc::new(move || {
                log.lock()
                    .unwrap()
                    .push(format!("{tag}: {}", seen(&status)));
            }));
        }
        log
    };

    let rig = Rig::new();
    let log = watch(&rig);
    assert_eq!(seen(&rig.status), "updating=none last=none");
    assert_eq!(rig.run(VB), succeeded(VB));
    assert_eq!(
        *log.lock().unwrap(),
        [
            format!("install: updating={VB} last=none"),
            format!("switch: updating={VB} last=none"),
            format!("confirm: updating={VB} last=none"),
        ]
    );
    assert_eq!(
        seen(&rig.status),
        format!("updating=none last={VB} Succeeded")
    );

    let rig = Rig::new();
    let log = watch(&rig);
    rig.local.world().install = Err(LocalError::Refused("verify_failed".to_owned()));
    assert_eq!(rig.run(VB), failed(VB));
    assert_eq!(
        *log.lock().unwrap(),
        [format!("install: updating={VB} last=none")]
    );
    assert_eq!(seen(&rig.status), format!("updating=none last={VB} Failed"));

    let rig = Rig::new();
    let log = watch(&rig);
    rig.local.world().state.pending = Some(Trial {
        version: Some(version(VB)),
        commit: COMMIT_B.to_owned(),
        deadline_unix: Some(NOW + 400),
    });
    assert_eq!(rig.run("0.1.9+gdddddddddddd-5"), succeeded(VB));
    assert_eq!(
        *log.lock().unwrap(),
        [format!("confirm: updating={VB} last=none")],
        "the trial's version, not the asked one"
    );
    assert_eq!(
        seen(&rig.status),
        format!("updating=none last={VB} Succeeded")
    );
}
