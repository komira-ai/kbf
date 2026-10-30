//! `--placement-load` on the js core: the rule reaches the core's scheduler, read back from
//! what it counts about a placement through the farm's public API (`Farm::load_counters`),
//! where `farm/placement_tests.rs` reads the legacy farm's scheduler through its lock.

use std::sync::Arc;

use clap::Parser;
use kbf_caps::NodeCaps;
use kbf_front::{Cache, Dispatch as _, Submission};
use kbf_proto::worker::ServerMessage;
use kbf_proto::worker::server_message::Message;
use kbf_sched::{DaemonInstance, FINISHED_RETENTION, LoadCounters, UNSERVABLE_WAIT};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::mpsc;

use crate::farm::{Farm, Reported};
use crate::{Args, PlacementLoad, SchedulerCore};

fn js_farm() -> Farm {
    Farm::on_core(
        SchedulerCore::Js,
        Arc::new(Cache::memory()).into(),
        UNSERVABLE_WAIT,
        FINISHED_RETENTION,
    )
}

/// The farm a server builds from `flags` on the js core, as `serve` and `frontend` both
/// build it.
fn from_flags(flags: &[&str]) -> Farm {
    let args = Args::parse_from(
        ["kbf-server", "--scheduler-core=js"]
            .into_iter()
            .chain(flags.iter().copied()),
    );
    let listeners = args.listeners().expect("listeners");
    let farm = Farm::from_listeners(Arc::new(Cache::memory()), &listeners);
    assert_eq!(farm.scheduler_core(), SchedulerCore::Js);
    farm
}

/// Gives `farm` one node of four cores and one action, which the core places at once (the
/// node is sent a `Start`), and returns what the scheduler counted.
fn placed(farm: &Farm) -> LoadCounters {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, mut inbox) = mpsc::unbounded_channel();
    farm.register(
        &WorkerId::new("node-a"),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(4_000, 8 << 30), caps),
        outbound,
        ServerMessage::default(),
    );
    farm.submit(Submission {
        request: kbf_sched::Request {
            key: ActionKey {
                instance: "main".to_owned(),
                action: Digest::new(DigestFunction::Sha256, [1; 32], 1),
            }
            .into(),
            qos: Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(1_000, 1 << 30),
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
    // Every message sent so far: the stream's first is the empty welcome given above.
    let mut started = false;
    while let Ok(sent) = inbox.try_recv() {
        started |= matches!(
            sent,
            Ok(ServerMessage {
                message: Some(Message::Start(_)),
            })
        );
    }
    assert!(started, "the action was not placed");
    farm.load_counters()
}

/// Catches a js farm placing by load without being told to (the library's default is to
/// place by load: a core built without kbf's configuration would admit by the load rule and
/// count `admitted`), an `on` the core never hears of (the core takes its configuration at
/// construction, so a builder that only stores the setting is lost), and an `off` that is
/// not on the booked rule.
#[tokio::test(start_paused = true)]
async fn a_js_farm_places_by_the_rule_its_flag_names() {
    assert_eq!(
        placed(&js_farm()).admitted,
        0,
        "the default admitted by load"
    );
    let off = js_farm().with_placement_load(PlacementLoad::Off);
    assert_eq!(placed(&off).admitted, 0, "off admitted by load");
    let on = js_farm().with_placement_load(PlacementLoad::On);
    assert_eq!(placed(&on).admitted, 1, "on did not place by load");
}

/// Catches `serve` and `frontend` building a js farm that ignores the parsed flag: both take
/// their farm from `Farm::from_listeners`. No flag and `off` place by the booked rule; `on`
/// admits the one action by load.
#[tokio::test(start_paused = true)]
async fn the_js_farm_a_server_builds_takes_the_parsed_flag() {
    assert_eq!(placed(&from_flags(&[])).admitted, 0, "no flag");
    let off = from_flags(&["--placement-load=off"]);
    assert_eq!(placed(&off).admitted, 0, "off");
    let on = from_flags(&["--placement-load=on"]);
    assert_eq!(placed(&on).admitted, 1, "on");
}
