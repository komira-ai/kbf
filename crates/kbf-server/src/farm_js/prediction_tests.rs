//! Tests of `--placement-predictions` on a core driven through `farm_js::dispatch`: what the
//! scheduler is charged for a lease, what the node is told to book and run it with, and what
//! is counted. The legacy core's cases are in `farm/tests`.
//!
//! Each test warms the estimator with runs of one action (so its row is usable), then
//! submits a new operation of the same shape and reads the cost the scheduler holds for the
//! lease it got (`lease_cost`) and the `Start` the node was sent.

use std::sync::Arc;

use kbf_front::{Cache, Described, Recorder, Submission};
use kbf_invocation::{BookSource, Evidence};
use kbf_proto::google::rpc::Status as RpcStatus;
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::{self, CgroupUsage, ResourceUsage, UsageBasis, server_message};
use kbf_sched::Request;
use kbf_state::Store;
use kbf_types::WorkerId;
use kbf_types::sched::usage::LeaseCost;
use kbf_types::{ActionKey, Digest, DigestFunction, FarmTime, LeaseId, LeaseKind, Qos, Resources};

use super::ablation::Booking;
use super::dispatch;
use super::testing::{Flaky, Rig, Wire, register, result_with, starts, wall};
use super::{KbfAdapter, open};
use crate::action_usage::Shapes;
use crate::learned::{Axis, Fit, Learned, LearnedSizes, Unset};
use crate::{PlacementLoad, PlacementPredictions};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn at(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis)
}

/// A core of term 7 over a fresh store whose adapter learns in shadow and treats
/// predictions as `predictions` says, placing by `load`.
fn rig(load: PlacementLoad, predictions: PlacementPredictions) -> Rig {
    let store = Flaky::new();
    let shared: Arc<dyn Store> = store.clone();
    let backend: kbf_front::Backend = Arc::new(Cache::memory()).into();
    let adapter = KbfAdapter::new(7, backend.clone(), Arc::clone(&shared), wall())
        .recording(Recorder::off())
        .learning_with(
            Learned::new(LearnedSizes::Shadow, 4 * GIB, 0).with_predictions(predictions),
            true,
        );
    let config = open::config(
        7,
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
        load,
    );
    let core = open::open(
        &config,
        adapter,
        shared,
        crate::EpochPolicy::Keep,
        FarmTime::default(),
    )
    .expect("a core over the store");
    Rig {
        core,
        store,
        backend,
    }
}

/// The request of action `tag`: a core and 4 GiB, as the booked rule charges it.
pub(super) fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 4 * GIB),
        hermetic: true,
        do_not_cache: false,
        whole_worker: false,
        needs: kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("a platform"),
    }
}

/// A submit of action `tag` of the shape `compile`, whose client named `evidence`.
fn submit_with(rig: &Rig, request: Request, evidence: Evidence) -> kbf_front::Ticket {
    let submission = Submission {
        request,
        call: None,
        described: Described {
            arguments: vec!["compile".to_owned()],
            ..Described::default()
        },
        evidence,
        max_wall: None,
    };
    dispatch::submit(&rig.core, at(0), &Shapes::default(), submission).expect("queued")
}

pub(super) fn submit(rig: &Rig, tag: u8) -> kbf_front::Ticket {
    submit_with(rig, request(tag), Evidence::default())
}

/// A `Result` that exited 0, peaked at 100 MiB and used half a core over two seconds.
fn measured(lease: LeaseId) -> worker::Result {
    let usage = ResourceUsage {
        wall_micros: 2_000_000,
        basis: UsageBasis::Cgroup as i32,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(1_000_000),
            memory_peak_bytes: Some(100 * MIB),
            ..CgroupUsage::default()
        }),
        ..ResourceUsage::default()
    };
    result_with(
        lease,
        ActionResult {
            exit_code: 0,
            execution_metadata: Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![usage.to_any()],
                ..Default::default()
            }),
            ..Default::default()
        },
    )
}

/// A `Result` of a run the memory limit killed.
fn killed(lease: LeaseId) -> worker::Result {
    worker::Result {
        status: Some(RpcStatus {
            code: tonic::Code::ResourceExhausted as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: worker::MemoryKill::OwnLimit as i32,
        ..result_with(lease, ActionResult::default())
    }
}

async fn report(rig: &Rig, worker: &str, result: worker::Result) {
    let ack = dispatch::report(
        &rig.core,
        &rig.backend,
        &wall(),
        &|| at(0),
        &WorkerId::new(worker),
        result,
    )
    .await
    .expect("a host the core hears")
    .expect("an ack");
    assert!(ack.accepted);
}

/// Runs the actions `1..=WARM` to the end on `node`, so that the estimator holds a usable
/// row for the shape.
const WARM: u8 = 6;

pub(super) async fn warm(rig: &Rig, node: &mut Wire, name: &str) {
    for tag in 1..=WARM {
        let _ticket = submit(rig, tag);
        let lease = *starts(&node.sent()).first().expect("started");
        report(rig, name, measured(lease)).await;
    }
}

/// The cost the scheduler holds for `lease`.
pub(super) fn cost(rig: &Rig, lease: LeaseId) -> LeaseCost {
    rig.core
        .with(|view, _| view.sched().lease_cost(lease))
        .expect("the scheduler holds the lease")
}

/// The `(millicpus, memory_bytes)` of the first `Start` among `sent`.
fn booked(sent: &[server_message::Message]) -> (u64, u64) {
    sent.iter()
        .find_map(|message| match message {
            server_message::Message::Start(start) => Some((start.millicpus, start.memory_bytes)),
            _ => None,
        })
        .expect("a Start was sent")
}

/// What the farm counts of the predictions: made, and unset for `Unset::Cold`.
fn counted(rig: &Rig) -> (u64, u64) {
    rig.core.with(|_, adapter| adapter.publish_learned());
    let metrics = rig.backend.metrics();
    (
        metrics.prediction.made(),
        metrics.prediction.unset(Unset::Cold as usize),
    )
}

/// Starts action `tag`, a new operation of the warmed shape, on a node of `rig` and returns its
/// lease with what its `Start` booked.
async fn start_new(rig: &Rig, node: &mut Wire, tag: u8) -> (LeaseId, (u64, u64)) {
    let _ticket = submit(rig, tag);
    let sent = node.sent();
    let lease = *starts(&sent).first().expect("started");
    (lease, booked(&sent))
}

/// Asserts `on` charges the lease the estimator's cores and memory peak, flat: the figures
/// the row holds and nothing else, not cold and not a retry, and no limit other than the
/// request's; and that the node is told to book and run what the request asked for, whether
/// the prediction is above or below it (the cores here are below, the memory limit too). Catches: the
/// prediction not handed to the core, handed after the first placement round, a profile or
/// cold charge made from a flat prediction, and a prediction that reaches the booking.
#[tokio::test(start_paused = true)]
async fn on_charges_the_predicted_cores_and_memory_and_books_the_request() {
    let rig = rig(PlacementLoad::On, PlacementPredictions::On);
    let mut node = register(&rig, "node-on", 0);
    warm(&rig, &mut node, "node-on").await;

    // The request asks for more cores than the row holds, and for more memory.
    let mut asked = request(9);
    asked.resources = Resources::new(3_000, 4 * GIB);
    let _ticket = submit_with(&rig, asked, Evidence::default());
    let sent = node.sent();
    let lease = *starts(&sent).first().expect("started");
    let charged = cost(&rig, lease);
    assert_eq!(
        (charged.cpu_millis, charged.memory_bytes),
        (2_000, 1024 * MIB),
        "the row's cores and peak, which are not the request's"
    );
    assert!(!charged.cold && !charged.retry, "{charged:?}");
    assert_eq!(charged.limit_bytes, 4 * GIB, "the limit is the request's");
    assert_eq!(
        booked(&sent),
        (3_000, 4 * GIB),
        "the node books and runs the request's cores and memory, not the prediction's"
    );
}

/// Asserts the ablation's bookings (`kbf_ablation_bookings_total`) follow what admission
/// charged: a lease started on a prediction moves `prediction` and no other source, and the same
/// shape with predictions off moves `default`. Catches: a prediction booking counted as the
/// default, and a counter that follows the flag instead of the charge.
#[tokio::test(start_paused = true)]
async fn the_ablation_counts_a_predicted_start_as_a_prediction_booking() {
    for (mode, expect) in [
        (PlacementPredictions::On, Booking::Prediction),
        (PlacementPredictions::Off, Booking::Default),
    ] {
        let rig = rig(PlacementLoad::On, mode);
        let mut node = register(&rig, "node-book", 0);
        warm(&rig, &mut node, "node-book").await;
        let counts = |rig: &Rig| {
            rig.core
                .with(|_, adapter| Booking::ALL.map(|booking| adapter.ablation().bookings(booking)))
        };
        let before = counts(&rig);

        let _ = start_new(&rig, &mut node, 9).await;
        let after = counts(&rig);
        for booking in Booking::ALL {
            let moved = after[booking as usize] - before[booking as usize];
            assert_eq!(moved, u64::from(booking == expect), "{mode:?}: {booking:?}");
        }
    }
}

/// Asserts `shadow` leaves the charge at the booked rule's (the request's cores, no memory)
/// while the prediction is counted, and `off` counts nothing. Catches: a shadow that hands
/// the core its prediction, a shadow that counts nothing, and an off that counts.
#[tokio::test(start_paused = true)]
async fn shadow_counts_and_charges_the_booked_cost_and_off_counts_nothing() {
    for (mode, made) in [
        (PlacementPredictions::Shadow, true),
        (PlacementPredictions::Off, false),
    ] {
        let rig = rig(PlacementLoad::On, mode);
        let mut node = register(&rig, "node-mode", 0);
        warm(&rig, &mut node, "node-mode").await;
        let before = counted(&rig);

        let (lease, start) = start_new(&rig, &mut node, 9).await;
        let charged = cost(&rig, lease);
        assert_eq!(
            (charged.cpu_millis, charged.memory_bytes),
            (1_000, 0),
            "{mode:?}: the booked cost"
        );
        assert_eq!(start, (1_000, 4 * GIB));
        let after = counted(&rig);
        assert_eq!(after.0 - before.0, u64::from(made), "{mode:?}: made");
        if !made {
            assert_eq!(after, (0, 0), "{mode:?}: nothing is counted at all");
        }
    }
}

/// Asserts the load alone (predictions off) charges the booked cost on the same warmed
/// estimator, so the two flags are independent. Catches: the load rule reading the
/// estimator by itself.
#[tokio::test(start_paused = true)]
async fn the_load_without_predictions_charges_the_booked_cost() {
    let rig = rig(PlacementLoad::On, PlacementPredictions::Off);
    let mut node = register(&rig, "node-load", 0);
    warm(&rig, &mut node, "node-load").await;

    let (lease, start) = start_new(&rig, &mut node, 9).await;
    let charged = cost(&rig, lease);
    assert_eq!((charged.cpu_millis, charged.memory_bytes), (1_000, 0));
    assert_eq!(start, (1_000, 4 * GIB));
}

/// Asserts a prediction above the request is charged as it is and the node is still told the
/// request's figures, and that a client that named its size is never charged below it.
/// Catches: a booking that follows the prediction up, and a hint lowered.
#[tokio::test(start_paused = true)]
async fn the_booking_is_the_requests_whatever_the_prediction_is() {
    let rig = rig(PlacementLoad::On, PlacementPredictions::On);
    let mut node = register(&rig, "node-hint", 0);
    warm(&rig, &mut node, "node-hint").await;

    let hint = Evidence {
        memory_source: BookSource::Hint,
        ..Evidence::default()
    };
    let _ticket = submit_with(&rig, request(9), hint);
    let sent = node.sent();
    let lease = *starts(&sent).first().expect("started");
    let charged = cost(&rig, lease);
    assert!(
        charged.memory_bytes >= 4 * GIB,
        "the named size is a floor of the charge: {charged:?}"
    );
    assert_eq!(booked(&sent), (1_000, 4 * GIB));
}

/// Asserts a run the memory limit killed is charged again at its new rung, at least the
/// prediction: `max(prediction, limit)`, marked a retry. Catches: a retry charged the
/// learned peak below its limit.
#[tokio::test(start_paused = true)]
async fn a_retry_after_an_own_limit_kill_is_charged_the_larger_of_prediction_and_limit() {
    let rig = rig(PlacementLoad::On, PlacementPredictions::On);
    let mut node = register(&rig, "node-retry", 0);
    warm(&rig, &mut node, "node-retry").await;

    let (first, _) = start_new(&rig, &mut node, 9).await;
    let predicted = cost(&rig, first).memory_bytes;
    assert_eq!(predicted, 1024 * MIB, "the prediction was charged");
    report(&rig, "node-retry", killed(first)).await;
    rig.core.tick(at(1_000)).await;
    let sent = node.sent();
    let second = *starts(&sent).first().expect("run again");
    assert_ne!(second, first);
    let retried = cost(&rig, second);
    assert!(retried.retry, "{retried:?}");
    assert!(retried.limit_bytes > 4 * GIB, "up the ladder: {retried:?}");
    assert_eq!(
        retried.memory_bytes,
        predicted.max(retried.limit_bytes),
        "{retried:?}"
    );
    assert_eq!(booked(&sent).1, retried.limit_bytes);
}

/// Asserts a caller that joins an operation still waiting for a lease neither counts a
/// second prediction nor replaces the operation's cost, even with a client hint that would
/// make a fresh prediction larger. Catches: a prediction made before the submit that is
/// counted or given to the core for a joined caller.
#[tokio::test(start_paused = true)]
async fn a_joined_caller_neither_counts_nor_changes_the_cost_of_a_waiting_operation() {
    let rig = rig(PlacementLoad::On, PlacementPredictions::On);
    let mut node = register(&rig, "node-join", 0);
    warm(&rig, &mut node, "node-join").await;
    // With predictions on each lease is charged the row's cores (2000 m) and a 1 GiB peak, so
    // two leases fill the node's 4 cores, not its memory: the next operation waits for CPU.
    let (first, _) = start_new(&rig, &mut node, 20).await;
    let (_second, _) = start_new(&rig, &mut node, 21).await;
    let before = counted(&rig);

    let waiting = submit(&rig, 9);
    let mut big = request(9);
    big.resources = Resources::new(1_000, 6 * GIB);
    let hint = Evidence {
        memory_source: BookSource::Hint,
        ..Evidence::default()
    };
    let joined = submit_with(&rig, big, hint);
    assert_eq!(waiting.name, joined.name, "one operation");
    assert!(node.sent().is_empty(), "it waits");
    assert_eq!(counted(&rig).0 - before.0, 1, "one prediction");

    report(&rig, "node-join", measured(first)).await;
    rig.core.tick(at(1_000)).await;
    let sent = node.sent();
    let lease = *starts(&sent)
        .first()
        .expect("it runs once the node has room");
    assert_eq!(cost(&rig, lease).memory_bytes, 1024 * MIB);
}

/// Asserts `shadow` compares the run of an operation with the prediction made for it: the
/// run peaked at 100 MiB against 1 GiB predicted, so it is counted over on the memory axis,
/// once, and at the scrape. Catches: a shadow that counts a prediction and never compares it
/// with the run.
#[tokio::test(start_paused = true)]
async fn shadow_compares_the_run_with_its_prediction() {
    let rig = rig(PlacementLoad::On, PlacementPredictions::Shadow);
    let mut node = register(&rig, "node-compare", 0);
    warm(&rig, &mut node, "node-compare").await;
    let over = |rig: &Rig| {
        rig.core.with(|_, adapter| adapter.publish_learned());
        rig.backend
            .metrics()
            .prediction
            .vs_run(Axis::Mem as usize, Fit::Over as usize)
    };
    let before = over(&rig);

    let (lease, _) = start_new(&rig, &mut node, 9).await;
    report(&rig, "node-compare", measured(lease)).await;
    assert_eq!(over(&rig) - before, 1);
}

/// Asserts an action that asks for a GPU, and one whose request pins no OS, get no
/// prediction and are counted by why. Catches: the estimator asked about work it does not
/// size, and an operation with no prediction left out of the count.
#[tokio::test(start_paused = true)]
async fn a_gpu_action_and_an_action_with_no_os_get_no_prediction() {
    let rig = rig(PlacementLoad::On, PlacementPredictions::On);
    let mut node = register(&rig, "node-unset", 0);
    warm(&rig, &mut node, "node-unset").await;
    let unset = |rig: &Rig, why: Unset| {
        rig.core.with(|_, adapter| adapter.publish_learned());
        rig.backend.metrics().prediction.unset(why as usize)
    };
    let (kind, no_os) = (unset(&rig, Unset::Kind), unset(&rig, Unset::NoOs));

    let mut gpu = request(7);
    gpu.resources = Resources::new(1_000, 4 * GIB).with_gpus(1);
    let _ticket = submit_with(&rig, gpu, Evidence::default());
    let mut anywhere = request(8);
    anywhere.needs = kbf_caps::Request::default();
    let _ticket = submit_with(&rig, anywhere, Evidence::default());

    assert_eq!(unset(&rig, Unset::Kind) - kind, 1);
    assert_eq!(unset(&rig, Unset::NoOs) - no_os, 1);
}
