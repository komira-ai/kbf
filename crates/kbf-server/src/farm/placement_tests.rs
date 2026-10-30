//! `--placement-load`: the load rule reaches the farm's scheduler, read back from what the
//! scheduler counts about a placement.

use std::sync::Arc;

use clap::Parser;
use kbf_front::{Cache, Submission};
use kbf_sched::LoadCounters;

use super::*;
use crate::{Args, PlacementLoad};

fn plain() -> Farm {
    Farm::new_legacy(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
}

/// The farm a server builds from `flags`, as `serve` and `frontend` both build it.
fn from_flags(flags: &[&str]) -> Farm {
    let args = Args::parse_from(std::iter::once("kbf-server").chain(flags.iter().copied()));
    let listeners = args.listeners().expect("listeners");
    Farm::from_listeners(Arc::new(Cache::memory()), &listeners)
}

/// Gives `farm` one node of four cores and one action, which the scheduler places at
/// once, and returns what the scheduler counted.
fn placed(farm: &Farm) -> LoadCounters {
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
            whole_worker: false,
        },
        call: None,
        described: kbf_front::Described::default(),
        evidence: Default::default(),
        max_wall: None,
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
    let test_core = Farm::for_test(
        Arc::new(Cache::memory()),
        testkit::TestCore::new(
            7,
            0,
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        ),
    );
    for (name, farm) in [
        ("Farm::new", plain()),
        ("off", plain().with_placement_load(PlacementLoad::Off)),
        ("for_test", test_core),
    ] {
        assert_eq!(placed(&farm).admitted, 0, "{name} admitted by load");
    }
    let on = plain().with_placement_load(PlacementLoad::On);
    assert_eq!(placed(&on).admitted, 1, "on did not place by load");
}

/// Catches `serve` and `frontend` building a farm that ignores the parsed flag: both take
/// their farm from `Farm::from_listeners`, so a flag that helper drops fails here. No flag
/// and `off` place by the booked rule; `on` admits the one action by load.
#[test]
fn the_farm_a_server_builds_takes_the_parsed_flag() {
    assert_eq!(placed(&from_flags(&[])).admitted, 0, "no flag");
    let off = from_flags(&["--placement-load=off"]);
    assert_eq!(placed(&off).admitted, 0, "off");
    let on = from_flags(&["--placement-load=on"]);
    assert_eq!(placed(&on).admitted, 1, "on");
}
