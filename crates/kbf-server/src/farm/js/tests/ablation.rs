//! The ablation counters (`crate::farm_js::ablation`) through a js farm: each moves for its
//! own case and for no other. The cases a farm cannot be given here (a learned prediction, a
//! cold key) are checked on the counters alone in `farm_js/ablation_tests.rs`.

use kbf_front::{Described, Dispatch as _, Submission};
use kbf_proto::worker::MemoryKill;
use kbf_sched::MemoryLadder;

use super::hot::{Joined, backend, farm_on, join, killed, linux, node, request};
use crate::farm::Farm;
use crate::farm_js::ablation::{Ablation, Booking};
use crate::{PlacementLoad, SchedulerCore};
use kbf_types::Resources;

const GIB: u64 = 1 << 30;

const ALL: [Booking; 4] = [
    Booking::Prediction,
    Booking::Cold,
    Booking::Retry,
    Booking::Default,
];

/// A js farm over a ladder of 4, 16 and 32 GiB, with a node of 64 GiB joined.
fn farm(load: PlacementLoad) -> (Farm, Joined) {
    let farm = farm_on(SchedulerCore::Js, &backend())
        .with_placement_load(load)
        .with_memory_ladder(MemoryLadder::from_gib(&[4, 16, 32]).expect("a ladder"));
    let mut reported = linux();
    reported.capacity = Resources::new(8_000, 64 * GIB);
    let joined = join(&farm, "node-a", reported);
    (farm, joined)
}

/// The action `tag`, asking for the first rung of the ladder.
fn submission(tag: u8) -> Submission {
    let mut request = request(tag);
    request.resources = Resources::new(1_000, 4 * GIB);
    Submission {
        request,
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    }
}

/// The `Start` of the rerun after a kill: the scheduler places it at the next tick, so the
/// clock is moved on (paused, so no real time passes) until one is sent.
async fn rerun_start(farm: &Farm, joined: &mut Joined) {
    for _ in 0..30 {
        farm.tick().await;
        if joined.started().is_some() {
            return;
        }
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
    }
    panic!("no rerun was started");
}

/// The counts a farm has now.
fn counted(farm: &Farm) -> (Ablation, [u64; 3]) {
    let (ablation, counters) = farm.ablation().expect("the js core counts");
    (ablation, Ablation::placements(&counters))
}

/// Asserts the farm's own driver has no ablation counts, so its page is unchanged.
#[tokio::test(start_paused = true)]
async fn the_farms_own_driver_has_none() {
    let legacy = farm_on(SchedulerCore::Legacy, &backend());
    assert!(legacy.ablation().is_none());
    let mut page = String::new();
    crate::metrics::render_farm(&mut page, &legacy);
    assert!(!page.contains("kbf_ablation_"));
}

/// Asserts the js farm's page carries the families, so a scrape sees them with no further
/// wiring.
#[tokio::test(start_paused = true)]
async fn the_js_farms_page_carries_the_families() {
    let (farm, _joined) = farm(PlacementLoad::Off);
    let mut page = String::new();
    crate::metrics::render_farm(&mut page, &farm);
    for name in [
        "kbf_ablation_placements_total",
        "kbf_ablation_charges_total",
        "kbf_ablation_bookings_total",
        "kbf_ablation_oom_retries_total",
    ] {
        assert!(page.contains(&format!("# TYPE {name} counter")), "{name}");
    }
}

/// Asserts that with the load rule off an action placed is booked as the default and nothing
/// else moves: no placement case, no retry. Catches a placement case counted while the rule
/// is not read, and a default booking read as a prediction.
#[tokio::test(start_paused = true)]
async fn a_default_placement_with_load_off_moves_only_the_default_booking() {
    let (farm, mut joined) = farm(PlacementLoad::Off);
    let _ticket = farm.submit(submission(1)).expect("queued");
    assert!(joined.started().is_some(), "placed");
    let (ablation, placements) = counted(&farm);
    assert_eq!(placements, [0, 0, 0]);
    for booking in ALL {
        let want = u64::from(booking == Booking::Default);
        assert_eq!(ablation.bookings(booking), want, "{booking:?}");
    }
    assert_eq!(ablation.oom_retries(16), 0);
}

/// Asserts that with the load rule on, one placement is one admission by the rule: exactly
/// one of the by-load and fallback cases moves, by one, and refusals do not. The node has sent
/// no reading, so it is charged by prediction alone and the fallback is the case.
#[tokio::test(start_paused = true)]
async fn a_placement_with_load_on_moves_one_admission_case() {
    let (farm, mut joined) = farm(PlacementLoad::On);
    let _ticket = farm.submit(submission(1)).expect("queued");
    assert!(joined.started().is_some(), "placed");
    let (_, placements) = counted(&farm);
    assert_eq!(placements.iter().sum::<u64>(), 1, "{placements:?}");
    assert_eq!(placements[2], 0, "nothing was refused");
}

/// Asserts an own-limit kill is one retry at the next rung, whose rerun is booked as a retry
/// while the first run stays a default booking: the retry counts under 16 GiB and no other
/// rung.
#[tokio::test(start_paused = true)]
async fn an_own_limit_kill_counts_one_retry_at_its_rung() {
    let (farm, mut joined) = farm(PlacementLoad::Off);
    let _ticket = farm.submit(submission(1)).expect("queued");
    let lease = joined.started().expect("placed");
    let (ablation, _) = counted(&farm);
    assert_eq!(ablation.oom_retries(16), 0);
    assert_eq!(ablation.bookings(Booking::Default), 1);
    let ack = farm
        .report(&node("node-a"), killed(&lease, MemoryKill::OwnLimit))
        .await
        .expect("an ack");
    assert!(ack.accepted);
    rerun_start(&farm, &mut joined).await;
    let (ablation, _) = counted(&farm);
    for rung in [4, 32] {
        assert_eq!(ablation.oom_retries(rung), 0, "rung {rung}");
    }
    assert_eq!(ablation.oom_retries(16), 1);
    assert_eq!(ablation.bookings(Booking::Retry), 1);
    assert_eq!(ablation.bookings(Booking::Default), 1);
}
