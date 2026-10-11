//! Node phases and whole-machine leases (`docs/design/scheduler.md#node-phases`): a
//! Mac that switches its GUI session for a whole-machine lease restarts its daemon, so
//! it is silent for minutes. The scheduler keeps that lease for the session switch
//! grace instead of the lease grace G, sends its `Start` again to the daemon that comes
//! back naming it, and offers a worker that is switching or quarantined no lease.

use kbf_caps::NodeCaps;
use kbf_sched::fence::{LEASE_GRACE, SESSION_SWITCH_GRACE};
use kbf_sched::{
    DaemonInstance, Event, Input, NodePhase, OpState, Request, RequeueReason, Scheduler,
    UNSERVABLE_WAIT,
};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseId,
    LeaseKind, Qos, Resources, StartLease, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
const MAC: Resources = Resources::new(12_000, 32 * GIB);
const MS: u64 = 1;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn request(n: u8, kind: LeaseKind) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(n),
        },
        qos: Qos::Ci,
        kind,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).expect("fits")
}

/// A scheduler whose farm time the test moves.
struct Harness {
    sched: Scheduler,
    now: u64,
}

impl Harness {
    fn new() -> Self {
        Self {
            sched: Scheduler::new(1).recording_requeues(),
            now: 1_000,
        }
    }

    fn at(&mut self, millis: u64) -> &mut Self {
        assert!(millis >= self.now, "farm time goes forward");
        self.now = millis;
        self
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.sched
            .apply(Input::new(FarmTime::from_millis(self.now), event))
    }

    /// Registers `name` as daemon process `instance`, running the native and the
    /// whole-machine drivers.
    fn register(&mut self, name: &str, instance: &str) {
        let caps = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
            .unwrap()
            .with_drivers(["native", "native-whole-machine"]);
        let effects = self.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(instance),
            predecessor_stopped: false,
            capacity: MAC,
            caps,
        });
        assert!(effects.is_empty(), "{effects:?}");
    }

    fn phase(&mut self, name: &str, phase: NodePhase) -> Vec<Effect> {
        self.feed(Event::Phase {
            worker: w(name),
            phase,
        })
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    /// Ticks; the grants proposed, and the reasons given to waiting operations.
    fn tick(&mut self) -> (Vec<LeaseGrant>, Vec<String>) {
        let mut grants = Vec::new();
        let mut reasons = Vec::new();
        for effect in self.feed(Event::Tick) {
            match effect {
                Effect::Commit(ControlRecord::Lease(g)) => grants.push(g),
                Effect::Waiting(waiting) => reasons.extend(waiting.reason),
                other => panic!("a tick proposed {other:?}"),
            }
        }
        (grants, reasons)
    }

    /// Commits `grant` and marks it started; the `Start` it emitted.
    fn start(&mut self, grant: &LeaseGrant) -> StartLease {
        let effects = self.feed(Event::Committed(ControlRecord::Lease(grant.clone())));
        let [Effect::Start(start)] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        self.feed(Event::Started {
            operation: grant.operation,
            lease: grant.lease,
        });
        start.clone()
    }

    fn heartbeat(&mut self, name: &str, running: &[LeaseId]) {
        let effects = self.feed(Event::Heartbeat {
            worker: w(name),
            running: running.to_vec(),
        });
        assert!(effects.is_empty(), "{effects:?}");
    }

    /// A whole-machine lease placed on `mac`, started, and switching: `Preparing` at
    /// the current time.
    fn preparing(&mut self) -> LeaseGrant {
        self.register("mac", "first");
        self.submit(1, request(1, LeaseKind::WholeMachine));
        let (grants, _) = self.tick();
        let [grant] = grants.as_slice() else {
            panic!("{grants:?}");
        };
        let start = self.start(grant);
        assert_eq!(
            start.resources, MAC,
            "a whole-machine lease books the whole Mac"
        );
        assert_eq!(self.phase("mac", NodePhase::Preparing(grant.lease)), []);
        grant.clone()
    }

    fn held(&self) -> Vec<LeaseId> {
        self.sched.leases_on(&w("mac"))
    }

    fn requeues(&mut self) -> Vec<(LeaseId, RequeueReason)> {
        self.sched
            .take_requeues()
            .into_iter()
            .map(|r| (r.lease, r.reason))
            .collect()
    }
}

/// Catches a scheduler that gives up a switching whole-machine lease at the lease
/// grace G, as for any silent worker (the mutant "requeue on any disconnect"): the
/// daemon's userspace restart would requeue every GUI lease and run it twice. Also
/// catches a grace that never ends: past the session switch grace a worker that never
/// came back loses the lease, with its own reason.
#[test]
fn a_switching_lease_outlives_g_while_silent_and_is_requeued_after_the_switch_grace() {
    let mut h = Harness::new();
    let grant = h.preparing();
    let since = h.now;

    h.at(since + millis(LEASE_GRACE) + 1_000).tick();
    assert_eq!(h.held(), [grant.lease], "given up at G");
    h.at(since + millis(SESSION_SWITCH_GRACE) - MS).tick();
    assert_eq!(
        h.held(),
        [grant.lease],
        "given up before the switch grace ended"
    );
    assert_eq!(h.requeues(), []);

    h.at(since + millis(SESSION_SWITCH_GRACE)).tick();
    assert_eq!(h.held(), []);
    assert_eq!(h.requeues(), [(grant.lease, RequeueReason::SessionSwitch)]);
    assert_eq!(h.sched.state(grant.operation), Some(&OpState::Queued));
}

/// Catches a lease kept past G that is not the one the worker switches for: only the
/// named lease outlives the lease grace. Here the worker names another lease than the
/// one it holds, so its own is given up at G as usual.
#[test]
fn only_the_named_lease_outlives_g() {
    let mut h = Harness::new();
    let grant = h.preparing();
    let other = LeaseId::new(1, grant.lease.seq + 7);
    h.phase("mac", NodePhase::Preparing(other));
    let since = h.now;
    h.at(since + millis(LEASE_GRACE)).tick();
    assert_eq!(h.held(), []);
    assert_eq!(h.requeues(), [(grant.lease, RequeueReason::Silent)]);
}

/// Catches a daemon that restarts for the switch and is never sent its lease again
/// (it would wait forever, and the lease would be given up at the handover grace), a
/// `Start` sent again on every phase report (twice to one session), and a `Start` sent
/// again before the restart, to the session that already has it. The restarted daemon
/// lists the lease from then on and keeps it.
#[test]
fn a_restarted_daemon_naming_its_switching_lease_is_sent_the_start_again() {
    let mut h = Harness::new();
    let grant = h.preparing();
    let again = h.phase("mac", NodePhase::Preparing(grant.lease));
    assert_eq!(again, [], "sent again to the session that has it");

    let back = h.now + 3 * 60_000;
    h.at(back).register("mac", "after the restart");
    let effects = h.phase("mac", NodePhase::Preparing(grant.lease));
    let [Effect::Start(start)] = effects.as_slice() else {
        panic!("no Start for the restarted daemon: {effects:?}");
    };
    assert_eq!(start.lease, grant.lease);
    assert_eq!(start.operation, grant.operation);
    assert_eq!(start.kind, LeaseKind::WholeMachine);
    assert_eq!(start.resources, MAC);
    assert_eq!(h.phase("mac", NodePhase::Preparing(grant.lease)), []);

    h.heartbeat("mac", &[grant.lease]);
    h.at(back + millis(LEASE_GRACE) - MS)
        .heartbeat("mac", &[grant.lease]);
    h.tick();
    assert_eq!(h.held(), [grant.lease]);
    assert_eq!(h.requeues(), []);
}

/// Catches a report of the same phase that restarts the switch grace: a daemon that
/// keeps coming back still `Preparing` would hold the Mac forever. The grace counts
/// from the first report of the phase.
#[test]
fn reporting_the_same_phase_again_does_not_extend_the_grace() {
    let mut h = Harness::new();
    let grant = h.preparing();
    let since = h.now;
    h.at(since + 5 * 60_000)
        .register("mac", "after the restart");
    h.phase("mac", NodePhase::Preparing(grant.lease));
    h.heartbeat("mac", &[grant.lease]);

    h.at(since + millis(SESSION_SWITCH_GRACE)).tick();
    assert_eq!(h.held(), [], "the grace was extended by the second report");
    assert_eq!(h.requeues(), [(grant.lease, RequeueReason::SessionSwitch)]);
}

/// Catches a `Restoring` phase that is not kept like `Preparing`, or that keeps the
/// grace of the `Preparing` before it: the switch back is a second restart, with a
/// grace of its own.
#[test]
fn restoring_starts_a_grace_of_its_own() {
    let mut h = Harness::new();
    let grant = h.preparing();
    let prepared = h.now;
    let restoring = prepared + 8 * 60_000;
    h.at(restoring)
        .phase("mac", NodePhase::Restoring(grant.lease));

    h.at(prepared + millis(SESSION_SWITCH_GRACE) + 1_000).tick();
    assert_eq!(
        h.held(),
        [grant.lease],
        "Restoring kept the grace of Preparing"
    );
    h.at(restoring + millis(SESSION_SWITCH_GRACE)).tick();
    assert_eq!(h.held(), []);
    assert_eq!(h.requeues(), [(grant.lease, RequeueReason::SessionSwitch)]);
}

/// Catches placement on a worker that is switching or restoring for a lease it no
/// longer holds (given up past the grace, then the daemon came back): a second
/// whole-machine lease would start while the session still belongs to another lease's
/// user. The work waits, naming the worker and its phase, and is placed once the
/// worker serves again.
#[test]
fn no_whole_machine_lease_is_placed_on_a_worker_that_prepares_or_restores() {
    for phase in [
        NodePhase::Preparing(LeaseId::new(9, 1)),
        NodePhase::Restoring(LeaseId::new(9, 1)),
    ] {
        let mut h = Harness::new();
        h.register("mac", "one");
        h.phase("mac", phase.clone());
        h.submit(1, request(1, LeaseKind::WholeMachine));
        let (grants, reasons) = h.tick();
        assert_eq!(grants, [], "placed while {}", phase.name());
        assert_eq!(
            reasons,
            [format!(
                "every live worker that can run it is cordoned or not serving: mac ({})",
                phase.name()
            )]
        );
        h.phase("mac", NodePhase::Serving);
        assert_eq!(h.tick().0.len(), 1, "not placed once serving again");
    }
}

/// Catches a quarantined worker that is offered leases (of either kind), or work that
/// is refused as unservable while only a quarantined worker could run it: like a
/// cordon, a quarantine waits for an operator, so the work waits too.
#[test]
fn a_quarantined_worker_is_offered_nothing_and_its_work_waits() {
    let mut h = Harness::new();
    h.register("mac", "one");
    h.phase(
        "mac",
        NodePhase::Quarantined("user-delete failed".to_owned()),
    );
    h.submit(1, request(1, LeaseKind::WholeMachine));
    h.submit(2, request(2, LeaseKind::Action));
    let (grants, reasons) = h.tick();
    assert_eq!(grants, []);
    let expect = "every live worker that can run it is cordoned or not serving: mac (quarantined)";
    assert_eq!(reasons, [expect, expect]);

    let later = h.now + millis(UNSERVABLE_WAIT) + 1_000;
    h.at(later).heartbeat("mac", &[]);
    assert_eq!(h.tick(), (Vec::new(), Vec::new()), "refused or placed");
    h.phase("mac", NodePhase::Serving);
    let (grants, _) = h.tick();
    let placed: Vec<_> = grants.iter().map(|g| g.operation).collect();
    assert_eq!(placed.len(), 1, "{grants:?}");
}

/// Catches a quarantine that reads as a plain cordon when it is the only connected
/// worker and serves no request kind: the reason names that a worker is not serving.
#[test]
fn every_connected_worker_quarantined_says_so() {
    let mut h = Harness::new();
    h.register("mac", "one");
    h.phase("mac", NodePhase::Quarantined("scan dirty".to_owned()));
    h.submit(1, request(1, LeaseKind::Vm));
    let (_, reasons) = h.tick();
    assert_eq!(
        reasons,
        ["every connected worker is cordoned or not serving"]
    );
}

/// Catches a `Start` sent again to a worker that names a lease another worker holds (a
/// stale daemon, a copied machine), and a phase for a worker that never registered
/// doing anything. Neither changes who holds the lease.
#[test]
fn a_phase_naming_another_worker_s_lease_sends_nothing() {
    let mut h = Harness::new();
    let grant = h.preparing();
    h.register("other", "two");
    h.register("other", "three");
    assert_eq!(h.phase("other", NodePhase::Preparing(grant.lease)), []);
    assert_eq!(h.phase("ghost", NodePhase::Preparing(grant.lease)), []);
    assert_eq!(h.held(), [grant.lease]);
}

/// Catches a phase for a lease whose grant is not committed yet that sends a `Start`
/// before the commit does (the commit-before-Start rule): only the commit sends it.
#[test]
fn a_phase_before_the_commit_sends_no_start() {
    let mut h = Harness::new();
    h.register("mac", "one");
    h.submit(1, request(1, LeaseKind::WholeMachine));
    let (grants, _) = h.tick();
    let [grant] = grants.as_slice() else {
        panic!("{grants:?}");
    };
    h.register("mac", "two");
    assert_eq!(h.phase("mac", NodePhase::Preparing(grant.lease)), []);
    assert_eq!(h.start(grant).lease, grant.lease);
}
