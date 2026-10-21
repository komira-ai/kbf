//! Runs the farm lost (issue #22): a daemon that restarted while its lease ran, proven
//! by its successor's drivers, and a run the node reported lost (fenced, or a
//! whole-machine action whose result the restart took). Each runs again, preferring a
//! node it was not lost on, from one budget shared with busy-node memory kills
//! ([`FARM_RERUNS`]); a run past it finishes the operation. The same budget bounds a
//! run the farm could not do ([`Failure::Infra`]) and a lease given up for silence or
//! replacement.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::fence::{HANDOVER_GRACE, LEASE_GRACE};
use kbf_sched::{
    DaemonInstance, Event, FARM_RERUNS, Input, NodePhase, OpState, Request, RequeueReason,
    Scheduler,
};
use kbf_sim::SimRng;
use kbf_types::{
    ActionKey, Answer, ControlRecord, Digest, DigestFunction, Effect, Failure, FarmFault, FarmRun,
    FarmTime, LeaseKind, LostWhy, OperationId, Outcome, Qos, Resources, ResultRecord, StartLease,
    StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
const FENCED: Outcome = Outcome::Failed(Failure::Lost(LostWhy::Fenced));
const RESTARTED: Outcome = Outcome::Failed(Failure::Lost(LostWhy::DaemonRestarted));
const INFRA: Outcome = Outcome::Failed(Failure::Infra);
const PRESSURE: Outcome = Outcome::Failed(Failure::NodeMemoryPressure);

fn key(n: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n)),
    }
}

/// Action `n`: one core and 1 GiB, of `kind`.
fn request(n: u8, kind: LeaseKind) -> Request {
    Request {
        key: key(n),
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

fn ok() -> Outcome {
    Outcome::Completed {
        action_result: Digest::new(DigestFunction::Sha256, [200; 32], 1),
    }
}

fn lost(worker: &str, why: LostWhy) -> FarmRun {
    FarmRun {
        worker: w(worker),
        fault: FarmFault::Lost(why),
    }
}

fn infra(worker: &str) -> FarmRun {
    FarmRun {
        worker: w(worker),
        fault: FarmFault::Infra,
    }
}

fn busy(worker: &str) -> FarmRun {
    FarmRun {
        worker: w(worker),
        fault: FarmFault::NodeMemoryPressure,
    }
}

/// A scheduler for term 1 that records requeues, with a clock the test moves. Every
/// commit is fed straight back, as the single-node server does.
struct Farm {
    s: Scheduler,
    now: FarmTime,
}

impl Farm {
    fn new() -> Self {
        Self {
            s: Scheduler::new(1).recording_requeues(),
            now: FarmTime::default(),
        }
    }

    fn at(&mut self, d: Duration) {
        let at = FarmTime::from_millis(u64::try_from(d.as_millis()).expect("millis"));
        assert!(at >= self.now, "farm time goes forward");
        self.now = at;
    }

    /// Feeds `event`, then every commit it asks for, and returns the other effects.
    fn feed(&mut self, event: Event) -> Vec<Effect> {
        let mut pending = self.s.apply(Input::new(self.now, event));
        let mut out = Vec::new();
        while !pending.is_empty() {
            let mut more = Vec::new();
            for effect in pending {
                match effect {
                    Effect::Commit(record) => {
                        more.extend(self.s.apply(Input::new(self.now, Event::Committed(record))));
                    }
                    other => out.push(other),
                }
            }
            pending = more;
        }
        out
    }

    /// Registers `name` as daemon process `instance`, saying whether its predecessor
    /// was proven stopped. `whole` nodes also serve whole-machine leases.
    fn up(&mut self, name: &str, instance: &str, predecessor_stopped: bool, whole: bool) {
        let drivers: &[&str] = if whole {
            &["native", "native-whole-machine"]
        } else {
            &["container"]
        };
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .unwrap()
            .with_drivers(drivers.iter().copied());
        let event = Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(instance),
            predecessor_stopped,
            capacity: Resources::new(8_000, 16 * GIB),
            caps,
        };
        assert!(self.feed(event).is_empty());
    }

    /// A node named `name`, its daemon process `name`, serving actions.
    fn node(&mut self, name: &str) {
        self.up(name, name, false, false);
    }

    /// `name`'s heartbeat listing `running`; the effects it led to.
    fn beat(&mut self, name: &str, running: &[&StartLease]) -> Vec<Effect> {
        let running = running.iter().map(|s| s.lease).collect();
        self.feed(Event::Heartbeat {
            worker: w(name),
            running,
        })
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    /// The `Start`s in `effects`.
    fn starts(effects: &[Effect]) -> Vec<StartLease> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Start(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    /// A placement round, which must start exactly one lease.
    fn start(&mut self) -> StartLease {
        let effects = self.feed(Event::Tick);
        let [start] = &Self::starts(&effects)[..] else {
            panic!("want one Start: {effects:?}");
        };
        start.clone()
    }

    /// `start`'s lease ends with `outcome`; the effects, and those of a placement round.
    fn end(&mut self, start: &StartLease, outcome: Outcome) -> Vec<Effect> {
        let event = Event::Report {
            operation: start.operation,
            lease: start.lease,
            outcome,
        };
        let mut effects = self.feed(event);
        effects.extend(self.feed(Event::Tick));
        effects
    }

    /// `start`'s lease ends with `outcome`, and its operation runs again: the next
    /// `Start`, which is for the same operation under a new lease.
    fn rerun(&mut self, start: &StartLease, outcome: Outcome) -> StartLease {
        let effects = self.end(start, outcome);
        assert!(answer_in(&effects).is_none(), "answered: {effects:?}");
        let [next] = &Self::starts(&effects)[..] else {
            panic!("no rerun after {outcome:?}: {effects:?}");
        };
        assert_eq!(next.operation, start.operation);
        assert_ne!(next.lease, start.lease);
        next.clone()
    }

    /// `holder` (running a lease) goes silent while the `others` keep heartbeating, and a
    /// placement round runs after the lease grace: the effects, which give the lease up.
    fn go_silent(&mut self, holder: &str, others: &[&str]) -> Vec<Effect> {
        let at = self
            .now
            .saturating_add(LEASE_GRACE + Duration::from_secs(1));
        self.now = at;
        for name in others.iter().filter(|n| **n != holder) {
            assert!(self.beat(name, &[]).is_empty());
        }
        self.feed(Event::Tick)
    }

    fn requeues(&mut self) -> Vec<RequeueReason> {
        self.s
            .take_requeues()
            .into_iter()
            .map(|r| r.reason)
            .collect()
    }
}

/// The one answer in `effects`, if any.
fn answer_in(effects: &[Effect]) -> Option<&Answer> {
    let answers: Vec<&Answer> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Answer(a) => Some(a),
            _ => None,
        })
        .collect();
    assert!(answers.len() <= 1, "{effects:?}");
    answers.first().copied()
}

/// Two nodes, `a` and `b`, with action 1 started on `a` at 0 s.
fn running_on_a() -> (Farm, StartLease) {
    let mut f = Farm::new();
    f.node("a");
    f.node("b");
    f.submit(1, request(1, LeaseKind::Action));
    let start = f.start();
    assert_eq!(start.worker, w("a"));
    (f, start)
}

/// Catches a proven restart that waits for the handover grace like any other (the
/// fast path missing: buck2 waits 45 s for the farm), a rerun placed on the node it
/// was lost on while another can run it (the "avoid ignored" mutant), and a give-up
/// not charged to the farm budget. Process `a2` registers naming its predecessor
/// stopped; its first heartbeat, which lists nothing, gives the lease up at once.
#[test]
fn a_proven_restart_reruns_at_once_on_another_node() {
    let (mut f, start) = running_on_a();
    f.at(Duration::from_secs(1));
    f.up("a", "a2", true, false);
    let effects = f.beat("a", &[]);
    assert!(answer_in(&effects).is_none(), "{effects:?}");
    assert_eq!(f.requeues(), [RequeueReason::Restarted]);
    let next = f.start();
    assert_eq!(next.operation, start.operation);
    assert_eq!(next.worker, w("b"), "rerun on the node it was lost on");
    assert_eq!(
        f.s.farm_runs(start.operation).unwrap(),
        [lost("a", LostWhy::DaemonRestarted)]
    );
}

/// Catches a predecessor claim honoured when the server did not find it stopped (the
/// claim's other half, the closed stream, is the server's; here it reports
/// `predecessor_stopped = false`): the lease is kept for the handover grace, then
/// given up as replaced, and charged to the farm budget as a fenced run.
#[test]
fn a_restart_without_proof_waits_for_the_handover_grace() {
    let (mut f, start) = running_on_a();
    f.at(Duration::from_secs(1));
    f.up("a", "a2", false, false);
    assert!(f.beat("a", &[]).is_empty());
    assert!(f.requeues().is_empty(), "given up before the grace");
    // `a` was last heard at 0 s, before the new process registered.
    f.at(HANDOVER_GRACE);
    assert!(f.beat("a", &[]).is_empty());
    assert_eq!(f.requeues(), [RequeueReason::Replaced]);
    assert_eq!(
        f.s.farm_runs(start.operation).unwrap(),
        [lost("a", LostWhy::Fenced)],
        "a replacement is a farm run"
    );
}

/// Catches a proof that clears every earlier process at once (the mutant "handover
/// ends now"): process 2 proves process 1 stopped, not process 0, which may still run
/// on another machine (a cloned disk). Only the lease of process 1 goes at once.
#[test]
fn a_proof_covers_the_named_predecessor_only() {
    let (mut f, first) = running_on_a();
    // Process 1, unproven, takes a second lease; `b` is cordoned so it lands on `a`.
    f.at(Duration::from_secs(1));
    f.up("a", "a1", false, false);
    f.feed(Event::Cordon { worker: w("b") });
    f.submit(2, request(2, LeaseKind::Action));
    let second = f.start();
    assert_eq!(second.worker, w("a"));
    f.feed(Event::Uncordon { worker: w("b") });
    f.at(Duration::from_secs(2));
    f.up("a", "a2", true, false);
    assert!(f.beat("a", &[]).is_empty());
    assert_eq!(f.requeues(), [RequeueReason::Restarted]);
    assert_eq!(f.s.state(second.operation), Some(&OpState::Queued));
    assert!(
        matches!(
            f.s.state(first.operation),
            Some(OpState::Leased { lease, .. }) if *lease == first.lease
        ),
        "process 0's lease given up on process 2's word"
    );
    f.at(Duration::from_secs(1) + HANDOVER_GRACE);
    f.beat("a", &[]);
    assert_eq!(f.requeues(), [RequeueReason::Replaced]);
}

/// Catches the requeue done when the restarted daemon registers (the mutant "requeue
/// at `WorkerUp`"): a whole-machine lease whose daemon restarted mid-action is still
/// held, its phase `Restoring`, and its `Start` goes to the new process, which runs
/// the restore. Giving it up before the phase is taken would place it again while the
/// node is still being cleaned.
#[test]
fn a_restoring_whole_machine_lease_survives_a_proven_restart() {
    let mut f = Farm::new();
    f.up("mac", "one", false, true);
    f.submit(1, request(1, LeaseKind::WholeMachine));
    let start = f.start();
    f.feed(Event::Started {
        operation: start.operation,
        lease: start.lease,
    });
    f.at(Duration::from_secs(1));
    f.up("mac", "two", true, true);
    let effects = f.feed(Event::Phase {
        worker: w("mac"),
        phase: NodePhase::Restoring(start.lease),
    });
    let [again] = &Farm::starts(&effects)[..] else {
        panic!("the Start not sent again: {effects:?}");
    };
    assert_eq!(again.lease, start.lease);
    assert!(f.beat("mac", &[]).is_empty());
    assert!(f.requeues().is_empty(), "the restoring lease was given up");
    assert!(matches!(
        f.s.state(start.operation),
        Some(OpState::Running { lease, .. }) if *lease == start.lease
    ));
}

/// Catches lost results not charged to the budget (the mutant: they would rerun for
/// ever), an off-by-one in it (`<` for `<=`: answered after 2 runs, or after 4), and an
/// answer that does not carry the runs. Three lost runs end the operation with the
/// third's failure; two lost runs and then an OK one end it completed, the answer
/// naming the two.
#[test]
fn three_lost_runs_use_up_the_budget_and_two_do_not() {
    let mut f = Farm::new();
    for name in ["a", "b", "c"] {
        f.node(name);
    }
    f.submit(1, request(1, LeaseKind::Action));
    let mut start = f.start();
    let mut nodes = Vec::new();
    for _ in 0..FARM_RERUNS {
        nodes.push(start.worker.clone());
        start = f.rerun(&start, FENCED);
    }
    nodes.push(start.worker.clone());
    assert_eq!(nodes, [w("a"), w("b"), w("c")], "each rerun on a new node");
    let effects = f.end(&start, FENCED);
    let answer = answer_in(&effects).expect("answered once the budget was used");
    assert_eq!(answer.outcome, FENCED);
    let fenced = |n: &str| lost(n, LostWhy::Fenced);
    assert_eq!(answer.farm_runs, [fenced("a"), fenced("b"), fenced("c")]);
    assert!(Farm::starts(&effects).is_empty(), "{effects:?}");
    assert_eq!(f.requeues(), [RequeueReason::Lost(LostWhy::Fenced); 2]);

    f.submit(2, request(2, LeaseKind::Action));
    let mut start = f.start();
    for _ in 0..FARM_RERUNS {
        start = f.rerun(&start, RESTARTED);
    }
    let effects = f.end(&start, ok());
    let answer = answer_in(&effects).expect("answered");
    assert_eq!(answer.outcome, ok());
    let restarted = |n: &str| lost(n, LostWhy::DaemonRestarted);
    assert_eq!(answer.farm_runs, [restarted("a"), restarted("b")]);
}

/// Catches a second counter for lost runs beside the busy-node one (the mutant: a
/// busy node's kill and two lost runs would run 4 times): one budget, shared.
#[test]
fn busy_node_kills_and_lost_runs_share_one_budget() {
    let (mut f, start) = running_on_a();
    let start = f.rerun(&start, PRESSURE);
    let start = f.rerun(&start, FENCED);
    let effects = f.end(&start, RESTARTED);
    let answer = answer_in(&effects).expect("answered after 3 runs");
    assert_eq!(answer.outcome, RESTARTED);
    assert_eq!(
        answer.farm_runs,
        [
            busy("a"),
            lost("b", LostWhy::Fenced),
            lost("a", LostWhy::DaemonRestarted)
        ]
    );
    assert_eq!(
        answer.memory_runs.len(),
        1,
        "the busy node's kill is a memory run too"
    );
}

/// Catches `avoid` as a hard exclusion (the mutant: a one-node farm would never run
/// the operation again) and `avoid` ignored (the rerun on the node it was lost on
/// while another can run it).
#[test]
fn a_rerun_prefers_another_node_and_falls_back_to_the_same() {
    let mut f = Farm::new();
    f.node("a");
    f.submit(1, request(1, LeaseKind::Action));
    let start = f.start();
    let next = f.rerun(&start, FENCED);
    assert_eq!(next.worker, w("a"), "the only node");

    f.node("b");
    let next = f.rerun(&next, FENCED);
    assert_eq!(next.worker, w("b"), "another node can run it");
}

/// Catches a node that cannot take the rerun now taken as reason to run it where it
/// was lost, and a cordoned one taken as one that can run it: the rerun waits for a
/// node outside the avoided set while one is live, serving and large enough (here `a`,
/// full), and goes back to the node it was lost on when the only other is cordoned.
#[test]
fn a_rerun_waits_only_for_a_node_that_could_run_it() {
    let mut f = Farm::new();
    f.node("a");
    f.node("b");
    let full = Request {
        resources: Resources::new(8_000, GIB),
        ..request(9, LeaseKind::Action)
    };
    f.submit(9, full);
    let full = f.start();
    assert_eq!(full.worker, w("a"));
    f.submit(1, request(1, LeaseKind::Action));
    let start = f.start();
    assert_eq!(start.worker, w("b"));
    let effects = f.end(&start, FENCED);
    assert!(
        Farm::starts(&effects).is_empty(),
        "placed on the node it was lost on: {effects:?}"
    );
    // `a` empties: the rerun goes there.
    let effects = f.end(&full, ok());
    let [next] = &Farm::starts(&effects)[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(next.worker, w("a"));

    let (mut g, start) = running_on_a();
    g.feed(Event::Cordon { worker: w("b") });
    let next = g.rerun(&start, FENCED);
    assert_eq!(next.worker, w("a"), "a cordoned node held the rerun back");
}

/// Catches a whole-machine rerun that holds the node it was lost on while another
/// could run it (the reservation not told of the avoided nodes): it fits nowhere now,
/// `a` is empty and `b` busy, and it holds `b`, as placement waits for `b`.
#[test]
fn a_whole_machine_rerun_holds_a_node_it_was_not_lost_on() {
    let mut f = Farm::new();
    f.up("a", "a", false, true);
    f.up("b", "b", false, true);
    f.submit(1, request(1, LeaseKind::WholeMachine));
    let start = f.start();
    assert_eq!(start.worker, w("a"));
    f.submit(2, request(2, LeaseKind::Action));
    let busy = f.start();
    assert_eq!(busy.worker, w("b"));
    let effects = f.end(&start, FENCED);
    assert!(Farm::starts(&effects).is_empty(), "{effects:?}");
    assert_eq!(f.s.reservation(start.operation), Some(&w("b")));
    let effects = f.end(&busy, ok());
    let [next] = &Farm::starts(&effects)[..] else {
        panic!("{effects:?}");
    };
    assert_eq!((next.operation, &next.worker), (start.operation, &w("b")));
}

/// Catches the give-up path left out of the budget, and a budget used up there that
/// answers nobody (it must propose the result, so the log ends the operation): the
/// third run is lost through a proven restart, and the heartbeat that gives it up
/// answers the operation as lost.
#[test]
fn a_restart_past_the_budget_answers_the_operation() {
    let (mut f, start) = running_on_a();
    let start = f.rerun(&start, FENCED);
    let start = f.rerun(&start, FENCED);
    assert_eq!(start.worker, w("a"), "both avoided: the first that fits");
    f.at(Duration::from_secs(1));
    f.up("a", "a2", true, false);
    let effects = f.beat("a", &[]);
    let answer = answer_in(&effects).expect("the give-up answered nobody");
    assert_eq!(answer.outcome, RESTARTED);
    assert_eq!(answer.lease, start.lease);
    assert_eq!(answer.farm_runs.len(), 3);
    assert_eq!(answer.farm_runs[2], lost("a", LostWhy::DaemonRestarted));
    assert!(Farm::starts(&effects).is_empty(), "{effects:?}");
    assert_eq!(f.requeues(), [RequeueReason::Lost(LostWhy::Fenced); 2]);
    assert!(f.s.state(start.operation).is_some_and(OpState::is_done));
}

/// Catches a result proposed twice for one give-up: the committed record of the
/// give-up past the budget is the operation's result, and its report afterwards
/// (the restarted daemon has none, but a copy of the record) changes nothing.
#[test]
fn the_record_of_a_give_up_is_committed_once() {
    let (mut f, start) = running_on_a();
    let start = f.rerun(&start, FENCED);
    let start = f.rerun(&start, FENCED);
    f.at(Duration::from_secs(1));
    f.up("a", "a2", true, false);
    let effects = f.s.apply(Input::new(
        f.now,
        Event::Heartbeat {
            worker: w("a"),
            running: Vec::new(),
        },
    ));
    let record = ControlRecord::Result(ResultRecord {
        lease: start.lease,
        operation: start.operation,
        outcome: RESTARTED,
    });
    assert_eq!(effects, [Effect::Commit(record.clone())]);
    assert!(f.beat("a", &[]).is_empty(), "proposed again");
    let effects = f.feed(Event::Committed(record.clone()));
    assert!(answer_in(&effects).is_some(), "{effects:?}");
    assert!(
        f.feed(Event::Committed(record)).is_empty(),
        "committed twice"
    );
}

/// One step of a generated sequence: a submission of action `n`, or the end of the
/// `pick`th running lease (modulo how many run) with an outcome.
#[derive(Clone, Debug)]
enum Step {
    Submit(u8),
    End(u64, Outcome),
}

/// Catches farm runs that are not a function of the scheduler's inputs: two
/// schedulers fed the same generated sequence of lost runs, busy-node kills and
/// completions over three nodes hold equal farm runs for every operation after every
/// step, and the sweep reaches operations with more than one.
#[test]
fn schedulers_fed_one_sequence_hold_equal_farm_runs() {
    let mut many = false;
    for seed in 0..16 {
        let mut rng = SimRng::from_seed(seed);
        let steps: Vec<Step> = (0..80)
            .map(|_| match rng.below(5) {
                0 | 1 => Step::Submit(u8::try_from(rng.below(6)).unwrap_or_default()),
                2 => Step::End(rng.below(64), FENCED),
                3 => Step::End(rng.below(64), PRESSURE),
                _ => Step::End(rng.below(64), ok()),
            })
            .collect();
        let mut sims = [Sim::new(), Sim::new()];
        for step in &steps {
            for sim in &mut sims {
                sim.apply(step);
            }
            let [a, b] = &sims;
            assert_eq!(a.farm_runs(), b.farm_runs(), "seed {seed}");
            many |= a.farm_runs().values().any(|runs| runs.len() > 1);
        }
    }
    assert!(many, "no seed lost one operation twice");
}

/// A farm of three nodes fed generated steps.
struct Sim {
    f: Farm,
    running: Vec<StartLease>,
    next: u64,
}

impl Sim {
    fn new() -> Self {
        let mut f = Farm::new();
        for name in ["a", "b", "c"] {
            f.node(name);
        }
        Self {
            f,
            running: Vec::new(),
            next: 0,
        }
    }

    fn apply(&mut self, step: &Step) {
        let effects = match step {
            Step::Submit(n) => {
                self.next += 1;
                self.f.submit(self.next, request(*n, LeaseKind::Action));
                self.f.feed(Event::Tick)
            }
            Step::End(pick, outcome) => {
                let running = u64::try_from(self.running.len()).unwrap_or(u64::MAX);
                if running == 0 {
                    return;
                }
                let at = usize::try_from(pick % running).unwrap_or_default();
                let start = self.running.remove(at);
                self.f.end(&start, *outcome)
            }
        };
        self.running.extend(Farm::starts(&effects));
    }

    /// The farm runs of every operation the scheduler holds.
    fn farm_runs(&self) -> BTreeMap<OperationId, Vec<FarmRun>> {
        (0..self.next + 1)
            .map(OperationId)
            .filter_map(|id| Some((id, self.f.s.farm_runs(id)?.to_vec())))
            .collect()
    }
}

/// Catches a run the farm could not do finishing its operation at once (the mutant:
/// `Failure::Infra` answered on the first run), a budget off by one (`<` for `<=`:
/// answered after 2 runs, or after 4), a rerun placed on the node it failed on while
/// others fit, and an answer that does not carry the runs. Three infra results end the
/// operation with the third's failure, the answer naming all three nodes; two and then
/// an OK one complete it.
#[test]
fn three_infra_results_use_up_the_budget_and_two_do_not() {
    let mut f = Farm::new();
    for name in ["a", "b", "c"] {
        f.node(name);
    }
    f.submit(1, request(1, LeaseKind::Action));
    let mut start = f.start();
    let mut nodes = Vec::new();
    for _ in 0..FARM_RERUNS {
        nodes.push(start.worker.clone());
        start = f.rerun(&start, INFRA);
    }
    nodes.push(start.worker.clone());
    assert_eq!(nodes, [w("a"), w("b"), w("c")], "each rerun on a new node");
    let effects = f.end(&start, INFRA);
    let answer = answer_in(&effects).expect("answered once the budget was used");
    assert_eq!(answer.outcome, INFRA);
    assert_eq!(answer.farm_runs, [infra("a"), infra("b"), infra("c")]);
    assert!(Farm::starts(&effects).is_empty(), "{effects:?}");
    assert_eq!(f.requeues(), [RequeueReason::Infra; 2]);

    f.submit(2, request(2, LeaseKind::Action));
    let mut start = f.start();
    for _ in 0..FARM_RERUNS {
        start = f.rerun(&start, INFRA);
    }
    let effects = f.end(&start, ok());
    let answer = answer_in(&effects).expect("answered");
    assert_eq!(answer.outcome, ok());
    assert_eq!(answer.farm_runs, [infra("a"), infra("b")]);
}

/// Catches a node that goes silent with the lease every time holding an operation for
/// ever (the mutant: silence requeues without charge), and an answer that does not
/// carry the runs. Three silent nodes in turn end the operation, answered as lost to a
/// fence (the node's own fence ended the run within the grace), naming all three.
#[test]
fn silent_nodes_use_up_the_budget_and_the_operation_is_answered() {
    let mut f = Farm::new();
    let nodes = ["a", "b", "c"];
    for name in nodes {
        f.node(name);
    }
    f.submit(1, request(1, LeaseKind::Action));
    let mut start = f.start();
    for _ in 0..FARM_RERUNS {
        let effects = f.go_silent(start.worker.as_str(), &nodes);
        assert!(answer_in(&effects).is_none(), "{effects:?}");
        let [next] = &Farm::starts(&effects)[..] else {
            panic!("no rerun after silence: {effects:?}");
        };
        assert_ne!(next.worker, start.worker, "rerun on the silent node");
        start = next.clone();
    }
    let effects = f.go_silent(start.worker.as_str(), &nodes);
    let answer = answer_in(&effects).expect("a silent node held the operation for ever");
    assert_eq!(answer.outcome, FENCED);
    assert_eq!(answer.lease, start.lease);
    let fenced = |n: &str| lost(n, LostWhy::Fenced);
    assert_eq!(answer.farm_runs, [fenced("a"), fenced("b"), fenced("c")]);
    assert!(Farm::starts(&effects).is_empty(), "{effects:?}");
    assert_eq!(f.requeues(), [RequeueReason::Silent; 2]);
    assert!(f.s.state(start.operation).is_some_and(OpState::is_done));
}

/// Catches a replacement without proof outside the budget (the mutant: `Replaced`
/// requeues without charge): a daemon process that keeps being replaced would hold the
/// operation for ever. The third run is given up as replaced and answers the operation
/// as a fenced run.
#[test]
fn a_replacement_past_the_budget_answers_the_operation() {
    let (mut f, start) = running_on_a();
    let start = f.rerun(&start, FENCED);
    let start = f.rerun(&start, INFRA);
    assert_eq!(start.worker, w("a"), "both avoided: the first that fits");
    f.at(Duration::from_secs(1));
    f.up("a", "a2", false, false);
    f.at(Duration::from_secs(1) + HANDOVER_GRACE);
    let effects = f.beat("a", &[]);
    let answer = answer_in(&effects).expect("the replacement answered nobody");
    assert_eq!(answer.outcome, FENCED);
    assert_eq!(answer.lease, start.lease);
    assert_eq!(
        answer.farm_runs,
        [
            lost("a", LostWhy::Fenced),
            infra("b"),
            lost("a", LostWhy::Fenced)
        ]
    );
    assert!(Farm::starts(&effects).is_empty(), "{effects:?}");
}

/// Catches one counter per fault kind (the mutant: silence and infra results counted
/// apart from lost runs would allow 6 runs): a lost run, an infra result and a silence
/// are one budget, so the third ends the operation, and the answer lists them in order.
#[test]
fn lost_infra_and_silence_share_one_budget() {
    let mut f = Farm::new();
    let nodes = ["a", "b", "c"];
    for name in nodes {
        f.node(name);
    }
    f.submit(1, request(1, LeaseKind::Action));
    let start = f.start();
    let start = f.rerun(&start, FENCED);
    let start = f.rerun(&start, INFRA);
    let effects = f.go_silent(start.worker.as_str(), &nodes);
    let answer = answer_in(&effects).expect("answered after 3 mixed runs");
    assert_eq!(answer.outcome, FENCED);
    assert_eq!(
        answer.farm_runs,
        [
            lost("a", LostWhy::Fenced),
            infra("b"),
            lost("c", LostWhy::Fenced)
        ]
    );
}

/// Catches a give-up past the budget proposed twice: silence is checked on every
/// placement round, and the round after the proposal, before its commit, must not
/// propose the result again. Committing it answers once.
#[test]
fn the_record_of_a_silence_is_committed_once() {
    let (mut f, start) = running_on_a();
    let start = f.rerun(&start, FENCED);
    let start = f.rerun(&start, INFRA);
    f.now = f.now.saturating_add(LEASE_GRACE + Duration::from_secs(1));
    f.beat("b", &[]);
    let tick = |f: &mut Farm| f.s.apply(Input::new(f.now, Event::Tick));
    let record = ControlRecord::Result(ResultRecord {
        lease: start.lease,
        operation: start.operation,
        outcome: FENCED,
    });
    assert_eq!(tick(&mut f), [Effect::Commit(record.clone())]);
    assert!(tick(&mut f).is_empty(), "proposed again");
    let effects = f.feed(Event::Committed(record));
    assert!(answer_in(&effects).is_some(), "{effects:?}");
}

/// Whether `failure` is the user's own (or the client's), which the farm never reruns.
/// Exhaustive, so a new [`Failure`] must be placed on one side or the other here.
const fn is_the_users(failure: Failure) -> bool {
    match failure {
        Failure::Timeout | Failure::Invalid | Failure::MissingInput => true,
        // Rerun from the farm budget, or (an action's own memory limit) from the
        // memory floor: `an_out_of_memory_kill_is_not_a_farm_fault`.
        Failure::Infra | Failure::NodeMemoryPressure | Failure::Lost(_) | Failure::OutOfMemory => {
            false
        }
    }
}

/// Catches the budget widened to failures that are not the farm's (the mutant: every
/// `Failure` charged, or the `Infra` check widened to a user's failure): the user's
/// action failing (timeout), an action that cannot run as written, and a missing input
/// are answered by their first run, with no farm run and no requeue. Every [`Failure`]
/// is classified by [`is_the_users`], so a new variant cannot skip this test.
#[test]
fn a_users_failure_is_never_retried() {
    let every = [
        Failure::Timeout,
        Failure::Invalid,
        Failure::MissingInput,
        Failure::Infra,
        Failure::OutOfMemory,
        Failure::NodeMemoryPressure,
        Failure::Lost(LostWhy::Fenced),
        Failure::Lost(LostWhy::DaemonRestarted),
    ];
    let users: Vec<Failure> = every.into_iter().filter(|f| is_the_users(*f)).collect();
    assert_eq!(users.len(), 3, "{users:?}");
    for (n, failure) in users.into_iter().enumerate() {
        let (mut f, start) = running_on_a();
        let outcome = Outcome::Failed(failure);
        let effects = f.end(&start, outcome);
        let answer = answer_in(&effects).unwrap_or_else(|| panic!("{failure:?} was retried"));
        assert_eq!(answer.outcome, outcome, "case {n}");
        assert!(answer.farm_runs.is_empty(), "{failure:?} charged");
        assert!(f.requeues().is_empty(), "{failure:?} requeued");
    }
}

#[path = "lost/give_up.rs"]
mod give_up;
