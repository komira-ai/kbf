//! `--placement-load`: the load rule reaches the farm's scheduler, read back from what the
//! scheduler counts about a placement.

use std::sync::Arc;

use kbf_front::{Cache, Submission};
use kbf_sched::LoadCounters;

use super::*;
use crate::PlacementLoad;

/// A farm with one node of four cores and one action submitted to it, which the
/// scheduler places at once. `mode` is `None` for a farm as [`Farm::new`] builds it.
fn placed(mode: Option<PlacementLoad>, test_core: bool) -> LoadCounters {
    let cache = Arc::new(Cache::memory());
    let farm = if test_core {
        Farm::for_test(
            cache,
            testkit::TestCore::new(
                7,
                0,
                kbf_sched::UNSERVABLE_WAIT,
                kbf_sched::FINISHED_RETENTION,
            ),
        )
    } else {
        Farm::new(
            cache,
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
    };
    let farm = match mode {
        Some(mode) => farm.with_placement_load(mode),
        None => farm,
    };
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, _inbox) = mpsc::unbounded_channel();
    farm.register(
        &WorkerId::new("node-a"),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        Reported::serving(kbf_types::Resources::new(4_000, 8 << 30), caps),
        outbound,
        ServerMessage::default(),
    );
    farm.submit(Submission {
        request: kbf_sched::Request {
            key: kbf_types::ActionKey {
                instance: "main".to_owned(),
                action: Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 1),
            }
            .into(),
            qos: kbf_types::Qos::Ci,
            kind: kbf_types::LeaseKind::Action,
            resources: kbf_types::Resources::new(1_000, 1 << 30),
            hermetic: true,
            do_not_cache: false,
            needs: kbf_caps::Request::default(),
        },
        call: None,
        described: kbf_front::Described::default(),
        evidence: Default::default(),
    })
    .expect("submitted");
    farm.lock().sched.load_counters()
}

/// Catches a farm placing by load without being told to (the library's default is to
/// place by load: a scheduler built without a configuration of kbf's would admit by the
/// load rule and count `admitted`), an `on` the scheduler never hears of, and a test farm
/// or a farm given `off` that is not on the booked rule.
#[test]
fn a_farm_places_by_the_rule_its_flag_names() {
    for (name, counters) in [
        ("Farm::new", placed(None, false)),
        ("off", placed(Some(PlacementLoad::Off), false)),
        ("for_test", placed(None, true)),
    ] {
        assert_eq!(counters.admitted, 0, "{name} admitted by load");
    }
    let on = placed(Some(PlacementLoad::On), false);
    assert_eq!(on.admitted, 1, "on did not place by load");
}
