//! `--usage-profiles on` on the js core: the curves the farm learned reach the scheduler, which
//! charges an action that has a prediction by its curve instead of flat. The core is driven
//! directly, as in `placement_load_tests`, with farm time given by the test: nothing here
//! sleeps or reads a clock, and what is asserted is the placement the core decided, the queue
//! it holds and what the scheduler counted, never how long anything took.
//!
//! The scene is two nodes of four cores. The warmed action is predicted two cores, which the
//! load rule charges a lease flat; its curve is a tenth of a core, which it charges at the
//! floor, three quarters of the flat cores (1500 m). The rule admits a node whose measured
//! load plus the charge is within 4400 m, so a node measured at 2600 m refuses the flat charge
//! (4600 m) and admits the curve's (4100 m), and a node at 4000 m refuses both.

use std::sync::Arc;

use js_usage::{ProfileKey, RunMinute, RunSeries, STALE_DAYS};
use kbf_front::{Backend, Cache, Described, Recorder, Submission};
use kbf_invocation::Evidence;
use kbf_sched::LoadCounters;
use kbf_state::Store;
use kbf_types::{ActionKey, Digest, DigestFunction, FarmTime};
use kbf_usage::UsageMode;

use super::placement_load_tests::{Node, at, counters};
use super::prediction_tests::{request, warm};
use super::testing::{Flaky, Rig};
use super::usage::Usage;
use super::{KbfAdapter, dispatch, open};
use crate::action_usage::Shapes;
use crate::farm::Settings;
use crate::learned::{Learned, LearnedSizes};
use crate::{EpochPolicy, PlacementLoad, PlacementPredictions};

const GIB: u64 = 1 << 30;
/// The day the profiles are made on, and its first wall minute.
const DAY: u64 = 20_000;
const MINUTE: u64 = DAY * 1_440;
/// What the two nodes of the scene measure: the first fits the curve and not the flat charge.
const FITS_THE_CURVE: u32 = 2_600;
const FITS_NEITHER: u32 = 4_000;
/// The farm time of the submit, after both nodes' second reading.
const SUBMIT_AT: u64 = 6_000;

fn settings(mode: UsageMode, predictions: PlacementPredictions) -> Settings {
    Settings {
        placement_load: PlacementLoad::On,
        predictions,
        usage: mode,
        ..Settings::default()
    }
}

fn action(tag: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
    }
}

/// A run of five minutes at a tenth of a core and 200 MiB.
fn quiet_run() -> RunSeries {
    RunSeries {
        minutes: (0..5)
            .map(|minute| RunMinute {
                minute,
                cores_milli: 100,
                ws_bytes: 200 << 20,
            })
            .collect(),
        flags: 0,
    }
}

/// A core over a fresh store, with the usage side `mode` says and the configuration the farm
/// builds for it, and the nodes `node-a` and `node-b`. The shape `compile` is warmed on
/// `node-a`, so that the estimator predicts it.
struct Scene {
    rig: Rig,
    usage: Arc<Usage>,
    a: Node,
    b: Node,
    profile: ProfileKey,
    /// What the scheduler had counted when the warming was done.
    warmed: LoadCounters,
}

/// What the scheduler counted since the shape was warmed.
#[derive(Debug, PartialEq, Eq)]
struct Since {
    admitted: u64,
    admitted_fallback: u64,
    blocked: u64,
}

impl Scene {
    async fn new(mode: UsageMode, predictions: PlacementPredictions) -> Self {
        let store = Flaky::new();
        let shared: Arc<dyn Store> = store.clone();
        let backend: Backend = Arc::new(Cache::memory()).into();
        let usage = Arc::new(Usage::open(Arc::clone(&shared), MINUTE).expect("a usage side"));
        let config = open::farm_config(
            7,
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
            &settings(mode, predictions),
            Some(&usage),
        );
        let adapter = KbfAdapter::new(
            7,
            backend.clone(),
            Arc::clone(&shared),
            super::testing::wall(),
        )
        .recording(Recorder::off())
        .noting_usage(Some(Arc::clone(&usage)))
        .learning_with(
            Learned::new(LearnedSizes::Shadow, 4 * GIB, 0).with_predictions(predictions),
            true,
        );
        let core = open::open(
            &config,
            adapter,
            shared,
            EpochPolicy::Keep,
            FarmTime::default(),
        )
        .expect("a core over the store");
        let rig = Rig {
            core,
            store,
            backend,
        };
        let mut a = Node::register(&rig, "node-a");
        warm(&rig, &mut a.wire, "node-a").await;
        let b = Node::register(&rig, "node-b");
        let profile = usage
            .shapes()
            .get(&action(1))
            .expect("the warmed shape has a profile name");
        let warmed = counters(&rig);
        Self {
            rig,
            usage,
            a,
            b,
            profile,
            warmed,
        }
    }

    /// Three clean runs of the shape, which is what a curve takes.
    fn learn(&self) {
        for _ in 0..3 {
            self.usage
                .sink()
                .run_ended(&self.profile, &quiet_run(), DAY)
                .expect("a run the profile takes");
        }
    }

    /// The nodes' two readings each: `a` and `b` millicores busy.
    async fn measure(&mut self, a: u32, b: u32) {
        self.a.measured(&self.rig, a, false).await;
        self.b.measured(&self.rig, b, false).await;
    }

    /// Submits a new action of the shape at [`SUBMIT_AT`], which the profile index knows as it
    /// knows the warmed ones.
    fn submit(&self) {
        self.usage.shapes().insert(action(9), self.profile.clone());
        let submission = Submission {
            request: request(9),
            call: None,
            described: Described {
                arguments: vec!["compile".to_owned()],
                ..Described::default()
            },
            evidence: Evidence::default(),
            max_wall: None,
        };
        dispatch::submit(
            &self.rig.core,
            at(SUBMIT_AT),
            &Shapes::default(),
            submission,
        )
        .expect("queued");
    }

    /// Whether each of the two nodes was told to start something since the last call.
    fn started(&mut self) -> (bool, bool) {
        (self.a.was_started(), self.b.was_started())
    }

    fn queued(&self) -> usize {
        self.rig.core.with(|view, _| view.sched().queued_len())
    }

    fn counted(&self) -> LoadCounters {
        counters(&self.rig)
    }

    fn since(&self) -> Since {
        let now = self.counted();
        Since {
            admitted: now.admitted - self.warmed.admitted,
            admitted_fallback: now.admitted_fallback - self.warmed.admitted_fallback,
            blocked: now.blocked - self.warmed.blocked,
        }
    }
}

/// The value of the exported family `name` for `counters`.
fn exported(counters: &LoadCounters, name: &str) -> u64 {
    let mut out = String::new();
    crate::load_metrics::render_counters(&mut out, counters);
    let prefix = format!("{name} ");
    out.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .expect("the family is written")
        .parse()
        .expect("a count")
}

/// Catches `on` not giving the core the curves, or giving them to `record`: of two nodes
/// that both refuse the action's flat charge, the one whose load leaves room for its curve
/// takes the lease under `on`, and under `record` the same farm queues it, with the same
/// readings and the same runs learned.
#[tokio::test(start_paused = true)]
async fn a_learned_curve_changes_the_placement() {
    let mut on = Scene::new(UsageMode::On, PlacementPredictions::On).await;
    on.learn();
    on.measure(FITS_THE_CURVE, FITS_NEITHER).await;
    on.submit();
    assert_eq!(on.started(), (true, false), "placed where the curve fits");
    assert_eq!(on.queued(), 0);
    assert_eq!(on.since().admitted, 1);

    let mut record = Scene::new(UsageMode::Record, PlacementPredictions::On).await;
    record.learn();
    record.measure(FITS_THE_CURVE, FITS_NEITHER).await;
    record.submit();
    assert_eq!(record.started(), (false, false), "charged flat, no node");
    assert_eq!(record.queued(), 1, "the action waits");
    assert_eq!(record.since().admitted, 0);
    assert!(
        record.since().blocked > 0,
        "both nodes were over the load rule at the flat charge, so the scheduler counted the queued action as blocked by load"
    );
}

/// Catches the counters not seeing the curve: under `on` the admission is charged by the
/// curve in CPU and in memory, and the exported families say so; under `record` both are 0.
/// Also catches a curve that is charged on a node the scheduler has no measurement of (it
/// charges by prediction alone there, and counts the key as having a curve it did not use).
#[tokio::test(start_paused = true)]
async fn the_curve_counters_move() {
    let mut on = Scene::new(UsageMode::On, PlacementPredictions::On).await;
    on.learn();
    on.measure(FITS_THE_CURVE, FITS_NEITHER).await;
    on.submit();
    let counted = on.counted();
    assert!(exported(&counted, "kbf_sched_load_profile_cpu_total") > 0);
    assert!(exported(&counted, "kbf_sched_load_profile_memory_total") > 0);
    assert_eq!(exported(&counted, "kbf_sched_load_curve_flat_total"), 0);

    let mut record = Scene::new(UsageMode::Record, PlacementPredictions::On).await;
    record.learn();
    record.measure(FITS_THE_CURVE, FITS_NEITHER).await;
    record.submit();
    let counted = record.counted();
    assert_eq!(exported(&counted, "kbf_sched_load_profile_cpu_total"), 0);
    assert_eq!(exported(&counted, "kbf_sched_load_profile_memory_total"), 0);

    // No reading from either node: both are charged by prediction alone, the action is
    // admitted, and its key's curve is not used.
    let unread = Scene::new(UsageMode::On, PlacementPredictions::On).await;
    unread.learn();
    unread.submit();
    let counted = unread.counted();
    let since = unread.since();
    assert_eq!(since.admitted, 1);
    assert_eq!(since.admitted_fallback, 1);
    assert_eq!(exported(&counted, "kbf_sched_load_curve_flat_total"), 1);
    assert_eq!(exported(&counted, "kbf_sched_load_profile_cpu_total"), 0);
}

/// Catches a curve read for an action the estimator has no prediction for (the documented
/// rule: a curve is read only with a prediction): with `--placement-predictions off` the key
/// has a curve and the action is charged what its request books, flat, and the scheduler
/// counts the key as having a curve it did not use.
#[tokio::test(start_paused = true)]
async fn an_action_without_a_prediction_is_charged_flat_under_on() {
    let mut on = Scene::new(UsageMode::On, PlacementPredictions::Off).await;
    on.learn();
    on.measure(FITS_THE_CURVE, FITS_NEITHER).await;
    on.submit();
    assert_eq!(on.started(), (true, false), "one core fits at 2600 m");
    assert_eq!(on.since().admitted, 1);
    let counted = on.counted();
    assert_eq!(counted.profile_cpu, 0);
    assert_eq!(counted.profile_memory, 0);
    assert_eq!(
        counted.curve_flat, 1,
        "the key has a curve and it was not used"
    );
}

/// Catches a curve idle for more than the staleness bound still charging: a curve last used on
/// day `D` is read until day `D + 60` and not on day `D + 61`, when the action is charged flat
/// and waits.
#[tokio::test(start_paused = true)]
async fn a_stale_curve_is_not_used() {
    for (today, placed) in [
        (DAY + STALE_DAYS, (true, false)),
        (DAY + STALE_DAYS + 1, (false, false)),
    ] {
        let mut on = Scene::new(UsageMode::On, PlacementPredictions::On).await;
        on.learn();
        on.usage.sink().curves().set_today(today);
        on.measure(FITS_THE_CURVE, FITS_NEITHER).await;
        on.submit();
        assert_eq!(on.started(), placed, "on day {today}");
        assert_eq!(on.queued(), usize::from(placed == (false, false)));
    }
}

/// Catches `on` changing anything for a farm that has learned nothing: with no profile the
/// two modes place the same action on the same node, hold the same queue and count the same.
#[tokio::test(start_paused = true)]
async fn on_with_no_profiles_equals_record() {
    let mut traces = Vec::new();
    for mode in [UsageMode::On, UsageMode::Record] {
        let mut scene = Scene::new(mode, PlacementPredictions::On).await;
        scene.measure(500, 1_500).await;
        scene.submit();
        traces.push((
            scene.started(),
            scene.queued(),
            scene.since(),
            scene.counted(),
        ));
    }
    assert_eq!(traces[0], traces[1]);
    assert_eq!(traces[0].0, (true, false), "the idle node took it");
    assert_eq!(traces[0].2.admitted, 1);
    assert_eq!(traces[0].3.profile_cpu, 0, "no curve was read");
}

/// Catches the configuration giving the scheduler curves in a mode that must not, or a
/// configuration rebuilt for the same usage side reading another source: only `on`, with a
/// usage side, carries the curves, and two configurations built from one usage side carry the
/// one source (the configuration's equality is the source's identity).
#[test]
fn the_configuration_carries_the_curves_only_under_on() {
    let store: Arc<dyn Store> = Flaky::new();
    let usage = Usage::open(store, MINUTE).expect("a usage side");
    let build = |mode: UsageMode, usage: Option<&Usage>| {
        open::farm_config(
            7,
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
            &settings(mode, PlacementPredictions::On),
            usage,
        )
    };
    let on = build(UsageMode::On, Some(&usage));
    assert_eq!(
        on,
        build(UsageMode::On, Some(&usage)),
        "one usage side, one source"
    );
    let bare = build(UsageMode::Record, None);
    assert_ne!(on, bare, "on carries the curves");
    assert_eq!(
        build(UsageMode::Record, Some(&usage)),
        bare,
        "record carries none"
    );
    assert_eq!(
        build(UsageMode::Off, Some(&usage)),
        bare,
        "off carries none"
    );
    assert_eq!(
        build(UsageMode::On, None),
        bare,
        "no usage side, none to give"
    );
}
