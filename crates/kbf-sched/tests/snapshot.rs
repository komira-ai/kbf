//! The scheduler's snapshot (`Scheduler::snapshot`): the leases it holds and the queue
//! it keeps, as plain values for the operator API.

use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{
    DaemonInstance, Event, Input, LeasePhase, MAX_AVOIDED, Request, Scheduler, platform_label,
};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseKind, Qos,
    Resources, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn linux() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
}

fn request(n: u8, qos: Qos) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(n),
        },
        qos,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn secs(s: u64) -> FarmTime {
    FarmTime::from_millis(s * 1_000)
}

struct Harness {
    s: Scheduler,
    now: u64,
}

impl Harness {
    fn new() -> Self {
        Self {
            s: Scheduler::new(7),
            now: 0,
        }
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(secs(self.now), event))
    }

    fn worker(&mut self, name: &str, cpus: u64) {
        let up = Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(cpus * 1_000, 64 * GIB),
            caps: linux(),
        };
        assert!(self.feed(up).is_empty());
    }

    fn submit(&mut self, n: u8, qos: Qos) {
        let event = Event::Submit {
            waiter: WaiterId(u64::from(n)),
            request: request(n, qos),
        };
        assert!(self.feed(event).is_empty());
    }

    /// A tick; returns the grants it proposed.
    fn tick(&mut self) -> Vec<LeaseGrant> {
        self.feed(Event::Tick)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Commit(ControlRecord::Lease(g)) => Some(g),
                _ => None,
            })
            .collect()
    }

    fn commit(&mut self, grant: &LeaseGrant) {
        let effects = self.feed(Event::Committed(ControlRecord::Lease(grant.clone())));
        assert!(matches!(effects.as_slice(), [Effect::Start(_)]));
    }

    fn start(&mut self, grant: &LeaseGrant) {
        let started = Event::Started {
            operation: grant.operation,
            lease: grant.lease,
        };
        assert!(self.feed(started).is_empty());
    }

    fn heartbeat(&mut self, name: &str) {
        let event = Event::Heartbeat {
            worker: w(name),
            running: self.s.leases_on(&w(name)),
        };
        self.feed(event);
    }
}

/// Catches a lease view that mixes up its fields or its phase: the operation, action
/// digest, instance, node, QoS and platform of the lease, the three phases a lease
/// passes (proposed before its grant commits, granted after, running once the worker
/// says so), the grant and start times, and the two ages (since the grant, and since
/// the phase began). Catches: age measured from the start; the phase age measured from
/// the grant; the start time not recorded; `Proposed` and `Granted` swapped.
#[test]
fn a_lease_is_viewed_through_its_phases() {
    let mut h = Harness::new();
    h.worker("a", 4);
    h.now = 10;
    h.submit(3, Qos::Interactive);
    let [grant] = h.tick().try_into().unwrap();

    h.now = 12;
    let [lease] = h.s.snapshot().leases.try_into().unwrap();
    assert_eq!(lease.lease, grant.lease);
    assert_eq!(lease.operation, grant.operation);
    assert_eq!(lease.instance, "main");
    assert_eq!(lease.action, digest(3));
    assert_eq!(lease.node, w("a"));
    assert_eq!(lease.qos, Qos::Interactive);
    assert_eq!(lease.platform, "any");
    assert_eq!(lease.phase, LeasePhase::Proposed);
    assert_eq!(lease.granted_at, secs(10));
    assert_eq!(lease.started_at, None);
    // The scheduler's newest input was the tick at 10: ages are as of then.
    assert_eq!(lease.age, Duration::ZERO);

    h.commit(&grant);
    let [lease] = h.s.snapshot().leases.try_into().unwrap();
    assert_eq!(lease.phase, LeasePhase::Granted);

    h.now = 15;
    h.start(&grant);
    h.now = 30;
    h.heartbeat("a");
    let snapshot = h.s.snapshot();
    assert_eq!((snapshot.term, snapshot.now), (7, secs(30)));
    let [lease] = snapshot.leases.try_into().unwrap();
    assert_eq!(lease.phase, LeasePhase::Running);
    assert_eq!(lease.granted_at, secs(10));
    assert_eq!(lease.started_at, Some(secs(15)));
    assert_eq!(lease.age, Duration::from_secs(20));
    assert_eq!(lease.phase_age, Duration::from_secs(15));
    assert_eq!(lease.phase.name(), "running");
    assert_eq!(LeasePhase::Proposed.name(), "proposed");
    assert_eq!(LeasePhase::Granted.name(), "granted");
}

/// Catches a node filter that leaks another node's leases, drops its own, or errors on
/// a node the scheduler does not know, and a view not in lease order or holding a lease
/// that ended. Catches: the filter ignored; the filter inverted.
#[test]
fn leases_filter_by_node_and_end_with_their_operation() {
    let mut h = Harness::new();
    h.worker("a", 1);
    h.worker("b", 1);
    h.submit(1, Qos::Ci);
    h.submit(2, Qos::Ci);
    let grants = h.tick();
    assert_eq!(grants.len(), 2);
    for g in &grants {
        h.commit(g);
        h.start(g);
    }
    let all = h.s.lease_snapshot(None);
    assert_eq!(
        all.iter().map(|l| l.lease).collect::<Vec<_>>(),
        grants.iter().map(|g| g.lease).collect::<Vec<_>>(),
        "in lease order"
    );
    for l in &all {
        let node = l.node.clone();
        let own = h.s.lease_snapshot(Some(&node));
        assert_eq!(own.len(), 1, "{node}");
        assert_eq!(own[0].lease, l.lease);
        assert_eq!(h.s.leases_on(&node), [l.lease]);
    }
    assert!(h.s.lease_snapshot(Some(&w("nobody"))).is_empty());

    let first = &grants[0];
    h.feed(Event::Report {
        operation: first.operation,
        lease: first.lease,
        outcome: kbf_types::Outcome::Completed {
            action_result: digest(99),
        },
    });
    h.feed(Event::Committed(ControlRecord::Result(
        kbf_types::ResultRecord {
            lease: first.lease,
            operation: first.operation,
            outcome: kbf_types::Outcome::Completed {
                action_result: digest(99),
            },
        },
    )));
    let left = h.s.lease_snapshot(None);
    assert_eq!(left.len(), 1);
    assert_ne!(left[0].lease, first.lease);
}

/// Catches a queue summary that miscounts, loses the platform or QoS split, orders its
/// groups wrongly (platform, then the more urgent QoS first), or ages from the wrong
/// moment: the oldest age is of the longest waiter overall and per group. Catches: the
/// age taken from the newest instead of the oldest; depth counting the running lease;
/// groups keyed by QoS only.
#[test]
fn the_queue_is_counted_and_aged_per_platform_and_qos() {
    let mut h = Harness::new();
    h.worker("a", 1);
    h.submit(1, Qos::Ci);
    let [running] = h.tick().try_into().unwrap();
    h.commit(&running);
    h.start(&running);
    assert_eq!(h.s.queue_snapshot().depth, 0);
    assert_eq!(h.s.queue_snapshot().oldest_age, None);
    assert!(h.s.queue_snapshot().groups.is_empty());

    h.now = 100;
    h.heartbeat("a");
    h.submit(2, Qos::Batch);
    h.now = 110;
    h.heartbeat("a");
    h.submit(3, Qos::Ci);
    h.now = 120;
    h.heartbeat("a");
    h.submit(4, Qos::Batch);
    h.now = 130;
    let mut needs_x86 = request(5, Qos::Ci);
    needs_x86.needs = kbf_caps::Request::parse([("arch", "x86_64")]).unwrap();
    assert!(
        h.feed(Event::Submit {
            waiter: WaiterId(5),
            request: needs_x86,
        })
        .is_empty()
    );
    h.heartbeat("a");
    h.now = 140;
    h.heartbeat("a");
    h.feed(Event::Tick);

    let queue = h.s.queue_snapshot();
    assert_eq!(queue.depth, 4);
    assert_eq!(queue.depth, h.s.queued_len());
    assert_eq!(queue.oldest_age, Some(Duration::from_secs(40)));
    let shape: Vec<_> = queue
        .groups
        .iter()
        .map(|g| {
            (
                g.platform.as_str(),
                g.qos.clone(),
                g.depth,
                g.oldest_age.as_secs(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("any", Qos::Ci, 1, 30),
            ("any", Qos::Batch, 2, 40),
            ("arch=x86_64", Qos::Ci, 1, 10),
        ]
    );
    assert!(queue.waiting.is_empty(), "a worker can run all of it");
    assert!(queue.avoided.is_empty());
    assert_eq!(queue.avoided_total, 0);
}

/// Catches a requeued operation aged from its first submission: it waits in the queue
/// again from the moment its lease was given up, and the age of a silent node's work is
/// not the time it ran. Catches: `queued_at` not reset on a requeue.
#[test]
fn a_requeued_operation_waits_from_its_give_up() {
    let mut h = Harness::new();
    h.worker("a", 1);
    h.submit(1, Qos::Ci);
    let [grant] = h.tick().try_into().unwrap();
    h.commit(&grant);
    h.start(&grant);
    // `a` falls silent; its lease is given up past the lease grace.
    h.now = 120;
    h.feed(Event::Tick);
    assert_eq!(h.s.queued_len(), 1, "given up and queued");
    h.now = 150;
    h.feed(Event::Tick);
    let queue = h.s.queue_snapshot();
    assert_eq!(queue.depth, 1);
    assert_eq!(queue.oldest_age, Some(Duration::from_secs(30)));
}

/// Catches work no worker can run reported without its reason, grouped wrongly, or
/// counted outside the depth: the reason, the count of operations that wait for it, and
/// the first of them in queue order as the example. Catches: example the last instead of
/// the first; counts per operation reset.
#[test]
fn work_nobody_can_run_is_grouped_by_its_reason() {
    let mut h = Harness::new();
    h.worker("a", 4);
    for (n, os) in [(1u8, "macos"), (2, "macos"), (3, "windows")] {
        let mut r = request(n, Qos::Ci);
        r.needs = kbf_caps::Request::parse([("os", os)]).unwrap();
        h.feed(Event::Submit {
            waiter: WaiterId(u64::from(n)),
            request: r,
        });
    }
    h.submit(4, Qos::Ci);
    h.now = 5;
    let [grant] = h.tick().try_into().unwrap();
    assert_eq!(grant.operation, h.s.snapshot().leases[0].operation);

    let queue = h.s.queue_snapshot();
    assert_eq!(queue.depth, 3, "the runnable one was granted");
    let counts: Vec<usize> = queue.waiting.iter().map(|g| g.count).collect();
    assert_eq!(counts.iter().sum::<usize>(), 3);
    assert_eq!(queue.waiting.len(), 2, "two reasons: macos and windows");
    let mac = queue
        .waiting
        .iter()
        .find(|g| g.count == 2)
        .expect("two wait for a mac");
    assert!(mac.reason.contains("macos"), "{}", mac.reason);
    assert_eq!(mac.example_instance, "main");
    assert_eq!(mac.example_action, digest(1), "the first in queue order");
    assert_eq!(mac.example.0, 0);
    for group in &queue.waiting {
        assert_eq!(
            h.s.waiting(group.example),
            Some(group.reason.as_str()),
            "the example waits for the reason"
        );
    }
    let reasons: Vec<&str> = queue.waiting.iter().map(|g| g.reason.as_str()).collect();
    let mut sorted = reasons.clone();
    sorted.sort_unstable();
    assert_eq!(reasons, sorted, "in reason order");
}

/// A scheduler where `n` operations were granted to `a`, never started, and given up as
/// not started, so placement avoids `a` for each; `a` is then cordoned so they stay
/// queued.
fn avoiding(n: u8) -> Harness {
    let mut h = Harness::new();
    h.worker("a", u64::from(n));
    for i in 0..n {
        h.submit(i, Qos::Ci);
    }
    let grants = h.tick();
    assert_eq!(grants.len(), usize::from(n));
    for g in &grants {
        h.commit(g);
    }
    // The node heartbeats past the start grace without ever listing them.
    h.now = 61;
    h.feed(Event::Heartbeat {
        worker: w("a"),
        running: Vec::new(),
    });
    h.feed(Event::Cordon { worker: w("a") });
    h.now = 62;
    h.feed(Event::Tick);
    h
}

/// Catches the workers placement avoids left out of the queue view, or listed for an
/// operation that is not queued: the operation, its action and the nodes, in operation
/// order. Catches: `avoided` empty; nodes of a running operation listed.
#[test]
fn queued_work_names_the_workers_it_avoids() {
    let h = avoiding(3);
    let queue = h.s.queue_snapshot();
    assert_eq!(queue.depth, 3);
    assert_eq!(queue.avoided_total, 3);
    assert_eq!(queue.avoided.len(), 3);
    for (i, a) in queue.avoided.iter().enumerate() {
        assert_eq!(a.operation.0, i as u64, "operation order");
        assert_eq!(a.nodes, [w("a")]);
        assert_eq!(a.instance, "main");
        assert_eq!(a.action, digest(u8::try_from(i).unwrap()));
    }
    // The same data the scheduler's own accessor holds.
    assert_eq!(h.s.avoided().len(), 3);
    // They wait for the cordon, which is a reason too.
    assert_eq!(queue.waiting.len(), 1);
    assert_eq!(queue.waiting[0].count, 3);
}

/// Catches an unbounded avoided list: past the cap only the first operations are
/// listed, and the total says how many there were.
#[test]
fn the_avoided_list_is_capped_and_the_total_is_not() {
    let n = u8::try_from(MAX_AVOIDED + 5).unwrap();
    let h = avoiding(n);
    let queue = h.s.queue_snapshot();
    assert_eq!(queue.avoided_total, MAX_AVOIDED + 5);
    assert_eq!(queue.avoided.len(), MAX_AVOIDED);
    assert_eq!(
        queue.avoided.last().unwrap().operation.0,
        MAX_AVOIDED as u64 - 1
    );
    assert_eq!(queue.depth, MAX_AVOIDED + 5);
}

/// Catches a platform label that is not stable or loses a requirement: the empty request
/// is `any`, and a request is its properties, `key=value`, joined by commas.
#[test]
fn a_platform_is_labelled_by_its_properties() {
    assert_eq!(platform_label(&kbf_caps::Request::default()), "any");
    let r = kbf_caps::Request::parse([("os", "linux"), ("arch", "x86_64")]).unwrap();
    assert_eq!(platform_label(&r), "arch=x86_64,os=linux");
}
