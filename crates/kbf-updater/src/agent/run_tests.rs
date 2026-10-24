//! The agent over fakes, with its two threads: what the heartbeat says, that it goes on while an
//! update blocks, how an order is taken, and how a restart and a failure are handled.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kbf_types::ReleasePlatform;

use super::coordinator::CoordinatorError;
use super::flags::Timing;
use super::local::Trial;
use super::renew::Renewer;
use super::run::{Agent, Stop, World};
use super::status::Status;
use super::testing::{
    COMMIT_B, Events, FakeCoordinator, FakeDrain, FakeHealth, FakeLocal, VA, VB, host, version,
};
use crate::inventory::Role;
use crate::ports::{Health, SystemClock};
use crate::wire::{Health as WireHealth, Heartbeat, Outcome, Reply};

fn timing() -> Timing {
    Timing {
        heartbeat: Duration::from_millis(5),
        drain_poll: Duration::from_millis(1),
        health_poll: Duration::from_millis(1),
        retry: Duration::from_secs(3600),
        ..Timing::DEFAULT
    }
}

struct Rig {
    events: Events,
    coordinator: FakeCoordinator,
    local: FakeLocal,
    drain: FakeDrain,
    health: FakeHealth,
    logs: Arc<Mutex<Vec<String>>>,
    timing: Timing,
    renewer: Option<Arc<dyn Renewer>>,
}

impl Rig {
    fn new() -> Self {
        let events = Events::default();
        let health = FakeHealth::new(&events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        Self {
            coordinator: FakeCoordinator::default(),
            local: FakeLocal::new(&events),
            drain: FakeDrain::new(&events),
            health,
            logs: Arc::default(),
            timing: timing(),
            renewer: None,
            events,
        }
    }

    fn agent(&self) -> Agent {
        let logs = Arc::clone(&self.logs);
        Agent::new(
            host(Role::Daemon),
            "stable".to_owned(),
            self.timing,
            World {
                coordinator: Arc::new(self.coordinator.clone()),
                local: Arc::new(self.local.clone()),
                drain: Arc::new(self.drain.clone()),
                health: Arc::new(self.health.clone()),
                clock: Arc::new(SystemClock::new()),
                unix_now: Arc::new(|| 10_000),
                renewer: self.renewer.clone(),
                log: Arc::new(move |line| logs.lock().unwrap().push(line.to_owned())),
            },
            Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
        )
    }

    /// Run the agent until `done` is true of it, for no more than ten seconds.
    fn run_until(&self, agent: &Agent, done: impl Fn(&Self) -> bool) {
        let stop = Stop::default();
        std::thread::scope(|scope| {
            let runner = scope.spawn(|| agent.run(&stop));
            let start = Instant::now();
            while !done(self) && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(1));
            }
            stop.stop();
            runner.join().unwrap();
        });
    }

    fn beats(&self) -> Vec<Heartbeat> {
        self.coordinator.beats()
    }

    fn logged(&self, needle: &str) -> bool {
        self.logs.lock().unwrap().iter().any(|l| l.contains(needle))
    }
}

fn ordered(rig: &Rig, target: &str) {
    rig.coordinator.reply(Ok(Reply::Update {
        version: version(target),
    }));
}

/// The heartbeat is the platform, the version the host runs, its health, the last update and the
/// target of the one running: and nothing that says which host it is. The node id and the role
/// the agent knows are in no field and in no value; the coordinator reads them from the
/// certificate.
#[test]
fn the_heartbeat_carries_no_node_id_or_role() {
    let rig = Rig::new();
    let agent = rig.agent();
    rig.run_until(&agent, |rig| rig.beats().len() >= 3);
    let beats = rig.beats();
    assert!(beats.len() >= 3);
    for beat in &beats {
        let json = serde_json::to_value(beat).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["health", "last_update", "platform", "updating", "version"]
        );
        let text = json.to_string();
        for forbidden in ["n1", "daemon", "node", "role", "host"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
        assert_eq!(json["platform"], "linux-x86_64");
    }
}

/// The first heartbeat is sent at start and already says what the host runs and that it is
/// healthy; before the helper has been asked, a host would say neither.
#[test]
fn the_first_heartbeat_says_the_running_version_and_health() {
    let rig = Rig::new();
    let agent = rig.agent();
    rig.run_until(&agent, |rig| !rig.beats().is_empty());
    let first = &rig.beats()[0];
    assert_eq!(first.version, version(VA));
    assert_eq!(first.health, WireHealth::Healthy);
    assert_eq!(
        (first.last_update.clone(), first.updating.clone()),
        (None, None)
    );
}

/// The heartbeat goes on through an A/B: while the install blocks, heartbeats keep arriving, each
/// saying `updating` with the target; afterwards they say the update succeeded and nothing is
/// updating.
#[test]
fn it_keeps_heartbeating_through_an_ab() {
    let rig = Rig::new();
    ordered(&rig, VB);
    let seen_while_blocked = Arc::new(Mutex::new(0_usize));
    {
        let (coordinator, seen) = (rig.coordinator.clone(), Arc::clone(&seen_while_blocked));
        rig.local.world().on_install = Some(Arc::new(move || {
            // Hold the install until three heartbeats have been sent since it began.
            let start = Instant::now();
            let updating =
                |c: &FakeCoordinator| c.beats().iter().filter(|b| b.updating.is_some()).count();
            while updating(&coordinator) < 3 && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(1));
            }
            *seen.lock().unwrap() = updating(&coordinator);
        }));
    }
    let agent = rig.agent();
    rig.run_until(&agent, |rig| {
        rig.beats().iter().any(|b| b.last_update.is_some())
    });
    assert!(
        *seen_while_blocked.lock().unwrap() >= 3,
        "heartbeats while the install blocked"
    );
    let beats = rig.beats();
    let during: Vec<_> = beats.iter().filter(|b| b.updating.is_some()).collect();
    assert!(during.iter().all(|b| b.updating == Some(version(VB))));
    let last = beats
        .iter()
        .rev()
        .find(|b| b.last_update.is_some())
        .unwrap();
    assert_eq!(
        last.last_update.as_ref().unwrap().outcome,
        Outcome::Succeeded
    );
    assert_eq!(last.last_update.as_ref().unwrap().target, version(VB));
    assert_eq!(last.updating, None);
    assert_eq!(
        last.version,
        version(VB),
        "the running version is B's after the confirm"
    );
    assert_eq!(rig.events.count("undrain"), 1);
}

/// The reply's version is the only thing taken from it: the helper is asked for that version
/// from the channel the agent was configured with, and for nothing else. A reply with any other
/// field is not a reply at all (the wire type refuses it), so no URL or path can ride along.
#[test]
fn the_replys_version_is_the_only_input() {
    let rig = Rig::new();
    ordered(&rig, VB);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| rig.events.count("local:install") >= 1);
    assert_eq!(
        rig.events.of(&["local:install"])[0],
        format!("local:install {VB} stable")
    );
    let with_url =
        r#"{"reply":"update","version":"0.1.7+gbbbbbbbbbbbb-3","url":"http://x.invalid/k"}"#;
    assert!(serde_json::from_str::<Reply>(with_url).is_err());
    let with_path = r#"{"reply":"update","version":"/tmp/kbf"}"#;
    assert!(serde_json::from_str::<Reply>(with_path).is_err());
}

/// An updater that was restarted in the middle of an A/B finishes it: its first heartbeat says
/// `updating` with the release on trial, nothing is fetched, drained or switched again, and the
/// trial is confirmed and the node given back.
#[test]
fn a_restarted_agent_resumes_the_trial_without_installing_twice() {
    let rig = Rig::new();
    {
        let mut world = rig.local.world();
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.pending = Some(Trial {
            version: Some(version(VB)),
            commit: COMMIT_B.to_owned(),
            deadline_unix: Some(10_400),
        });
        world.before = Some(version(VA));
    }
    *rig.drain.drained.lock().unwrap() = true;
    // The trial is held at its confirm until a heartbeat has been sent.
    {
        let coordinator = rig.coordinator.clone();
        rig.local.world().on_confirm = Some(Arc::new(move || {
            let start = Instant::now();
            while coordinator.beats().is_empty() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }));
    }
    // The coordinator keeps ordering B, as it would until it sees the result.
    ordered(&rig, VB);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| {
        rig.beats().iter().any(|b| b.last_update.is_some())
    });
    let beats = rig.beats();
    assert_eq!(
        beats[0].updating,
        Some(version(VB)),
        "the first heartbeat says updating"
    );
    assert_eq!(rig.events.count("local:install"), 0);
    assert_eq!(rig.events.count("local:switch"), 0);
    assert_eq!(rig.events.count("drain"), 0);
    assert_eq!(rig.events.count("local:confirm"), 1);
    assert!(!rig.drain.is_drained());
}

/// A target that failed is not tried again before the retry time is out: the coordinator
/// keeps ordering it, and the host drains itself once, not at every heartbeat. When the time is
/// out the same order is tried again.
#[test]
fn a_target_that_failed_is_left_alone_until_the_retry_time() {
    let mut rig = Rig::new();
    rig.local.world().install = Err(super::local::LocalError::Refused(
        "verify_failed".to_owned(),
    ));
    ordered(&rig, VB);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| rig.beats().len() >= 12);
    assert_eq!(
        rig.events.count("local:install"),
        1,
        "{:?}",
        rig.events.all()
    );
    assert!(rig.logged("not again yet"));
    let beats = rig.beats();
    let last = beats.last().unwrap();
    assert_eq!(last.last_update.as_ref().unwrap().outcome, Outcome::Failed);

    rig.timing.retry = Duration::from_millis(30);
    let rig2 = Rig {
        timing: rig.timing,
        ..Rig::new()
    };
    rig2.local.world().install = Err(super::local::LocalError::Refused(
        "verify_failed".to_owned(),
    ));
    ordered(&rig2, VB);
    let agent = rig2.agent();
    rig2.run_until(&agent, |rig| rig.events.count("local:install") >= 2);
    assert!(rig2.events.count("local:install") >= 2);
}

/// A coordinator that cannot be reached, or answers wrongly, costs a log line and the next
/// heartbeat: nothing starts and nothing stops.
#[test]
fn a_coordinator_that_fails_is_logged_and_the_agent_goes_on() {
    let rig = Rig::new();
    rig.coordinator
        .reply(Err(CoordinatorError::Unreachable("down".to_owned())));
    let agent = rig.agent();
    rig.run_until(&agent, |rig| rig.beats().len() >= 3);
    assert!(rig.logged("heartbeat: the coordinator is unreachable"));
    assert_eq!(rig.events.count("local:install"), 0);
}

/// A helper that does not answer leaves the host with no version it can name and not healthy;
/// when it knows only the commit, the version is the commit's; when it knows neither, the host is
/// not healthy; and the status follows the host: a health that turns bad is in a later heartbeat.
#[test]
fn the_status_follows_what_the_helper_and_the_health_check_say() {
    let rig = Rig::new();
    rig.local.world().query = Err(super::local::LocalError::Unreachable(
        "no socket".to_owned(),
    ));
    let agent = rig.agent();
    rig.run_until(&agent, |rig| rig.beats().len() >= 2);
    let first = &rig.beats()[0];
    assert_eq!(
        (first.version.as_str(), first.health),
        ("0", WireHealth::Unhealthy)
    );
    assert!(rig.logged("the host helper does not answer"));

    let rig = Rig::new();
    rig.local.world().state.version = None;
    rig.health
        .say("0.1.0+gaaaaaaaaaaaa", vec![Ok(Health::Healthy)]);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| !rig.beats().is_empty());
    assert_eq!(rig.beats()[0].version.as_str(), "0.1.0+gaaaaaaaaaaaa");
    assert_eq!(rig.beats()[0].health, WireHealth::Healthy);

    let rig = Rig::new();
    {
        let mut world = rig.local.world();
        world.state.version = None;
        world.state.commit = None;
    }
    let agent = rig.agent();
    rig.run_until(&agent, |rig| !rig.beats().is_empty());
    assert_eq!(rig.beats()[0].health, WireHealth::Unhealthy);

    // The node stops being healthy: a later heartbeat says so (the idle refresh).
    let rig = Rig::new();
    let agent = rig.agent();
    let health = rig.health.clone();
    let flipped = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&flipped);
    let stop = Stop::default();
    std::thread::scope(|scope| {
        let runner = scope.spawn(|| agent.run(&stop));
        let start = Instant::now();
        while rig.beats().is_empty() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
        health.say(VA, vec![Ok(Health::Waiting("node lost".to_owned()))]);
        *flag.lock().unwrap() = true;
        while !rig
            .beats()
            .iter()
            .any(|b| b.health == WireHealth::Unhealthy)
            && start.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        stop.stop();
        runner.join().unwrap();
    });
    assert!(*flipped.lock().unwrap());
    assert!(
        rig.beats()
            .iter()
            .any(|b| b.health == WireHealth::Unhealthy)
    );
}

/// The agent stops when it is told to, and both threads end.
#[test]
fn the_agent_stops_when_told() {
    let rig = Rig::new();
    let agent = rig.agent();
    let started = Instant::now();
    rig.run_until(&agent, |_| true);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(agent.status().version, version(VA));
}

/// A target that failed holds back only itself: an order for another version goes ahead at once.
#[test]
fn a_failed_target_does_not_hold_back_another() {
    let rig = Rig::new();
    rig.local.world().install = Err(super::local::LocalError::Refused(
        "verify_failed".to_owned(),
    ));
    ordered(&rig, VB);
    let agent = rig.agent();
    let other = "0.1.9+gdddddddddddd-5";
    let stop = Stop::default();
    std::thread::scope(|scope| {
        let runner = scope.spawn(|| agent.run(&stop));
        let start = Instant::now();
        while rig.events.count("local:install") < 1 && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
        ordered(&rig, other);
        while rig.events.count(&format!("local:install {other}")) < 1
            && start.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        stop.stop();
        runner.join().unwrap();
    });
    assert!(rig.events.count(&format!("local:install {other}")) >= 1);
    assert_eq!(
        rig.events.count(&format!("local:install {VB}")),
        1,
        "VB is not tried again"
    );
}

/// Between heartbeats the host is asked what it runs only when a heartbeat's time has passed
/// with no order: with a long heartbeat the status is not refreshed at every slice.
#[test]
fn the_status_is_refreshed_once_a_heartbeat_has_passed_with_no_order() {
    let mut rig = Rig::new();
    rig.timing.heartbeat = Duration::from_millis(120);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| rig.events.count("local:query") >= 4);
    let queries = rig.events.count("local:query");
    assert!(queries >= 4, "{queries}");
}

/// A renewer that counts its rounds and takes its time over each.
struct SlowRenewer {
    rounds: Arc<AtomicUsize>,
    takes: Duration,
    wait: Duration,
}

impl Renewer for SlowRenewer {
    fn round(&self) -> Duration {
        self.rounds.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.takes);
        self.wait
    }
}

/// The certificate's renewal runs on a thread of its own, again and again for as long as the
/// agent runs: a renewal that takes a long time (the frontend is slow) does not hold back a
/// heartbeat, and the agent still stops when told.
#[test]
fn the_renewer_runs_beside_the_heartbeats_and_does_not_hold_them_back() {
    let mut rig = Rig::new();
    let rounds = Arc::new(AtomicUsize::new(0));
    rig.renewer = Some(Arc::new(SlowRenewer {
        rounds: Arc::clone(&rounds),
        takes: Duration::from_millis(400),
        wait: Duration::from_millis(1),
    }));
    let agent = rig.agent();
    rig.run_until(&agent, |rig| {
        rounds.load(Ordering::SeqCst) >= 2 && rig.beats().len() >= 3
    });
    assert!(
        rounds.load(Ordering::SeqCst) >= 2,
        "renewed again after waiting"
    );
    assert!(
        rig.beats().len() >= 3,
        "heartbeats went on while a renewal was under way"
    );
}

/// An agent given no renewer (the files are an operator's) starts none.
#[test]
fn an_agent_with_no_renewer_has_none_to_run() {
    let rig = Rig::new();
    let agent = rig.agent();
    rig.run_until(&agent, |rig| !rig.beats().is_empty());
    assert!(!rig.beats().is_empty());
}
