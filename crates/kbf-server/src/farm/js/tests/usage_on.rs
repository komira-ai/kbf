//! `--usage-profiles on` through the farm's own construction: the curve source the core reads is
//! the one of the usage side the farm keeps, on the first build over a state store and after a
//! rebuild on a builder change (which opens the usage side again).
//!
//! The scheduler counts an admission of an action whose key has a curve and that was charged
//! flat (`curve_flat`), and a node that has sent no reading is charged by prediction alone, so
//! a curve that the farm's usage side learned shows in that count exactly when the core asks the
//! farm's own curve source. Nothing here reads a clock or waits.

use std::sync::Arc;

use js_usage::{RunMinute, RunSeries};
use kbf_front::Dispatch as _;
use kbf_state::Store;
use kbf_types::Resources;

use super::hot::{backend, farm_on, join, linux};
use super::js;
use super::usage::{fake, state, submission};
use crate::farm::Farm;
use crate::farm::js::wall_minute;
use crate::{PlacementLoad, SchedulerCore, UsageProfiles};

const GIB: u64 = 1 << 30;

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

/// Submits action `tag` of the shape named by `shape` (the first argument of its command) and
/// returns how many admissions so far were charged flat
/// though their key has a curve.
fn flat_with_curve_after(farm: &Farm, tag: u8, shape: &str) -> u64 {
    let mut submitted = submission(tag);
    submitted.described.arguments = vec![shape.to_owned()];
    let _ticket = farm.submit(submitted).expect("queued");
    farm.load_counters().curve_flat
}

/// Teaches the farm's own usage side three runs of the shape of action `tag`, named by `shape`.
fn learn(farm: &Farm, tag: u8, shape: &str) {
    let _ = flat_with_curve_after(farm, tag, shape);
    let usage = js(farm).usage.as_ref().expect("a usage side under on");
    let action = kbf_types::ActionKey {
        instance: "main".to_owned(),
        action: kbf_types::Digest::new(
            kbf_types::DigestFunction::Sha256,
            [tag; 32],
            u64::from(tag),
        ),
    };
    let profile = usage.shapes().get(&action).expect("the shape is named");
    let today = wall_minute(js(farm).ambient) / 1440;
    for _ in 0..3 {
        usage
            .sink()
            .run_ended(&profile, &quiet_run(), today)
            .expect("a run the profile takes");
    }
}

fn node(farm: &Farm) -> super::hot::Joined {
    let mut reported = linux();
    reported.capacity = Resources::new(8_000, 64 * GIB);
    join(farm, "node-o", reported)
}

/// Catches a core built with a curve source other than the farm's own usage side's, on the
/// first build over a state store and again after a rebuild: a run fed through the farm's usage
/// side must be charged by the core as a key with a curve (flat on a node with no reading, so
/// counted `curve_flat`) before the rebuild and, for a shape learned only after it, through the
/// usage side the rebuild opened, which is not the first one. It pins behaviour the farm already
/// has, so it passes on the commit that adds it.
#[test]
fn the_core_reads_the_curves_of_the_usage_side_the_farm_keeps_through_a_rebuild() {
    let store: Arc<dyn Store> = fake();
    let farm = farm_on(SchedulerCore::Js, &backend())
        .with_usage_profiles(UsageProfiles::On)
        .with_placement_load(PlacementLoad::On)
        .with_state(Some(state(&store)))
        .expect("a farm over the store");
    let _joined = node(&farm);
    assert_eq!(
        flat_with_curve_after(&farm, 1, "compile"),
        0,
        "no curve yet"
    );
    learn(&farm, 2, "compile");
    assert_eq!(
        flat_with_curve_after(&farm, 3, "compile"),
        1,
        "the farm's usage side has a curve and the core read it"
    );
    let first = Arc::clone(js(&farm).usage.as_ref().expect("a usage side"));

    let farm = farm.with_memory_floors(5);
    let second = js(&farm).usage.as_ref().expect("a usage side");
    assert!(
        !Arc::ptr_eq(&first, second),
        "the rebuild opened another usage side"
    );
    let _joined = node(&farm);
    learn(&farm, 4, "link");
    assert_eq!(
        flat_with_curve_after(&farm, 5, "link"),
        1,
        "the rebuilt core reads the curves of the usage side the rebuild opened"
    );
}
