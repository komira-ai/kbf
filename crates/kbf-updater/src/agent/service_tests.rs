//! An agent that runs services besides `kbf`, over fakes: what its heartbeat says, how an order
//! for a service is run with that service's ports, that one A/B runs at a time on the host
//! whichever services they are for, and that a failure is that service's alone. And the
//! readiness URL.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kbf_types::ReleasePlatform;

use super::build::no_drain;
use super::flags::Timing;
use super::run::{Agent, ServicePorts, Stop, World};
use super::service_health::ReadyUrl;
use super::status::Status;
use super::testing::{
    Events, FakeCoordinator, FakeDrain, FakeHealth, FakeLocal, ManualClock, VA, VB, host, version,
};
use crate::inventory::Role;
use crate::ports::{Health, HealthCheck, PortError};
use crate::wire::{Health as WireHealth, Heartbeat, Outcome, Reply, ServiceBeat, ServiceReply};

fn timing() -> Timing {
    Timing {
        heartbeat: Duration::from_millis(5),
        drain_poll: Duration::from_millis(1),
        health_poll: Duration::from_millis(1),
        retry: Duration::from_secs(3600),
        ..Timing::DEFAULT
    }
}

/// The fakes of one service (or of `kbf`), with an event log of their own.
struct Fakes {
    events: Events,
    local: FakeLocal,
    drain: FakeDrain,
    health: FakeHealth,
}

impl Fakes {
    fn new() -> Self {
        let events = Events::default();
        let health = FakeHealth::new(&events);
        health.say(VA, vec![Ok(Health::Healthy)]);
        health.say(VB, vec![Ok(Health::Healthy)]);
        Self {
            local: FakeLocal::new(&events),
            drain: FakeDrain::new(&events),
            health,
            events,
        }
    }

    fn ports(&self, name: &str, channel: &str) -> ServicePorts {
        ServicePorts {
            name: name.to_owned(),
            channel: channel.to_owned(),
            local: Arc::new(self.local.clone()),
            drain: Arc::new(self.drain.clone()),
            health: Arc::new(self.health.clone()),
        }
    }
}

struct Rig {
    kbf: Fakes,
    db: Fakes,
    cache: Fakes,
    coordinator: FakeCoordinator,
    logs: Arc<Mutex<Vec<String>>>,
}

fn update(service: Option<&str>, target: &str) -> ServiceReply {
    ServiceReply::Update {
        service: service.map(str::to_owned),
        version: version(target),
    }
}

impl Rig {
    fn new() -> Self {
        Self {
            kbf: Fakes::new(),
            db: Fakes::new(),
            cache: Fakes::new(),
            coordinator: FakeCoordinator::default(),
            logs: Arc::default(),
        }
    }

    fn world(&self) -> World {
        let logs = Arc::clone(&self.logs);
        World {
            coordinator: Arc::new(self.coordinator.clone()),
            local: Arc::new(self.kbf.local.clone()),
            drain: Arc::new(self.kbf.drain.clone()),
            health: Arc::new(self.kbf.health.clone()),
            clock: Arc::new(ManualClock::default()),
            unix_now: Arc::new(|| 10_000),
            renewer: None,
            log: Arc::new(move |line| logs.lock().unwrap().push(line.to_owned())),
        }
    }

    fn kbf_only(&self) -> Agent {
        Agent::new(
            host(Role::Daemon),
            "stable".to_owned(),
            timing(),
            self.world(),
            Status::unknown(ReleasePlatform::LinuxX86_64, version("0")),
        )
    }

    /// An agent that runs `db` (channel `db-ch`, the node drained) and `cache` (channel
    /// `cache-ch`, nothing to drain).
    fn agent(&self) -> Agent {
        self.kbf_only().with_services(vec![
            self.db.ports("db", "db-ch"),
            ServicePorts {
                drain: no_drain(),
                ..self.cache.ports("cache", "cache-ch")
            },
        ])
    }

    /// Run the agent until `done`, for no more than ten seconds.
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

/// The service `name` in a heartbeat.
fn beat_of<'a>(beat: &'a Heartbeat, name: &str) -> &'a ServiceBeat {
    beat.services
        .as_ref()
        .and_then(|all| all.iter().find(|b| b.name == name))
        .expect("the service is in the heartbeat")
}

/// Whether the last heartbeat says the service `name` last ended as `outcome`.
fn ended(rig: &Rig, name: &str, outcome: Outcome) -> bool {
    rig.beats().last().is_some_and(|b| {
        b.services.as_ref().is_some_and(|all| {
            all.iter().any(|s| {
                s.name == name && s.last_update.as_ref().is_some_and(|u| u.outcome == outcome)
            })
        })
    })
}

/// An agent given no service sends the heartbeat of today, byte for byte (no `services`), reads
/// the reply with the strict reader (an order in the shape of today's runs; the reply that names
/// a service is never asked for), and has no other service's state.
#[test]
fn an_agent_with_no_services_is_the_agent_of_today() {
    let rig = Rig::new();
    rig.coordinator.reply(Ok(Reply::Update {
        version: version(VB),
    }));
    let agent = rig.kbf_only();
    rig.run_until(&agent, |rig| {
        rig.beats().iter().any(|b| b.last_update.is_some())
    });
    let first = serde_json::to_string(&rig.beats()[0]).unwrap();
    assert_eq!(
        first,
        r#"{"platform":"linux-x86_64","version":"0.1.5+gaaaaaaaaaaaa-2","health":"healthy","last_update":null,"updating":null}"#
    );
    assert!(rig.beats().iter().all(|b| b.services.is_none()));
    // The order read by the strict reader ran, with the agent's channel.
    assert_eq!(
        rig.kbf
            .events
            .count("local:install 0.1.7+gbbbbbbbbbbbb-3 stable"),
        1
    );
    assert_eq!(agent.service_status("db"), None);
    assert_eq!(agent.status().version, version(VB));
}

/// An agent given services lists each in the heartbeat with the version and health its own helper
/// and health check say, and the top level stays `kbf`'s.
#[test]
fn a_two_service_agent_reports_both() {
    let rig = Rig::new();
    rig.cache.local.world().state.version = Some(version(VB));
    rig.cache
        .health
        .say(VB, vec![Ok(Health::Waiting("warming".to_owned()))]);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| !rig.beats().is_empty());
    let first = &rig.beats()[0];
    assert_eq!(
        (first.version.clone(), first.health),
        (version(VA), WireHealth::Healthy)
    );
    let names: Vec<_> = first
        .services
        .as_ref()
        .unwrap()
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, ["db", "cache"]);
    let db = beat_of(first, "db");
    assert_eq!(
        (db.version.clone(), db.health),
        (version(VA), WireHealth::Healthy)
    );
    assert_eq!((db.last_update.clone(), db.updating.clone()), (None, None));
    let cache = beat_of(first, "cache");
    assert_eq!(
        (cache.version.clone(), cache.health),
        (version(VB), WireHealth::Unhealthy)
    );
    assert!(rig.logged("cache: the host is not healthy: not yet healthy on"));
    assert_eq!(agent.service_status("db").unwrap().version, version(VA));
}

/// An order that names `db` runs the A/B with `db`'s ports: its channel, its helper, its drain,
/// its health. `kbf`'s ports and `cache`'s are not used for it, and the result is `db`'s alone.
#[test]
fn an_order_for_a_service_runs_with_that_services_ports() {
    let rig = Rig::new();
    rig.coordinator.script(vec![update(Some("db"), VB)]);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| ended(rig, "db", Outcome::Succeeded));
    let db = rig.db.events.all();
    let at = |needle: &str| {
        db.iter()
            .position(|e| e.starts_with(needle))
            .unwrap_or(usize::MAX)
    };
    assert!(
        at("local:install 0.1.7+gbbbbbbbbbbbb-3 db-ch") < at("drain n1"),
        "{db:?}"
    );
    assert!(at("drain n1") < at("local:switch"), "{db:?}");
    assert!(at("local:switch") < at("health:held"), "{db:?}");
    assert!(at("health:held") < at("local:confirm"), "{db:?}");
    assert!(at("local:confirm") < at("undrain"), "{db:?}");
    for events in [&rig.kbf.events, &rig.cache.events] {
        assert_eq!(
            events.of(&[
                "local:install",
                "local:switch",
                "local:confirm",
                "drain",
                "undrain"
            ]),
            Vec::<String>::new()
        );
    }
    let last = rig.beats().into_iter().last().unwrap();
    assert_eq!(last.last_update, None);
    assert_eq!(last.version, version(VA));
    let db = beat_of(&last, "db");
    assert_eq!(
        (db.version.clone(), db.updating.clone()),
        (version(VB), None)
    );
    assert_eq!(db.last_update.as_ref().unwrap().target, version(VB));
}

/// A service that declares no drain is replaced without any drain: `kbf`'s drain port is not
/// called for it (nor its own, which is the no-drain port), and the A/B still runs to the end.
#[test]
fn a_service_with_no_drain_is_updated_without_calling_any_drain() {
    let rig = Rig::new();
    rig.coordinator.script(vec![update(Some("cache"), VB)]);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| ended(rig, "cache", Outcome::Succeeded));
    assert_eq!(
        rig.kbf.events.of(&["drain", "leases", "undrain"]),
        Vec::<String>::new()
    );
    assert_eq!(
        rig.db.events.of(&["drain", "leases", "undrain"]),
        Vec::<String>::new()
    );
    assert_eq!(
        rig.cache
            .events
            .count("local:install 0.1.7+gbbbbbbbbbbbb-3 cache-ch"),
        1
    );
    assert_eq!(rig.cache.events.count("local:confirm"), 1);
    assert_eq!(
        rig.cache.events.of(&["drain", "leases", "undrain"]),
        Vec::<String>::new()
    );
}

/// An update of `db` that does not come up healthy is put back by the helper, its node is given
/// back, and the failure is `db`'s: `kbf`'s and `cache`'s states, ports and results are untouched.
#[test]
fn a_failing_service_reverts_and_reports_failed_for_that_service_only() {
    let rig = Rig::new();
    rig.db.health.say(
        VB,
        vec![Ok(Health::Abandoned("it will not serve".to_owned()))],
    );
    rig.coordinator.script(vec![update(Some("db"), VB)]);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| ended(rig, "db", Outcome::Failed));
    assert_eq!(rig.db.events.count("local:revert"), 1);
    assert_eq!(rig.db.events.count("local:confirm"), 0);
    assert!(
        !rig.db.drain.is_drained(),
        "the node is given back on the old release"
    );
    let last = rig.beats().into_iter().last().unwrap();
    let db = beat_of(&last, "db");
    assert_eq!(db.version, version(VA));
    assert_eq!(db.last_update.as_ref().unwrap().target, version(VB));
    assert_eq!(db.health, WireHealth::Healthy);
    assert_eq!(last.last_update, None);
    assert_eq!(beat_of(&last, "cache").last_update, None);
    assert!(rig.logged("db: update to"));
    for events in [&rig.kbf.events, &rig.cache.events] {
        assert_eq!(
            events.of(&["local:install", "local:revert", "drain"]),
            Vec::<String>::new()
        );
    }
}

/// One A/B at a time on the host: orders for `db` and `cache` that arrive while `kbf` updates are
/// not started until `kbf`'s update is over (its node is given back), and then run one after the
/// other.
#[test]
fn an_order_for_a_service_waits_while_kbf_updates() {
    let rig = Rig::new();
    rig.coordinator.script(vec![
        update(None, VB),
        update(Some("db"), VB),
        update(Some("cache"), VB),
    ]);
    // `kbf`'s install returns only after the order for `db` was handed to the agent.
    {
        let coordinator = rig.coordinator.clone();
        rig.kbf.local.world().on_install = Some(Arc::new(move || {
            let start = Instant::now();
            while coordinator.delivered() < 3 && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }));
    }
    // `db`'s install writes into `kbf`'s log, so the two are in one order.
    {
        let kbf_events = rig.kbf.events.clone();
        rig.db.local.world().on_install = Some(Arc::new(move || kbf_events.push("db:install")));
        let kbf_events = rig.kbf.events.clone();
        rig.cache.local.world().on_install =
            Some(Arc::new(move || kbf_events.push("cache:install")));
    }
    let agent = rig.agent();
    rig.run_until(&agent, |rig| {
        ended(rig, "db", Outcome::Succeeded) && ended(rig, "cache", Outcome::Succeeded)
    });
    assert_eq!(rig.coordinator.delivered(), 3);
    let all = rig.kbf.events.all();
    let at = |needle: &str| all.iter().position(|e| e == needle).unwrap_or(usize::MAX);
    assert!(at("undrain") < at("db:install"), "{all:?}");
    assert!(
        at("local:confirm 0.1.7+gbbbbbbbbbbbb-3") < at("db:install"),
        "{all:?}"
    );
    assert_eq!(agent.status().version, version(VB));
    assert_eq!(agent.service_status("db").unwrap().version, version(VB));
}

/// The back-off is per service: `db` failing a target is not tried again for it before the retry
/// time, and that does not hold `kbf` or `cache` back from the same target.
#[test]
fn a_failed_target_backs_off_for_that_service_only() {
    let rig = Rig::new();
    rig.db
        .health
        .say(VB, vec![Ok(Health::Abandoned("no".to_owned()))]);
    rig.coordinator.script(vec![update(Some("db"), VB)]);
    let agent = rig.agent();
    let stop = Stop::default();
    std::thread::scope(|scope| {
        let runner = scope.spawn(|| agent.run(&stop));
        let wait = |done: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !done() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        wait(&|| ended(&rig, "db", Outcome::Failed));
        // The same order again for `db`, then the same target for `kbf` and for `cache`.
        rig.coordinator.script(vec![
            update(Some("db"), VB),
            update(None, VB),
            update(Some("cache"), VB),
        ]);
        wait(&|| {
            agent.status().last_update.is_some()
                && agent.service_status("cache").unwrap().last_update.is_some()
                && rig.logged("db: update to 0.1.7+gbbbbbbbbbbbb-3: failed a moment ago")
        });
        stop.stop();
        runner.join().unwrap();
    });
    assert_eq!(rig.db.events.count("local:install"), 1);
    assert_eq!(rig.kbf.events.count("local:install"), 1);
    assert_eq!(rig.cache.events.count("local:install"), 1);
    assert_eq!(
        agent.status().last_update.unwrap().outcome,
        Outcome::Succeeded
    );
}

/// An order for a service the agent does not run is logged and nothing is touched; an order that
/// names `kbf` is `kbf`'s.
#[test]
fn an_order_for_an_unknown_service_is_ignored_and_one_for_kbf_is_kbfs() {
    let rig = Rig::new();
    rig.coordinator
        .script(vec![update(Some("other"), VB), update(Some("kbf"), VB)]);
    let agent = rig.agent();
    rig.run_until(&agent, |rig| {
        rig.beats().iter().any(|b| b.last_update.is_some())
    });
    assert!(rig.logged("an order for other, which this agent does not run"));
    assert_eq!(
        rig.kbf
            .events
            .count("local:install 0.1.7+gbbbbbbbbbbbb-3 stable"),
        1
    );
    for events in [&rig.db.events, &rig.cache.events] {
        assert_eq!(events.count("local:install"), 0);
    }
}

/// A restart in the middle of a service's trial finishes that trial from the health step, with
/// the service's helper: nothing is fetched, and the first heartbeat says the service is updating.
#[test]
fn a_trial_the_helper_has_for_a_service_is_finished_on_start() {
    let rig = Rig::new();
    {
        let mut world = rig.db.local.world();
        world.state.version = Some(version(VB));
        world.state.pending = Some(super::local::Trial {
            version: Some(version(VB)),
            commit: super::testing::COMMIT_B.to_owned(),
            deadline_unix: Some(10_600),
        });
        // The confirm waits for the first heartbeat, so that heartbeat is sent mid-trial.
        let coordinator = rig.coordinator.clone();
        world.on_confirm = Some(Arc::new(move || {
            let start = Instant::now();
            while coordinator.beats().is_empty() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }));
    }
    let agent = rig.agent();
    rig.run_until(&agent, |rig| ended(rig, "db", Outcome::Succeeded));
    assert_eq!(rig.db.events.count("local:install"), 0);
    assert_eq!(rig.db.events.count("local:confirm"), 1);
    let first = &rig.beats()[0];
    assert_eq!(beat_of(first, "db").updating, Some(version(VB)));
    assert_eq!(first.updating, None);
}

/// When a service's helper does not answer, that service is unhealthy in the heartbeat, with the
/// reason logged under its name, and the others are not.
#[test]
fn a_service_whose_helper_does_not_answer_is_unhealthy_alone() {
    let rig = Rig::new();
    rig.cache.local.world().query = Err(super::local::LocalError::Unreachable("gone".to_owned()));
    let agent = rig.agent();
    rig.run_until(&agent, |rig| !rig.beats().is_empty());
    let first = &rig.beats()[0];
    assert_eq!(beat_of(first, "cache").health, WireHealth::Unhealthy);
    assert_eq!(beat_of(first, "db").health, WireHealth::Healthy);
    assert_eq!(first.health, WireHealth::Healthy);
    assert!(rig.logged("cache: the host is not healthy: the host helper does not answer"));
}

// The readiness URL.

/// A server on loopback that answers each connection with the next of `answers`, and the request
/// lines it saw.
fn serve(answers: Vec<&'static [u8]>) -> (SocketAddr, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for answer in answers {
            let (mut tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = [0_u8; 1024];
            let n = tcp.read(&mut buf).unwrap();
            seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
            tcp.write_all(answer).unwrap();
        }
        seen
    });
    (addr, handle)
}

/// 2xx on the URL's path is healthy; any other status is "not yet" with the status in the text;
/// what is not a status line, and a port nobody listens on, are errors. `kbf`'s held and plain
/// checks are the same, and the call is a `GET` of the path with `Connection: close`.
#[test]
fn a_readiness_url_is_healthy_when_it_answers_2xx() {
    let (addr, server) = serve(vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        b"HTTP/1.1 204 No Content\r\n\r\n",
        b"HTTP/1.1 503 Service Unavailable\r\n\r\n",
        b"not http at all\r\n",
        b"HTTP/1.1 abc\r\n",
        b"",
    ]);
    let ready = ReadyUrl::new(addr, "/readyz".to_owned());
    let h = host(Role::Daemon);
    let v = version(VB);
    assert_eq!(ready.refusal(&h), None);
    assert_eq!(ready.check(&h, &v), Ok(Health::Healthy));
    assert_eq!(ready.check_held(&h, &v), Ok(Health::Healthy));
    assert_eq!(
        ready.check(&h, &v),
        Ok(Health::Waiting(
            "the readiness URL answers HTTP 503".to_owned()
        ))
    );
    for _ in 0..3 {
        assert!(matches!(ready.check(&h, &v), Err(PortError { .. })));
    }
    let seen = server.join().unwrap();
    assert!(seen[0].starts_with("GET /readyz HTTP/1.1\r\n"), "{seen:?}");
    assert!(seen[0].contains("Connection: close\r\n"));
    // Nobody listens any more.
    assert!(ready.check(&h, &v).is_err());
}
