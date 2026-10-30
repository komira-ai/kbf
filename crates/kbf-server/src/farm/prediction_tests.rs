//! `--placement-predictions`, through the farm: what the scheduler is charged for an operation
//! (read back from `lease_cost` once the operation is placed) in each mode, the counts, the
//! comparison with the run in shadow, the restart, and the node whose cgroup has a CPU quota.
//! Time is the farm's: the estimator's clock is set by the test, and nothing sleeps.

use std::sync::Arc;

use clap::Parser;
use kbf_estimator::{Basis, Bounds, GIB, Hint, MIB, Outcome as RunOutcome, Run};
use kbf_front::{Cache, Described, Dispatch, Submission};
use kbf_invocation::Evidence;
use kbf_proto::worker::{NodeLoad, ServerMessage};
use kbf_sched::DaemonInstance;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{
    ActionKey, Digest, DigestFunction, FarmTime, LeaseId, LeaseKind, OperationId, Qos, Resources,
    WorkerId,
};

use super::phase::Reported;
use super::{EpochPolicy, Farm};
use crate::action_usage::Subject;
use crate::learned::{Attempt, Axis, Fit, LearnedSizes, Unset, key_of};
use crate::machine::Platform;
use crate::state_store::StateStore;
use crate::{Args, PlacementLoad, PlacementPredictions};

const DAY: u64 = 86_400_000;
const MINUTE: u64 = 60_000_000;
const ALL: [Unset; 8] = [
    Unset::Missing,
    Unset::Stale,
    Unset::Cold,
    Unset::Corrupt,
    Unset::OverCap,
    Unset::Kind,
    Unset::NoOs,
    Unset::NoSubject,
];

/// A farm that learns in shadow, places by load, and predicts as `predictions` says.
fn farm(predictions: PlacementPredictions) -> Farm {
    Farm::new_legacy(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_wall_epoch(20_000 * DAY)
    .with_learned_sizes(LearnedSizes::Shadow, 4 * GIB)
    .with_placement_load(PlacementLoad::On)
    .with_placement_predictions(predictions)
}

/// A Linux node `name` of `cores` cores and `gib` GiB, with its stream.
fn node(farm: &Farm, name: &str, cores: u64, gib: u64) -> (WorkerId, super::StreamId) {
    let worker = WorkerId::new(name);
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = tokio::sync::mpsc::unbounded_channel();
    // The receiver lives as long as the test: a dropped one closes the stream.
    std::mem::forget(inbox);
    let stream = farm.register(
        &worker,
        DaemonInstance::new(name),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(cores * 1_000, gib << 30), caps),
        outbound,
        ServerMessage::default(),
    );
    (worker, stream)
}

/// What a submission is like. The defaults are the predicted case: an action of unnamed size on
/// Linux, instance `main`, program `cc`, asking for a core and 4 GiB.
#[derive(Clone)]
struct Job {
    tag: u8,
    platform: Vec<(&'static str, &'static str)>,
    bare: bool,
    gpus: u64,
    cpu_millis: u64,
    memory: u64,
}

impl Job {
    fn new(tag: u8) -> Self {
        Self {
            tag,
            platform: vec![("OSFamily", "linux")],
            bare: false,
            gpus: 0,
            cpu_millis: 1_000,
            memory: 4 * GIB,
        }
    }

    fn submit(&self, farm: &Farm) {
        let request = kbf_sched::Request {
            key: ActionKey {
                instance: "main".to_owned(),
                action: Digest::new(DigestFunction::Sha256, [self.tag; 32], 1),
            }
            .into(),
            qos: Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(self.cpu_millis, self.memory).with_gpus(self.gpus),
            hermetic: true,
            do_not_cache: false,
            needs: kbf_caps::Request::from_platform(self.platform.iter().copied()).unwrap(),
            whole_worker: false,
        };
        let arguments = if self.bare {
            Vec::new()
        } else {
            vec!["cc".to_owned()]
        };
        farm.submit(Submission {
            request,
            call: None,
            described: Described {
                arguments,
                ..Described::default()
            },
            evidence: Evidence::default(),
            max_wall: None,
        })
        .expect("submitted");
    }
}

/// The estimator's key of `Job::new(_)`.
fn key() -> kbf_estimator::Key {
    let (subject, _) = Subject::of(Described {
        arguments: vec!["cc".to_owned()],
        ..Described::default()
    });
    let platform = Platform {
        os_family: "linux",
        isa: "-".to_owned(),
    };
    key_of("main", &platform, Basis::Cgroup, &subject).expect("a key")
}

fn run(wall_us: u64, peak: u64, cpu_us: u64) -> Run {
    Run {
        outcome: RunOutcome::Success,
        wall_us,
        peak_memory_bytes: Some(peak),
        booked_memory_bytes: 4 * GIB,
        raised_memory_bytes: None,
        limit_hits: 0,
        swap_peak_bytes: 0,
        cpu_usage_us: cpu_us,
        node_cpu_pressure_permille: Some(0),
    }
}

/// Shows the estimator `runs` runs of the key, as the farm's reports would.
fn teach(farm: &Farm, runs: u64, run: &Run) {
    let bounds = Bounds {
        default_memory: 4 * GIB,
        host_memory: 64 * GIB,
        node_cpu_millis: 8_000,
    };
    let mut state = farm.lock();
    for _ in 0..runs {
        let attempt = Attempt {
            key: key(),
            run: *run,
        };
        state.side.learned.observe(&attempt, &bounds);
    }
}

/// The runs the keys are taught: five minutes, 500 MiB, one and a half cores. The estimate
/// for them is 1,500 millicores and a life of six minutes.
fn five_minutes() -> Run {
    run(5 * MINUTE, 500 * MIB, 450_000_000)
}

/// What the scheduler charges a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Charged {
    cpu_millis: u64,
    memory_bytes: u64,
    limit_bytes: u64,
    cold: bool,
    retry: bool,
}

/// What the scheduler charges `lease`, which it holds.
fn charged(farm: &Farm, lease: LeaseId) -> Charged {
    let cost = farm.lock().sched.lease_cost(lease).expect("a charge");
    Charged {
        cpu_millis: cost.cpu_millis,
        memory_bytes: cost.memory_bytes,
        limit_bytes: cost.limit_bytes,
        cold: cost.cold,
        retry: cost.retry,
    }
}

/// What the scheduler charges the one lease `worker` holds.
fn cost(farm: &Farm, worker: &WorkerId) -> Charged {
    let leases = farm.lock().sched.leases_on(worker);
    assert_eq!(leases.len(), 1, "one lease placed");
    charged(farm, leases[0])
}

/// Asserts `charged` is what the scheduler charges an action it was given no prediction for:
/// its request's cores and no memory.
fn booked(charged: &Charged) {
    assert_eq!(charged.cpu_millis, 1_000);
    assert_eq!(charged.memory_bytes, 0);
    assert!(!charged.cold);
    assert_eq!(charged.limit_bytes, 4 * GIB);
}

fn unset_counts(farm: &Farm) -> Vec<u64> {
    let state = farm.lock();
    ALL.iter()
        .map(|why| state.side.learned.predictions_unset(*why))
        .collect()
}

fn made(farm: &Farm) -> u64 {
    farm.lock().side.learned.predictions_made()
}

/// Catches a prediction that is not the estimator's answer (a swapped cores and memory, the
/// request's own figures), a cold one, and the scheduler not charged it: the lease the
/// operation gets is charged the estimate's cores and memory, and the cores are those the
/// runs used and not the core the request asked for.
#[test]
fn a_warm_estimate_gives_a_non_cold_prediction_with_the_estimators_figures() {
    let farm = farm(PlacementPredictions::On);
    let (worker, _) = node(&farm, "node-a", 4, 64);
    teach(&farm, 6, &five_minutes());
    let expected = {
        let state = farm.lock();
        let bounds = Bounds {
            default_memory: 4 * GIB,
            host_memory: 64 * GIB,
            node_cpu_millis: 4_000,
        };
        state
            .side
            .learned
            .summaries()
            .estimate(&key(), state.side.learned.day(), &bounds, &Hint::default())
            .expect("a warm key")
    };
    Job::new(1).submit(&farm);
    let charged = cost(&farm, &worker);
    assert_eq!(charged.cpu_millis, 1_500);
    assert_eq!(u64::from(expected.cpu_millis), 1_500);
    assert_eq!(charged.memory_bytes, expected.memory_bytes);
    assert!(charged.memory_bytes > 500 * MIB && charged.memory_bytes < 4 * GIB);
    assert!(!charged.cold && !charged.retry);
    assert_eq!(
        charged.limit_bytes,
        4 * GIB,
        "the lease still runs under its rung"
    );
    assert_eq!(made(&farm), 1);
    assert_eq!(unset_counts(&farm), [0; 8]);
    assert_eq!(farm.lock().sched.load_counters().admitted_cold, 0);
}

/// Catches cores charged above what the node registered: runs that used a core and a half on
/// a node that booked one core are charged the node's one core (and the estimate's memory).
#[test]
fn the_charged_cores_never_exceed_the_registered_node() {
    let farm = farm(PlacementPredictions::On);
    let (worker, _) = node(&farm, "node-a", 1, 64);
    teach(&farm, 6, &five_minutes());
    Job::new(1).submit(&farm);
    let charged = cost(&farm, &worker);
    assert_eq!(charged.cpu_millis, 1_000);
    assert!(charged.memory_bytes > 500 * MIB && charged.memory_bytes < 4 * GIB);
    assert_eq!(made(&farm), 1);
}

/// What was charged and counted for `job` on a farm that has `prepare`d the estimator, in
/// `on`: no prediction was set (the lease is charged as `off` charges it), and the one
/// reason `why` was counted once.
fn unset_for(job: &Job, why: Unset, prepare: impl FnOnce(&Farm)) {
    let farm = farm(PlacementPredictions::On);
    let (worker, _) = node(&farm, "node-a", 4, 16);
    prepare(&farm);
    job.submit(&farm);
    booked(&cost(&farm, &worker));
    assert_eq!(farm.lock().sched.load_counters().admitted_cold, 0);
    assert_eq!(made(&farm), 0);
    let want: Vec<u64> = ALL.iter().map(|w| u64::from(*w == why)).collect();
    assert_eq!(unset_counts(&farm), want, "{why:?}");
}

/// Catches an action whose key the estimator has never seen charged a prediction.
#[test]
fn a_key_with_no_row_gets_no_prediction() {
    unset_for(&Job::new(1), Unset::Missing, |_| {});
}

/// Catches an action whose key has one sample charged a prediction (the estimator is cold).
#[test]
fn a_key_with_one_sample_gets_no_prediction() {
    unset_for(&Job::new(1), Unset::Cold, |farm| {
        teach(farm, 1, &five_minutes());
    });
}

/// Catches a prediction from a row last seen more than sixty days ago.
#[test]
fn a_key_last_seen_sixty_one_days_ago_gets_no_prediction() {
    unset_for(&Job::new(1), Unset::Stale, |farm| {
        teach(farm, 6, &five_minutes());
        farm.lock()
            .side
            .learned
            .clock(FarmTime::from_millis(61 * DAY));
    });
}

/// Catches a prediction that would take the whole host: runs that peaked at 20 GiB give an
/// estimate above the 16 GiB node, which the estimator refuses.
#[test]
fn an_estimate_the_size_of_the_node_gets_no_prediction() {
    unset_for(&Job::new(1), Unset::OverCap, |farm| {
        // Booked at 32 GiB, so that a peak of 20 GiB is not a run cut off at its limit.
        let big = Run {
            booked_memory_bytes: 32 * GIB,
            ..run(5 * MINUTE, 20 * GIB, 450_000_000)
        };
        teach(farm, 6, &big);
    });
}

/// Catches a prediction for an action whose OS the request does not pin.
#[test]
fn an_action_that_pins_no_os_gets_no_prediction() {
    let job = Job {
        platform: Vec::new(),
        ..Job::new(1)
    };
    unset_for(&job, Unset::NoOs, |farm| teach(farm, 6, &five_minutes()));
}

/// Catches a prediction for an action that describes nothing (every such action would share
/// one key).
#[test]
fn an_action_with_no_subject_gets_no_prediction() {
    let job = Job {
        bare: true,
        ..Job::new(1)
    };
    unset_for(&job, Unset::NoSubject, |farm| {
        teach(farm, 6, &five_minutes())
    });
}

/// Catches a GPU action given a prediction: it is not asked about and is counted by its reason.
/// No node here has a GPU, so the action stays queued, and nothing is placed to read.
#[test]
fn a_gpu_action_gets_no_prediction() {
    let farm = farm(PlacementPredictions::On);
    let (worker, _) = node(&farm, "node-a", 4, 64);
    teach(&farm, 6, &five_minutes());
    let job = Job {
        gpus: 1,
        ..Job::new(1)
    };
    job.submit(&farm);
    assert!(farm.lock().sched.leases_on(&worker).is_empty());
    assert_eq!(made(&farm), 0);
    let want: Vec<u64> = ALL.iter().map(|w| u64::from(*w == Unset::Kind)).collect();
    assert_eq!(unset_counts(&farm), want);
}

/// Catches `off` doing anything: no prediction made or counted, and the lease charged as a
/// farm built with no mention of the flag charges it, whatever the key has learned.
#[test]
fn off_changes_nothing() {
    let charged = |farm: Farm| {
        let (worker, _) = node(&farm, "node-a", 4, 64);
        teach(&farm, 6, &five_minutes());
        Job::new(1).submit(&farm);
        let state = farm.lock();
        assert_eq!(state.side.learned.predictions_made(), 0);
        assert_eq!(state.side.learned.predictions_pending(), 0);
        drop(state);
        assert_eq!(unset_counts(&farm), [0; 8]);
        cost(&farm, &worker)
    };
    let off = charged(farm(PlacementPredictions::Off));
    booked(&off);
    let unmentioned = Farm::new_legacy(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_wall_epoch(20_000 * DAY)
    .with_learned_sizes(LearnedSizes::Shadow, 4 * GIB)
    .with_placement_load(PlacementLoad::On);
    assert_eq!(off, charged(unmentioned));
}

/// What a quiet success that used `peak` bytes and `cpu_us` of CPU time over `wall_us` carries,
/// as the farm reads a result.
fn measured(wall_us: u64, peak: u64, cpu_us: u64) -> crate::action_usage::Measured {
    use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
    use kbf_proto::worker::{CgroupUsage, ResourceUsage, UsageBasis};
    let usage = ResourceUsage {
        wall_micros: wall_us,
        cgroup: Some(CgroupUsage {
            memory_peak_bytes: Some(peak),
            cpu_usage_micros: Some(cpu_us),
            ..CgroupUsage::default()
        }),
        basis: UsageBasis::Cgroup.into(),
        ..ResourceUsage::default()
    };
    let result = kbf_proto::worker::Result {
        action_result: Some(ActionResult {
            execution_metadata: Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![usage.to_any()],
                ..ExecutedActionMetadata::default()
            }),
            ..ActionResult::default()
        }),
        ..kbf_proto::worker::Result::default()
    };
    crate::action_usage::Measured::of(&result).expect("a usage")
}

/// The operations the scheduler holds a lease for, with the lease, oldest first.
fn leased(farm: &Farm) -> Vec<(OperationId, LeaseId)> {
    let state = farm.lock();
    state
        .sched
        .unfinished()
        .filter_map(|op| match state.sched.state(op) {
            Some(kbf_sched::OpState::Leased { lease, .. }) => Some((op, *lease)),
            Some(kbf_sched::OpState::Running { lease, .. }) => Some((op, *lease)),
            _ => None,
        })
        .collect()
}

/// The farm takes the success of `lease` of `operation`, which `measured` describes.
fn succeed(
    farm: &Farm,
    worker: &WorkerId,
    (operation, lease): (OperationId, LeaseId),
    measured: crate::action_usage::Measured,
) {
    let report = super::report::Report {
        lease,
        outcome: kbf_types::Outcome::Completed {
            action_result: Digest::new(DigestFunction::Sha256, [1; 32], 1).into(),
        },
        measured: Some(measured),
        exit_code: Some(0),
        run: None,
    };
    let now = farm.now();
    farm.lock().take_report(now, worker, operation, report);
}

/// Asserts shadow end to end: the scheduler is charged exactly what `off` charges (a core,
/// no memory), each operation is counted as predicted, and each finished run is counted in the
/// bucket it falls in on each axis against the prediction made at submit: three runs, one in
/// every bucket of every axis (predicted: 1,500 millicores, a memory M, six minutes of life),
/// and the scrape shows the counts after a tick.
#[tokio::test(start_paused = true)]
async fn shadow_sets_nothing_and_counts_each_run_against_its_prediction() {
    let cache = Arc::new(Cache::memory());
    let farm = Farm::new_legacy(
        cache.clone(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_wall_epoch(20_000 * DAY)
    .with_learned_sizes(LearnedSizes::Shadow, 4 * GIB)
    .with_placement_load(PlacementLoad::On)
    .with_placement_predictions(PlacementPredictions::Shadow);
    let (worker, _) = node(&farm, "node-a", 4, 64);
    teach(&farm, 6, &five_minutes());
    Job::new(1).submit(&farm);
    booked(&cost(&farm, &worker));
    Job::new(2).submit(&farm);
    Job::new(3).submit(&farm);
    assert_eq!(farm.lock().sched.leases_on(&worker).len(), 3);
    assert_eq!(made(&farm), 3);
    assert_eq!(farm.lock().side.learned.predictions_pending(), 3);
    let memory = {
        let state = farm.lock();
        let lease = state.sched.leases_on(&worker)[0];
        assert_eq!(
            state
                .sched
                .lease_cost(lease)
                .expect("a charge")
                .memory_bytes,
            0
        );
        // M, the estimate's memory, from the same row.
        let bounds = Bounds {
            default_memory: 4 * GIB,
            host_memory: 64 * GIB,
            node_cpu_millis: 4_000,
        };
        state
            .side
            .learned
            .summaries()
            .estimate(&key(), state.side.learned.day(), &bounds, &Hint::default())
            .expect("a warm key")
            .memory_bytes
    };
    let held = leased(&farm);
    assert_eq!(held.len(), 3);
    // (A): 90 s, 100 MiB, 1,500 m: cores fit, memory over, life over (minute 2 against 6).
    succeed(
        &farm,
        &worker,
        held[0],
        measured(90_000_000, 100 * MIB, 135_000_000),
    );
    // (B): 10 minutes, 3 GiB, 3,000 m: under on every axis (life 11).
    succeed(
        &farm,
        &worker,
        held[1],
        measured(10 * MINUTE, 3 * GIB, 1_800_000_000),
    );
    // (C): 3 minutes, three quarters of M, 700 m: cores over, memory fit, life fit (4).
    succeed(
        &farm,
        &worker,
        held[2],
        measured(3 * MINUTE, memory / 4 * 3, 126_000_000),
    );
    let counts = |axis| {
        let state = farm.lock();
        [Fit::Under, Fit::Fit, Fit::Over]
            .map(|fit| state.side.learned.predictions_vs_run(axis, fit))
    };
    assert_eq!(counts(Axis::Cpu), [1, 1, 1]);
    assert_eq!(counts(Axis::Mem), [1, 1, 1]);
    assert_eq!(counts(Axis::Life), [1, 1, 1]);
    assert_eq!(farm.lock().side.learned.predictions_pending(), 0);
    farm.tick().await;
    let scraped = &cache.metrics().prediction;
    assert_eq!(scraped.made(), 3);
    for axis in 0..3 {
        for bucket in 0..3 {
            assert_eq!(
                scraped.vs_run(axis, bucket),
                1,
                "axis {axis} bucket {bucket}"
            );
        }
    }
    for reason in 0..8 {
        assert_eq!(scraped.unset(reason), 0);
    }
}

/// A store that holds the estimator's rows for the key and two unfinished operations: one
/// leased to a node of one core, and one that asked for two cores and so waits.
fn stored() -> Arc<Fake> {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let first = over(store.clone(), PlacementPredictions::Off);
    let (_worker, _) = node(&first, "node-a", 1, 64);
    teach(&first, 6, &five_minutes());
    first.write_behind_now();
    Job {
        memory: 256 * MIB,
        ..Job::new(1)
    }
    .submit(&first);
    Job {
        memory: 256 * MIB,
        cpu_millis: 2_000,
        ..Job::new(2)
    }
    .submit(&first);
    assert_eq!(first.lock().sched.unfinished().count(), 2);
    store
}

/// A farm over `store`, as a start builds it.
fn over(store: Arc<dyn Store>, predictions: PlacementPredictions) -> Farm {
    farm(predictions)
        .with_state(Some(StateStore {
            store,
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store")
}

/// Catches `on` that predicts a new operation only (an operation queued over a restart would
/// be charged as `off` charges it until it ends), a stored operation predicted before the
/// summaries are read back, one whose lease the store holds being predicted, and a prediction
/// below what the request books (its client's evidence is not stored, so each figure of the
/// request is taken as named: the operation asked for two cores, the estimate is one and a
/// half, and the charge is two).
#[tokio::test(start_paused = true)]
async fn a_restart_predicts_the_queued_operations_from_the_stored_rows() {
    let restarted = over(stored(), PlacementPredictions::On);
    assert_eq!(made(&restarted), 1, "the queued one, not the leased one");
    let (worker, _) = node(&restarted, "node-b", 4, 64);
    restarted.tick().await;
    let held = leased(&restarted);
    assert_eq!(held.len(), 1, "the queued operation is placed");
    let charged = charged(&restarted, held[0].1);
    assert_eq!(charged.cpu_millis, 2_000);
    assert!(charged.memory_bytes > 500 * MIB && charged.memory_bytes < 4 * GIB);
    assert!(!charged.cold);
    assert_eq!(restarted.lock().sched.leases_on(&worker).len(), 1);
    assert_eq!(unset_counts(&restarted), [0; 8]);
}

/// Catches a stored operation the estimator cannot be asked about given a prediction anyway,
/// or counted under another reason: a GPU action and a whole-machine request (not asked), one
/// whose subject the store did not keep or whose action describes nothing, and one whose
/// `needs` pin no OS. The one that can be named is predicted, and nothing else is.
#[test]
fn a_stored_operation_that_cannot_be_named_is_counted_and_gets_no_prediction() {
    use super::durable::StoredOperation;
    let farm = farm(PlacementPredictions::On);
    teach(&farm, 6, &five_minutes());
    let (named, _) = Subject::of(Described {
        arguments: vec!["cc".to_owned()],
        ..Described::default()
    });
    let (empty, _) = Subject::of(Described::default());
    let linux = kbf_caps::Request::from_platform([("OSFamily", "linux")]).unwrap();
    let action = ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [1; 32], 1),
    };
    let stored =
        |kind: LeaseKind, gpus: u64, needs: &kbf_caps::Request, subject: Option<&Subject>| {
            StoredOperation {
                operation: OperationId(1),
                action: action.clone(),
                request: kbf_sched::Request {
                    key: (&action).into(),
                    qos: Qos::Ci,
                    kind,
                    resources: Resources::new(1_000, 256 * MIB).with_gpus(gpus),
                    hermetic: true,
                    do_not_cache: false,
                    needs: needs.clone(),
                    whole_worker: false,
                },
                lease: None,
                losses: 0,
                fate: None,
                subject: subject.cloned(),
                size_key: None,
                max_wall: None,
            }
        };
    let state = &mut *farm.lock();
    let mut predict = |op: StoredOperation| state.side.predict_stored(&state.engine.sched, &op);
    let none = kbf_caps::Request::default();
    assert_eq!(
        predict(stored(LeaseKind::Action, 1, &linux, Some(&named))),
        None
    );
    assert_eq!(
        predict(stored(LeaseKind::WholeMachine, 0, &linux, Some(&named))),
        None
    );
    assert_eq!(predict(stored(LeaseKind::Action, 0, &linux, None)), None);
    assert_eq!(
        predict(stored(LeaseKind::Action, 0, &linux, Some(&empty))),
        None
    );
    assert_eq!(
        predict(stored(LeaseKind::Action, 0, &none, Some(&named))),
        None
    );
    let counted = |why| state.side.learned.predictions_unset(why);
    assert_eq!(
        ALL.map(counted),
        [0, 0, 0, 0, 0, 2, 1, 2],
        "kind twice, no OS once, no subject twice"
    );
    assert_eq!(state.side.learned.predictions_made(), 0);
    let predicted = state
        .side
        .predict_stored(
            &state.engine.sched,
            &stored(LeaseKind::Action, 0, &linux, Some(&named)),
        )
        .expect("the one that can be named");
    assert_eq!(predicted.cpu_millis, 1_500);
    assert!(!predicted.cold);
    assert_eq!(state.side.learned.predictions_made(), 1);
}

/// Catches a restart that sets a prediction with `off` or in `shadow`: the operation is
/// charged its request's cores and no memory, as it is without the flag.
#[tokio::test(start_paused = true)]
async fn a_restart_sets_nothing_unless_on() {
    for mode in [PlacementPredictions::Off, PlacementPredictions::Shadow] {
        let farm = over(stored(), mode);
        let (_worker, _) = node(&farm, "node-b", 4, 64);
        farm.tick().await;
        let held = leased(&farm);
        assert_eq!(held.len(), 1, "{mode:?}");
        let charged = charged(&farm, held[0].1);
        assert_eq!(
            charged.cpu_millis, 2_000,
            "{mode:?}: what the request asked for"
        );
        assert_eq!(charged.memory_bytes, 0, "{mode:?}");
    }
}

/// Catches the flag lost between the parsed flags and the farm a server builds, in either
/// order of the builders.
#[test]
fn the_farm_a_server_builds_takes_the_flag_in_either_order() {
    let args = Args::parse_from([
        "kbf-server",
        "--learned-sizes=shadow",
        "--placement-load=on",
        "--placement-predictions=on",
    ]);
    let listeners = args.listeners().expect("listeners");
    let built = Farm::from_listeners(Arc::new(Cache::memory()), &listeners);
    assert_eq!(
        built.lock().side.learned.predictions(),
        PlacementPredictions::On
    );
    let plain = || {
        Farm::new_legacy(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
    };
    let before = plain()
        .with_placement_predictions(PlacementPredictions::Shadow)
        .with_learned_sizes(LearnedSizes::Shadow, 4 * GIB);
    assert_eq!(
        before.lock().side.learned.predictions(),
        PlacementPredictions::Shadow
    );
    let after = plain()
        .with_learned_sizes(LearnedSizes::Shadow, 4 * GIB)
        .with_placement_predictions(PlacementPredictions::Shadow);
    assert_eq!(
        after.lock().side.learned.predictions(),
        PlacementPredictions::Shadow
    );
    let off = plain().with_learned_sizes(LearnedSizes::Shadow, 4 * GIB);
    assert_eq!(
        off.lock().side.learned.predictions(),
        PlacementPredictions::Off
    );
}

/// A reading of a node whose actions cgroup has a CPU quota (`limited`), `seq` seconds in.
fn reading(seq: u64, limited: bool) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 1_000_000,
        actions_cpu_limited: limited,
        ..NodeLoad::default()
    }
}

/// Asserts the misconfiguration is surfaced: with `--placement-load on`, a node that reports
/// a CPU quota is counted in the gauge and warned about once for each time it starts to
/// report one, and the gauge falls when it stops; with placement load off nothing is said.
/// Catches a quota that goes unsaid, a warning on every heartbeat, a gauge that stays up for a
/// node that stopped, and a warning where the load is not read.
#[tokio::test(start_paused = true)]
async fn a_cpu_quota_under_placement_load_on_is_counted_and_warned_of_once() {
    let log = crate::test_log::capture();
    let cache = Arc::new(Cache::memory());
    let farm = Farm::new_legacy(
        cache.clone(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_placement_load(PlacementLoad::On);
    let (worker, stream) = node(&farm, "node-quota", 4, 64);
    let said = || {
        log.text()
            .matches("actions cgroup has a CPU quota while --placement-load is on")
            .count()
    };
    let limited_nodes = || cache.metrics().prediction.cpu_limited_nodes();
    farm.note_node_load(&worker, stream, Some(&reading(1, false)));
    farm.tick().await;
    assert_eq!((limited_nodes(), said()), (0, 0));
    farm.note_node_load(&worker, stream, Some(&reading(2, true)));
    farm.tick().await;
    assert_eq!((limited_nodes(), said()), (1, 1));
    assert!(
        log.text().contains("node-quota"),
        "the warning names the node"
    );
    farm.note_node_load(&worker, stream, Some(&reading(3, true)));
    farm.tick().await;
    assert_eq!(
        (limited_nodes(), said()),
        (1, 1),
        "once, not on each heartbeat"
    );
    farm.note_node_load(&worker, stream, Some(&reading(4, false)));
    farm.tick().await;
    assert_eq!((limited_nodes(), said()), (0, 1));
    farm.note_node_load(&worker, stream, Some(&reading(5, true)));
    assert_eq!(said(), 2, "a new quota is a new warning");
    farm.tick().await;
    assert_eq!(limited_nodes(), 1);
    // No reading for more than three windows (of a second each): the node is not reporting
    // a quota now.
    tokio::time::advance(std::time::Duration::from_secs(4)).await;
    farm.tick().await;
    assert_eq!(limited_nodes(), 0, "a stale reading is not counted");

    let quiet_cache = Arc::new(Cache::memory());
    let off = Farm::new_legacy(
        quiet_cache.clone(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let (worker, stream) = node(&off, "node-off", 4, 64);
    off.note_node_load(&worker, stream, Some(&reading(1, true)));
    off.tick().await;
    assert_eq!(quiet_cache.metrics().prediction.cpu_limited_nodes(), 0);
    assert_eq!(said(), 2, "nothing is said where the load is not read");
}
