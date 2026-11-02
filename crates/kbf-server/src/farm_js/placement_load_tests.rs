//! `--placement-load` on the js core: what the node readings fed through a heartbeat do to
//! where an action goes, read back from the placement and from what the scheduler counts
//! about it ([`LoadCounters`]). The core is driven directly, with farm time given by the
//! test: nothing here sleeps or reads a clock.
//!
//! A reading is a pair of `NodeLoad`s five seconds apart on the node's own clock (the
//! scheduler's window), the second of which gives the busy cores of the window.

use std::sync::Arc;
use std::time::Duration;

use js_server_core::Beat;
use kbf_front::{Backend, Cache, Recorder};
use kbf_sched::{LoadConfig, LoadCounters};
use kbf_state::Store;
use kbf_types::sched::usage::NodeLoad;
use kbf_types::{FarmTime, WorkerId};

use super::testing::{Flaky, Rig, Wire, register, started, submit, wall};
use super::{KbfAdapter, open};
use crate::farm::Settings;
use crate::{EpochPolicy, PlacementLoad};

/// The scheduler's window, in microseconds on a node's clock.
const WINDOW_USEC: u64 = 5_000_000;
/// Three windows of the scheduler, in milliseconds: a reading older than this is stale.
const STALE_AFTER_MS: u64 = 15_000;
/// Cores the nodes of `register` have, and the most CPU the load rule charges one above
/// them (`s_cpu` of 10%), in millicores. An action of `testing::request` asks for one core.
const LIMIT_MILLIS: u32 = 4_400;
const ONE_CORE: u32 = 1_000;

pub(super) fn at(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis)
}

/// A core of term 7 over a fresh store that places as `placement` says.
fn rig_placing(placement: PlacementLoad) -> Rig {
    let store = Flaky::new();
    let shared: Arc<dyn Store> = store.clone();
    let backend: Backend = Arc::new(Cache::memory()).into();
    let adapter =
        KbfAdapter::new(7, backend.clone(), Arc::clone(&shared), wall()).recording(Recorder::off());
    let config = open::config(
        7,
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
        placement,
    );
    let core = open::open(
        &config,
        adapter,
        shared,
        EpochPolicy::Keep,
        FarmTime::default(),
    )
    .expect("a core over the store");
    Rig {
        core,
        store,
        backend,
    }
}

/// One node's readings: a counter of CPU time and a clock that run on, window by window.
#[derive(Default)]
struct Dial {
    seq: u64,
    mono_usec: u64,
    cpu_usec: u64,
}

impl Dial {
    /// The next reading, a window after the last, in which `millis` millicores were busy.
    fn read(&mut self, millis: u32, limited: bool) -> NodeLoad {
        self.seq += 1;
        self.mono_usec += WINDOW_USEC;
        self.cpu_usec += u64::from(millis) * WINDOW_USEC / 1_000;
        NodeLoad {
            seq: self.seq,
            mono_usec: self.mono_usec,
            cpu_usage_usec: self.cpu_usec,
            actions_cpu_limited: limited,
            ..NodeLoad::default()
        }
    }
}

/// A node of the rig, registered and heard from.
pub(super) struct Node {
    pub(super) name: &'static str,
    pub(super) wire: Wire,
    dial: Dial,
    beats: u64,
}

impl Node {
    pub(super) fn register(rig: &Rig, name: &'static str) -> Self {
        Self {
            name,
            wire: register(rig, name, 0),
            dial: Dial::default(),
            beats: 0,
        }
    }

    /// A heartbeat at farm time `ms`, carrying `load` if there is one.
    async fn beat(&mut self, rig: &Rig, ms: u64, load: Option<NodeLoad>) {
        self.beats += 1;
        let beat = Beat::new(self.beats, Vec::new());
        let beat = match load {
            Some(load) => beat.with_load(load),
            None => beat,
        };
        let heard = rig
            .core
            .heartbeat(at(ms), &WorkerId::new(self.name), self.wire.stream, beat)
            .await
            .expect("a node the core hears");
        assert!(heard, "{} is acknowledged", self.name);
    }

    /// The two readings that give a window, the first at 1 s and the second at 6 s of farm
    /// time, in which `millis` millicores were busy.
    pub(super) async fn measured(&mut self, rig: &Rig, millis: u32, limited: bool) {
        let baseline = self.dial.read(0, limited);
        self.beat(rig, 1_000, Some(baseline)).await;
        let window = self.dial.read(millis, limited);
        self.beat(rig, 6_000, Some(window)).await;
    }

    /// Whether the core told the node to start an action since the last call.
    pub(super) fn was_started(&mut self) -> bool {
        started(&self.wire.sent()).is_some()
    }
}

pub(super) fn counters(rig: &Rig) -> LoadCounters {
    rig.core.with(|view, _| view.sched().load_counters())
}

/// Whether the scheduler charges `name` by prediction alone, with no usable measurement.
fn charged_by_prediction(rig: &Rig, name: &str) -> bool {
    rig.core.with(|view, _| {
        view.sched()
            .load_view(&WorkerId::new(name))
            .expect("a registered node")
            .fallback
    })
}

/// Two nodes, `hot` busy for `hot_millis` and the other idle, and one action submitted at
/// 6 s. Returns the name of the node it started on and the counters after.
async fn two_nodes(
    placement: PlacementLoad,
    hot: &str,
    hot_millis: u32,
) -> (&'static str, LoadCounters) {
    let rig = rig_placing(placement);
    let mut a = Node::register(&rig, "node-a");
    let mut b = Node::register(&rig, "node-b");
    for node in [&mut a, &mut b] {
        let millis = if node.name == hot { hot_millis } else { 0 };
        node.measured(&rig, millis, false).await;
    }
    submit(&rig, 1, 6_000).expect("queued");
    let on_a = a.was_started();
    let on_b = b.was_started();
    assert_ne!(on_a, on_b, "the action started on exactly one node");
    (if on_a { "node-a" } else { "node-b" }, counters(&rig))
}

/// Catches `on` ranking by booking alone and ignoring what the nodes measured: of two nodes
/// with nothing booked, one busy for half its cores and one idle, the idle one gets the
/// action, whichever of them the booked rule's name order would pick. Also catches the
/// choice not being the load rule's (`scored`, and not by a tie-break), and a reading that
/// is not taken.
#[tokio::test(start_paused = true)]
async fn on_places_on_the_node_with_the_lower_measured_load() {
    for (hot, cool) in [("node-a", "node-b"), ("node-b", "node-a")] {
        let (placed, counted) = two_nodes(PlacementLoad::On, hot, 2_000).await;
        assert_eq!(placed, cool, "{hot} is the busy one");
        assert_eq!(counted.admitted, 1);
        assert_eq!(counted.scored, 1, "two nodes admitted, so the score chose");
        assert_eq!(
            counted.chose_by_tie_break, 0,
            "decided by the load, not a tie"
        );
        assert_eq!(
            counted.readings_accepted, 4,
            "a baseline and a window for each node"
        );
        assert_eq!(counted.admitted_fallback, 0, "both nodes were measured");
    }
}

/// Catches `off` reading the load: the same two nodes with the same readings place by the
/// booked rule, so the node that gets the action is the same whichever of them is busy, and
/// the scheduler counts no admission by load.
#[tokio::test(start_paused = true)]
async fn off_places_the_same_whichever_node_is_busy() {
    let (when_a_is_busy, counted) = two_nodes(PlacementLoad::Off, "node-a", 2_000).await;
    let (when_b_is_busy, _) = two_nodes(PlacementLoad::Off, "node-b", 2_000).await;
    assert_eq!(
        when_a_is_busy, when_b_is_busy,
        "the readings moved the action"
    );
    assert_eq!(counted.admitted, 0, "admitted by load");
    assert_eq!(counted.scored, 0, "scored by load");
}

/// Whether a single action submitted at 6 s started on a node that `measured` `millis`, and
/// the counters, or `None` for an action that stayed queued.
async fn one_node(placement: PlacementLoad, millis: u32, limited: bool) -> (bool, LoadCounters) {
    let rig = rig_placing(placement);
    let mut node = Node::register(&rig, "node-a");
    node.measured(&rig, millis, limited).await;
    submit(&rig, 1, 6_000).expect("queued");
    (node.was_started(), counters(&rig))
}

/// Catches the over-capacity bound being something else than the cores plus the 10% slack
/// (4400 m), measured plus the action's own core: a node busy for 3400 m takes one more core
/// (4400 m, at the bound) and a node busy for 3401 m does not. The refusal is counted as the
/// load blocking it and is not an admission.
#[tokio::test(start_paused = true)]
async fn on_refuses_a_node_over_capacity_and_admits_one_at_the_bound() {
    let (at_bound, counted) = one_node(PlacementLoad::On, LIMIT_MILLIS - ONE_CORE, false).await;
    assert!(at_bound, "4400 m is within the bound");
    assert_eq!(counted.admitted, 1);
    assert_eq!(counted.blocked, 0);

    let (over, counted) = one_node(PlacementLoad::On, LIMIT_MILLIS - ONE_CORE + 1, false).await;
    assert!(!over, "4401 m is over the bound");
    assert_eq!(counted.admitted, 0);
    assert!(counted.blocked > 0, "the load held the action back");
}

/// Catches a refusal that loses the action: the action refused on a busy node starts when
/// the node has been idle for two windows (the larger of the last two windows counts, so a
/// fall is believed after two), and not before.
#[tokio::test(start_paused = true)]
async fn an_action_refused_for_load_starts_when_the_node_cools() {
    let rig = rig_placing(PlacementLoad::On);
    let mut node = Node::register(&rig, "node-a");
    node.measured(&rig, 3_800, false).await;
    submit(&rig, 1, 6_000).expect("queued");
    assert!(!node.was_started(), "refused while busy");

    let one_cool = node.dial.read(0, false);
    node.beat(&rig, 11_000, Some(one_cool)).await;
    rig.core.tick(at(11_000)).await;
    assert!(!node.was_started(), "one idle window is not enough");

    let two_cool = node.dial.read(0, false);
    node.beat(&rig, 16_000, Some(two_cool)).await;
    rig.core.tick(at(16_000)).await;
    assert!(node.was_started(), "two idle windows are");
}

/// Catches `off` refusing by what a node measured: the node `on` refuses at 3401 m takes
/// the action under the booked rule, which asks only for a free core.
#[tokio::test(start_paused = true)]
async fn off_places_on_a_node_that_on_refuses() {
    let (on, _) = one_node(PlacementLoad::On, LIMIT_MILLIS - ONE_CORE + 1, false).await;
    let (off, counted) = one_node(PlacementLoad::Off, LIMIT_MILLIS - ONE_CORE + 1, false).await;
    assert!(!on);
    assert!(off, "the booked rule does not read the load");
    assert_eq!(counted.admitted, 0);
}

/// Catches a stale reading still charging the node: the node `on` refuses while its
/// reading is fresh is charged by prediction once the reading is older than three windows
/// (15 s of farm time), where an idle prediction admits the action and the scheduler counts
/// a fallback admission. The reading is fresh at the bound and stale one millisecond after.
#[tokio::test(start_paused = true)]
async fn a_stale_reading_falls_back_to_the_predicted_charge() {
    let rig = rig_placing(PlacementLoad::On);
    let mut node = Node::register(&rig, "node-a");
    node.measured(&rig, 3_800, false).await;
    let last = 6_000;

    node.beat(&rig, last + STALE_AFTER_MS, None).await;
    assert!(!charged_by_prediction(&rig, "node-a"), "fresh at the bound");
    submit(&rig, 1, last + STALE_AFTER_MS).expect("queued");
    assert!(!node.was_started(), "still refused on the measurement");

    node.beat(&rig, last + STALE_AFTER_MS + 1, None).await;
    assert!(
        charged_by_prediction(&rig, "node-a"),
        "stale one millisecond later"
    );
    rig.core.tick(at(last + STALE_AFTER_MS + 1)).await;
    assert!(node.was_started(), "charged by prediction, the action fits");
    let counted = counters(&rig);
    assert_eq!(counted.admitted, 1);
    assert_eq!(
        counted.admitted_fallback, 1,
        "on a node charged by prediction"
    );
}

/// Catches a node whose actions cgroup has a CPU limit being charged by its measurement
/// (the reading is of a cgroup the limit throttles, so it says nothing of the node): the
/// busy node that is refused unlimited is charged by prediction when the reading says it is
/// limited, takes the action, and is counted in `fallback_cpu_limited`.
#[tokio::test(start_paused = true)]
async fn a_cpu_limited_node_falls_back_to_the_predicted_charge() {
    let (unlimited, counted) = one_node(PlacementLoad::On, 3_800, false).await;
    assert!(!unlimited, "the same reading, not limited, is refused");
    assert_eq!(counted.fallback_cpu_limited, 0);

    let (limited, counted) = one_node(PlacementLoad::On, 3_800, true).await;
    assert!(limited, "limited, the node is charged by prediction");
    assert_eq!(counted.admitted_fallback, 1);
    assert_eq!(counted.fallback_cpu_limited, 1, "the node is counted");
}

/// Catches the library's default (it places by load) reaching a farm that did not ask for
/// it: the farm's settings default to `off`, the core's configuration for them carries `off`
/// explicitly (written out, so raising the library cannot change it), and `on` is the only
/// setting that carries the library's mode.
#[test]
fn the_default_under_js_is_off_whatever_the_library_defaults_to() {
    assert_eq!(
        LoadConfig::default().mode,
        kbf_sched::PlacementLoad::On,
        "the library places by load unless told not to"
    );
    let settings = Settings::default();
    assert_eq!(settings.placement_load, PlacementLoad::Off);
    let config = |settings: &Settings| {
        open::farm_config(7, Duration::ZERO, Duration::ZERO, settings, None)
            .load
            .expect("the placement mode is given explicitly")
    };
    assert_eq!(config(&settings), PlacementLoad::Off.config());
    assert_eq!(config(&settings).mode, kbf_sched::PlacementLoad::Off);
    let on = Settings {
        placement_load: PlacementLoad::On,
        ..Settings::default()
    };
    assert_eq!(config(&on).mode, kbf_sched::PlacementLoad::On);
}
