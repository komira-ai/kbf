//! Admission by measured CPU (`Scheduler::with_cpu_admission`) at the scale it is for: one
//! node of 88 threads that keeps 8 for itself (80 CPUs for actions), fed a burst of
//! actions that each start 16 threads and, later, a few single-threaded ones.
//!
//! The world is a deterministic model on logical ticks (one tick is one farm second; no
//! clock is read): an action has a parallelism `p` and an amount of work in CPU-ticks; each
//! tick the node's supply (its CPUs) is shared max-min fairly between the running leases,
//! each taking at most `p` CPUs; a lease whose work is done reports; the node's daemon then
//! reads what its leases used that tick as `busy_millis` and sends it with its heartbeat.
//! The number of runnable threads is the sum of `p` over the running leases. The real
//! scheduler places, and every placement it makes is booked as it is for any farm: one core
//! and 4 GiB per action, nothing more.
//!
//! The randomized fleet of `f4/world.rs` is not extended: its actions have a size and a
//! duration but no parallelism, and the supply they share is not modelled.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{CpuAdmission, DaemonInstance, Event, Request, Scheduler, harness};
use kbf_types::{
    ActionKey, Digest, DigestFunction, Effect, FarmTime, LeaseId, LeaseKind, OperationId, Outcome,
    Qos, Resources, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
/// Threads of the node, and what it keeps for itself.
const THREADS: u64 = 88;
const RESERVED: u64 = 8;
/// The CPUs the actions share.
const CPUS: u64 = THREADS - RESERVED;
/// The most runnable threads the feature may allow: three per thread of the node.
const RUNNABLE_BOUND: u64 = 3 * THREADS;
/// Ticks are farm seconds.
const TICK_MS: u64 = 1_000;
/// Safety stop of a run, in ticks.
const HORIZON: u64 = 5_000;

/// The scenario's two kinds of action.
const WIDE: Kind = Kind {
    parallelism: 16,
    work: 160,
};
const NARROW: Kind = Kind {
    parallelism: 1,
    work: 600,
};

#[derive(Clone, Copy)]
struct Kind {
    /// Threads a running action keeps runnable, and the CPUs it can use at once.
    parallelism: u64,
    /// CPU-ticks of work.
    work: u64,
}

/// What a run measured.
struct Run {
    /// The runnable threads after each tick's placement.
    runnable: Vec<u64>,
    /// The tick each action (by submission order) was started in.
    started: Vec<Option<u64>>,
    /// The tick each action arrived in.
    arrived: Vec<u64>,
    /// The CPU-milliseconds of work done up to and including each tick, in total.
    done: Vec<u64>,
    /// The tick the last action finished in; `None` if the horizon came first.
    finished: Option<u64>,
}

impl Run {
    fn max_runnable(&self) -> u64 {
        self.runnable.iter().copied().max().unwrap_or(0)
    }

    /// The longest an action of the second kind (arriving after tick 0) waited to start.
    fn waited(&self, from: usize) -> Vec<Option<u64>> {
        (from..self.started.len())
            .map(|i| self.started[i].map(|s| s - self.arrived[i]))
            .collect()
    }

    /// Work done by the end of tick `tick`, or by the end of the run if it ended first.
    fn work_by(&self, tick: u64) -> u64 {
        let at = usize::try_from(tick).unwrap_or(usize::MAX);
        self.done[at.min(self.done.len() - 1)]
    }
}

/// How the node's daemon reports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Report {
    /// A reading with every heartbeat, from the first.
    Every,
    /// None at all: a daemon that does not measure.
    Never,
    /// One at tick 0, and none after: a daemon that stopped measuring.
    FirstOnly,
}

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

/// The CPUs each demand gets when `supply` is shared max-min fairly: nobody gets more than
/// they ask for, and what a small demand leaves over is shared by the rest. Integer
/// arithmetic, so a run is exact and replays.
fn fair_shares(supply: u64, demands: &[u64]) -> Vec<u64> {
    let mut order: Vec<usize> = (0..demands.len()).collect();
    order.sort_by_key(|&i| demands[i]);
    let mut shares = vec![0; demands.len()];
    let mut left = supply;
    for (k, &i) in order.iter().enumerate() {
        let fair = left / (demands.len() - k) as u64;
        shares[i] = demands[i].min(fair);
        left -= shares[i];
    }
    shares
}

/// Runs the scenario: 120 wide actions at tick 0 and 8 narrow ones at tick 100, on the one
/// node, with `admission` (or without) and the daemon reporting as `report` says.
fn run(admission: Option<CpuAdmission>, report: Report) -> Run {
    let mut sched = Scheduler::new(1);
    if let Some(admission) = admission {
        sched = sched.with_cpu_admission(admission);
    }
    let node = WorkerId::new("big");
    let mut arrivals: Vec<(u64, Kind)> = Vec::new();
    arrivals.extend((0..120).map(|_| (0, WIDE)));
    arrivals.extend((0..8).map(|_| (100, NARROW)));

    let mut out = Run {
        runnable: Vec::new(),
        started: vec![None; arrivals.len()],
        arrived: arrivals.iter().map(|a| a.0).collect(),
        done: Vec::new(),
        finished: None,
    };
    // Operation ids are given in submission order.
    let op_of = |id: OperationId| usize::try_from(id.0).expect("small");
    // The leases running: the action, and the work left in CPU-milliseconds.
    let mut running: BTreeMap<LeaseId, (usize, u64)> = BTreeMap::new();
    let (mut submitted, mut finished, mut total, mut busy) = (0usize, 0usize, 0u64, 0u64);

    let feed = |sched: &mut Scheduler, tick: u64, event: Event| {
        harness::feed(sched, FarmTime::from_millis(tick * TICK_MS), event)
    };
    feed(
        &mut sched,
        0,
        Event::WorkerUp {
            worker: node.clone(),
            instance: DaemonInstance::new("big"),
            predecessor_stopped: false,
            capacity: Resources::new(CPUS * 1_000, 256 * GIB),
            caps: NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
                .expect("report")
                .with_drivers(["container"]),
        },
    );
    for tick in 0..HORIZON {
        for (n, (at, _)) in arrivals.iter().enumerate().skip(submitted) {
            if *at != tick {
                continue;
            }
            feed(
                &mut sched,
                tick,
                Event::Submit {
                    waiter: WaiterId(n as u64),
                    request: Request {
                        key: ActionKey {
                            instance: "main".to_owned(),
                            action: digest(n as u64),
                        },
                        qos: Qos::Ci,
                        kind: LeaseKind::Action,
                        // The default booking: one core, 4 GiB.
                        resources: Resources::new(1_000, 4 * GIB),
                        hermetic: true,
                        do_not_cache: false,
                        needs: kbf_caps::Request::default(),
                    },
                },
            );
            submitted += 1;
        }
        // The daemon's heartbeat: the reading of the tick that just ended, then the leases.
        let reading = match report {
            Report::Every => Some(busy),
            Report::FirstOnly => (tick == 0).then_some(busy),
            Report::Never => None,
        };
        if let Some(busy_millis) = reading {
            feed(
                &mut sched,
                tick,
                Event::Busy {
                    worker: node.clone(),
                    busy_millis,
                },
            );
        }
        feed(
            &mut sched,
            tick,
            Event::Heartbeat {
                worker: node.clone(),
                running: running.keys().copied().collect(),
            },
        );
        for effect in feed(&mut sched, tick, Event::Tick) {
            if let Effect::Start(start) = effect {
                let action = op_of(start.operation);
                let kind = if arrivals[action].1.parallelism == WIDE.parallelism {
                    WIDE
                } else {
                    NARROW
                };
                running.insert(start.lease, (action, kind.work * 1_000));
                out.started[action] = Some(tick);
                feed(
                    &mut sched,
                    tick,
                    Event::Started {
                        operation: start.operation,
                        lease: start.lease,
                    },
                );
            }
        }
        out.runnable.push(
            running
                .values()
                .map(|(action, _)| arrivals[*action].1.parallelism)
                .sum(),
        );
        // The tick runs: the supply is shared, work is done, finished leases report.
        let leases: Vec<LeaseId> = running.keys().copied().collect();
        let demands: Vec<u64> = leases
            .iter()
            .map(|l| arrivals[running[l].0].1.parallelism * 1_000)
            .collect();
        let shares = fair_shares(CPUS * 1_000, &demands);
        busy = shares.iter().sum();
        total += busy;
        out.done.push(total);
        for (lease, share) in leases.iter().zip(shares) {
            let (action, left) = running[lease];
            if left <= share {
                running.remove(lease);
                finished += 1;
                feed(
                    &mut sched,
                    tick,
                    Event::Report {
                        operation: OperationId(action as u64),
                        lease: *lease,
                        outcome: Outcome::Completed {
                            action_result: digest(1_000_000 + action as u64),
                        },
                    },
                );
            } else {
                running.get_mut(lease).expect("running").1 = left - share;
            }
        }
        if finished == arrivals.len() {
            out.finished = Some(tick);
            break;
        }
    }
    out
}

/// 90 percent of the node's CPUs, 8 leases per reading, readings every tick, and an action
/// is admitted regardless after `max_wait` ticks.
fn admission(max_wait: u64) -> CpuAdmission {
    CpuAdmission::new(90, Duration::from_millis(TICK_MS))
        .with_ramp(8)
        .with_max_wait(Duration::from_millis(max_wait * TICK_MS))
}

/// The aging bound the scenario runs with: longer than the wide actions take to drain.
const MAX_WAIT: u64 = 300;

/// Catches a fair-share model that creates or loses CPU, or lets a small demand take more
/// than it asked: the runs below measure work through it.
#[test]
fn the_nodes_cpus_are_shared_max_min_fairly() {
    assert_eq!(fair_shares(80, &[16, 16, 16]), [16, 16, 16]);
    assert_eq!(fair_shares(80, &[1, 100, 100]), [1, 39, 40]);
    assert_eq!(fair_shares(10, &[1, 1, 1, 1, 1, 100]), [1, 1, 1, 1, 1, 5]);
    assert_eq!(fair_shares(80, &[]), Vec::<u64>::new());
}

/// Asserts that the feature on keeps the runnable threads at most three per thread of the
/// node at every tick, and that the same scenario with it off exceeds that: the burst of 120
/// actions that each start 16 threads is given all 80 cores' worth of leases at once,
/// 1,280 runnable threads. The second half is what makes the first meaningful: a bound the
/// scenario never tests would hold for any scheduler. Catches a clause that is not
/// reached (the reading not carried into placement), a ramp that is not counted, and a
/// threshold compared against the wrong quantity.
#[test]
fn the_feature_bounds_runnable_threads_and_the_scenario_exceeds_it_without() {
    let off = run(None, Report::Every);
    let on = run(Some(admission(MAX_WAIT)), Report::Every);
    assert!(
        off.max_runnable() > RUNNABLE_BOUND,
        "feature off: {} runnable",
        off.max_runnable()
    );
    assert_eq!(off.max_runnable(), 80 * WIDE.parallelism);
    assert!(
        on.max_runnable() <= RUNNABLE_BOUND,
        "feature on: {} runnable at tick {:?}",
        on.max_runnable(),
        on.runnable.iter().position(|r| *r > RUNNABLE_BOUND)
    );
    assert!(on.finished.is_some() && off.finished.is_some());
}

/// Asserts that every narrow action starts within the aging bound of its arrival, with the
/// feature on, and that they are made to wait (the scenario's point: the node is full of
/// wide actions queued ahead of them). Catches an action that starves behind a busy node.
#[test]
fn every_narrow_action_starts_within_the_aging_bound() {
    let on = run(Some(admission(MAX_WAIT)), Report::Every);
    let waits = on.waited(120);
    assert_eq!(waits.len(), 8);
    for (i, wait) in waits.iter().enumerate() {
        let wait = wait.unwrap_or_else(|| panic!("narrow action {i} never started"));
        assert!(wait <= MAX_WAIT, "narrow action {i} waited {wait} ticks");
        assert!(wait > 0, "narrow action {i} was not made to wait");
    }
}

/// Asserts that the feature costs little throughput: by the tick the feature-off run has
/// done all its work, the feature-on run has done at least 90 percent of it. Catches an
/// admission that leaves the node idle (a threshold that closes at any load, a ramp that
/// never refills).
#[test]
fn the_feature_keeps_the_node_busy() {
    let off = run(None, Report::Every);
    let on = run(Some(admission(MAX_WAIT)), Report::Every);
    let end = off.finished.expect("the feature-off run finishes");
    let (done_off, done_on) = (off.work_by(end), on.work_by(end));
    assert!(
        done_on * 10 >= done_off * 9,
        "by tick {end}: {done_on} CPU-ms of {done_off} done"
    );
}

/// Asserts that a node whose daemon sends no reading, or stopped sending them, is admitted
/// to as without the feature: the same leases at the same ticks as the feature-off run when
/// no reading is ever sent; and, when only the first is, the run reaches the same peak as
/// the feature-off run once the reading is three ticks old. Catches an absent or stale
/// reading taken as busy (a node nobody can place on) or as idle (the ramp still applied).
#[test]
fn an_absent_or_stale_reading_fails_open() {
    let off = run(None, Report::Every);
    let never = run(Some(admission(MAX_WAIT)), Report::Never);
    assert_eq!(never.runnable, off.runnable);
    assert_eq!(never.started, off.started);

    let stale = run(Some(admission(MAX_WAIT)), Report::FirstOnly);
    assert_eq!(stale.max_runnable(), off.max_runnable());
    // Held to the ramp while the one reading is current (ticks 0 to 3), then open.
    assert_eq!(stale.runnable[0], 8 * WIDE.parallelism);
    assert_eq!(stale.runnable[3], 8 * WIDE.parallelism);
    assert!(stale.runnable[4] > stale.runnable[3]);
}
