//! Tests of the js core's construction (`super`), run in a build with `farm-core-js`.

use std::sync::Arc;
use std::time::Duration;

use kbf_front::Cache;
use kbf_sched::{
    FINISHED_RETENTION, LOSS_REQUEUES, MemoryLadder, UNSERVABLE_WAIT, WholeNodeConfig,
};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};

use super::*;
use crate::PlacementLoad;
use crate::farm::EpochPolicy;
use crate::state_store::StateStore;

fn farm() -> Farm {
    Farm::new(
        Arc::new(Cache::memory()),
        UNSERVABLE_WAIT,
        FINISHED_RETENTION,
    )
}

fn js(farm: &Farm) -> &JsFarm {
    match &farm.driver {
        Driver::Js(js) => js,
        Driver::Legacy(_) => panic!("the farm runs on the legacy driver"),
    }
}

/// Catches `Farm::new` building the legacy driver in a build with the feature (every farm
/// suite would then run on the core it is not meant to test), a farm that takes a state
/// store over without building its core over it (no epoch), and a term or lease epoch that
/// is not the epoch the core opened under: `Welcome` and every lease id would name an epoch
/// the store does not hold. The farm's ambient term is fixed to a value no process term
/// equals, so the epoch cannot be the farm's earlier term.
#[test]
fn a_farm_is_built_on_the_js_core_and_takes_its_term_and_lease_epoch_from_the_stores_epoch() {
    let mut farm = farm();
    assert_eq!(js(&farm).core.epoch(), None, "no store, no epoch");
    assert_eq!(farm.term(), farm.lease_epoch());
    let fixed = 7_000_000_u64;
    match &mut farm.driver {
        Driver::Js(js) => {
            js.ambient = Ambient::Fixed {
                term: fixed,
                started: tokio::time::Instant::now(),
                unix_ms: 0,
            };
        }
        Driver::Legacy(_) => panic!("the farm runs on the legacy driver"),
    }
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    kbf_state::share(&*store).expect("the shared marker");
    let farm = farm
        .with_state(Some(StateStore {
            store,
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
    let epoch = js(&farm).core.epoch().expect("a store has an epoch");
    assert_eq!(epoch, fixed);
    assert_eq!(farm.term(), epoch);
    assert_eq!(farm.lease_epoch(), epoch);
    assert_eq!(js(&farm).term, epoch);
}

/// Catches a builder that is lost on the js core (the core takes its configuration at
/// construction, so a setting that is only stored never reaches it), and a configuration
/// that leaves out the ladder, floors, loss requeues, whole-node settings, placement mode or
/// orphan wait: the core would schedule as its defaults do, not as the flags say.
#[test]
fn the_cores_configuration_carries_every_setting_of_the_farm() {
    let settings = Settings {
        ladder: MemoryLadder::default(),
        floors: 3,
        placement_load: PlacementLoad::Off,
        whole_node: Some(WholeNodeConfig::dedicated(2)),
        orphan_wait: Duration::from_secs(9),
        ..Settings::default()
    };
    let config = open::farm_config(
        11,
        Duration::from_secs(5),
        Duration::from_secs(6),
        &settings,
    );
    assert_eq!(config.term, 11);
    assert_eq!(config.unservable_wait, Duration::from_secs(5));
    assert_eq!(config.finished_retention, Duration::from_secs(6));
    assert_eq!(config.memory_floors, Some(3));
    assert_eq!(config.memory_ladder, Some(MemoryLadder::default()));
    assert_eq!(config.loss_requeues, LOSS_REQUEUES);
    assert_eq!(config.orphan_wait, Duration::from_secs(9));
    assert_eq!(config.whole_node, Some(WholeNodeConfig::dedicated(2)));
    assert!(
        config.load.is_some(),
        "the placement mode is given explicitly"
    );
    let bare = open::farm_config(11, Duration::ZERO, Duration::ZERO, &Settings::default());
    assert!(bare.whole_node.is_none());
}

/// Catches the farm's builders not reaching a js farm: each is applied through a rebuild of
/// the core, after which the farm still runs on the js driver and holds the settings.
#[test]
fn the_builders_rebuild_the_core_and_keep_the_settings() {
    let farm = farm()
        .with_memory_floors(4)
        .with_placement_load(PlacementLoad::Off)
        .with_held_cap(17)
        .with_orphan_wait(Duration::from_secs(2))
        .with_whole_node(Some(WholeNodeConfig::dedicated(2)));
    let js = js(&farm);
    assert_eq!(js.settings.floors, 4);
    assert_eq!(js.settings.held_cap, 17);
    assert_eq!(js.settings.orphan_wait, Duration::from_secs(2));
    assert!(js.settings.whole_node.is_some());
}

/// Catches a farm on the js driver that names the legacy core (the gauge
/// `kbf_scheduler_core` and anything else reading it would say so too).
#[test]
fn a_js_farm_names_the_js_core() {
    assert_eq!(farm().scheduler_core(), crate::SchedulerCore::Js);
}
