//! The accept rule and the shown load of a node's readings: the rule itself over `Kept`, and
//! the farm's wiring of it (streams, sessions, the node view). The farm's clock is tokio's, and
//! these tests stop and advance it; none reads a real one.

use std::sync::Arc;
use std::time::Duration;

use kbf_proto::worker::{LeaseClock, LeaseId as ProtoLease, LeaseMinute, NodeLoad, ServerMessage};
use kbf_sched::DaemonInstance;
use kbf_types::{Resources, WorkerId};

use super::*;
use crate::PlacementLoad;
use crate::farm::phase::Reported;
use crate::fleet::{DroppedView, ListsView};

const GIB: u64 = 1 << 30;
const SECOND: u64 = 1_000_000;

fn node() -> Resources {
    Resources::new(4_000, 64 * GIB)
}

fn at(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis)
}

fn process(name: &str) -> Session {
    Session::Process(name.to_owned())
}

/// A reading `seq`, taken at `mono` microseconds, with `usage` microseconds of CPU used in all.
fn reading(seq: u64, mono: u64, usage: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: mono,
        cpu_usage_usec: usage,
        ..NodeLoad::default()
    }
}

/// What is kept after `next` is taken (or refused) as `session` at `now` on a node of `capacity`,
/// with `kept` before it.
fn take(
    kept: Option<Kept>,
    session: &Session,
    next: &NodeLoad,
    capacity: &Resources,
    now: u64,
) -> Option<Kept> {
    Kept::take(kept, session, next, capacity, at(now)).kept
}

fn newest_seq(kept: &Option<Kept>) -> Option<u64> {
    kept.as_ref().map(|k| k.last.seq)
}

/// Catches a first reading being measured against nothing (a rate from a baseline of zero),
/// and the measured CPU being something other than usage over time in thousandths of a CPU.
#[test]
fn a_first_reading_has_no_rate_and_a_second_has_one() {
    let s = process("d");
    let first = take(None, &s, &reading(1, 10 * SECOND, 5 * SECOND), &node(), 0);
    let first = first.expect("accepted");
    assert_eq!(first.measured_cpu_millicores(), None);
    assert!(!first.stale(at(u64::MAX)), "no window, so never stale");

    // 1.5 CPU-seconds in 1 second: 1500 millicores.
    let second = take(
        Some(first),
        &s,
        &reading(2, 11 * SECOND, 6_500_000),
        &node(),
        1_000,
    )
    .expect("accepted");
    assert_eq!(second.measured_cpu_millicores(), Some(1_500));
}

/// Catches the measured CPU being the newest window alone, the older alone, or an average:
/// it is the larger of the last two, and a window older than two falls out.
#[test]
fn measured_cpu_is_the_larger_of_the_last_two_windows() {
    let s = process("d");
    let mut kept = take(None, &s, &reading(1, 0, 0), &node(), 0);
    // Windows of 3000, 1000, 500 and 700 millicores, each a second long.
    let mut usage = 0;
    for (i, rate) in [3_000_u64, 1_000, 500, 700].into_iter().enumerate() {
        usage += rate * 1_000;
        let seq = i as u64 + 2;
        kept = take(
            kept,
            &s,
            &reading(seq, seq * SECOND - SECOND, usage),
            &node(),
            0,
        );
    }
    let measured = kept.as_ref().and_then(Kept::measured_cpu_millicores);
    assert_eq!(
        measured,
        Some(700),
        "3000 and 1000 are older than two windows"
    );

    // One more window of 100: the last two are 700 and 100.
    kept = take(
        kept,
        &s,
        &reading(6, 5 * SECOND, usage + 100_000),
        &node(),
        0,
    );
    assert_eq!(
        kept.as_ref().and_then(Kept::measured_cpu_millicores),
        Some(700)
    );
    kept = take(
        kept,
        &s,
        &reading(7, 6 * SECOND, usage + 200_000),
        &node(),
        0,
    );
    assert_eq!(
        kept.as_ref().and_then(Kept::measured_cpu_millicores),
        Some(100)
    );
}

/// Catches a reading whose seq is not above the last accepted being taken (a duplicated or
/// reordered heartbeat would move the baseline back), and a dropped one moving the baseline.
#[test]
fn a_seq_not_above_the_last_is_dropped() {
    let s = process("d");
    let kept = take(None, &s, &reading(5, SECOND, 0), &node(), 0);
    for seq in [5, 4, 1] {
        let again = take(
            kept.clone(),
            &s,
            &reading(seq, 9 * SECOND, 9 * SECOND),
            &node(),
            0,
        );
        assert_eq!(newest_seq(&again), Some(5), "seq {seq}");
        assert_eq!(again.expect("kept").last.mono_usec, SECOND, "seq {seq}");
    }
    let next = take(kept, &s, &reading(6, 2 * SECOND, SECOND), &node(), 0);
    assert_eq!(newest_seq(&next), Some(6));
}

/// Catches a restarted daemon (readings counted again from 1, its clock and counters from 0)
/// being dropped for a low seq or measured against the process before it.
#[test]
fn a_new_session_resets_the_baseline() {
    let kept = take(
        None,
        &process("old"),
        &reading(40, 900 * SECOND, 800 * SECOND),
        &node(),
        0,
    );
    let restarted = take(
        kept,
        &process("new"),
        &reading(1, 2 * SECOND, SECOND),
        &node(),
        0,
    )
    .expect("accepted");
    assert_eq!(restarted.last.seq, 1);
    assert_eq!(
        restarted.measured_cpu_millicores(),
        None,
        "no window across processes"
    );
    assert_eq!(restarted.windows, [None, None]);

    // A daemon that names no process is a new session on each stream.
    let a = Session::of("", StreamId(1));
    let b = Session::of("", StreamId(2));
    assert_ne!(a, b);
    assert_eq!(a, Session::of("", StreamId(1)));
    assert_eq!(Session::of("p", StreamId(1)), process("p"));
    assert_eq!(Session::of("p", StreamId(2)), process("p"));
}

/// Catches each validation failing to drop its reading, a dropped reading moving the baseline,
/// and a reading on the limit being refused.
#[test]
fn a_reading_that_fails_validation_counts_as_absent() {
    let s = process("d");
    let base = take(None, &s, &reading(1, 10 * SECOND, 10 * SECOND), &node(), 0);
    let cap = node();
    let too_much_memory = NodeLoad {
        memory_working_set_bytes: 4 * cap.memory_bytes + 1,
        ..reading(2, 11 * SECOND, 10 * SECOND)
    };
    let bad = [
        ("seq 0", reading(0, 11 * SECOND, 10 * SECOND)),
        ("cpu counter back", reading(2, 11 * SECOND, 10 * SECOND - 1)),
        ("clock back", reading(2, 9 * SECOND, 10 * SECOND)),
        ("clock still", reading(2, 10 * SECOND, 10 * SECOND)),
        // 16.001 cores in a second on 4 threads.
        (
            "more than 4 cores a thread",
            reading(2, 11 * SECOND, 26_001_000),
        ),
        ("memory above 4x", too_much_memory),
    ];
    for (why, bad) in bad {
        let after = take(base.clone(), &s, &bad, &cap, 0);
        assert_eq!(newest_seq(&after), Some(1), "{why}");
        assert_eq!(
            after.expect("kept").last.cpu_usage_usec,
            10 * SECOND,
            "{why}"
        );
    }
    // A first reading of a session is checked for what it can be checked for.
    let first_bad = NodeLoad {
        memory_working_set_bytes: u64::MAX,
        ..reading(1, SECOND, 0)
    };
    assert!(take(None, &s, &first_bad, &cap, 0).is_none());
    assert!(take(None, &s, &reading(0, SECOND, 0), &cap, 0).is_none());

    // The limits themselves are believed: 16.000 cores a second on 4 threads, 4x the memory.
    let at_limits = NodeLoad {
        memory_working_set_bytes: 4 * cap.memory_bytes,
        ..reading(2, 11 * SECOND, 26 * SECOND)
    };
    let taken = take(base, &s, &at_limits, &cap, 0).expect("accepted");
    assert_eq!(taken.measured_cpu_millicores(), Some(16_000));
}

/// Catches a node that reported no capacity having every window refused (4 times nothing), and
/// a counter that does not fit being wrapped: the rate saturates.
#[test]
fn a_node_with_no_capacity_skips_the_size_checks_and_a_huge_rate_saturates() {
    let s = process("d");
    let none = Resources::default();
    let base = take(None, &s, &reading(1, 0, 0), &none, 0);
    let big = NodeLoad {
        memory_working_set_bytes: u64::MAX,
        ..reading(2, 1, u64::MAX)
    };
    let kept = take(base, &s, &big, &none, 0).expect("accepted");
    assert_eq!(kept.measured_cpu_millicores(), Some(u64::MAX));
}

/// Catches staleness measured from the wrong end (a reading's own clock), a node stale at
/// exactly three windows, and a stale node still showing its measured CPU.
#[test]
fn readings_are_stale_after_three_windows() {
    let s = process("d");
    let first = take(None, &s, &reading(1, 0, 0), &node(), 0);
    // A window of 2 seconds, taken at farm time 10 000 ms.
    let kept = take(first, &s, &reading(2, 2 * SECOND, SECOND), &node(), 10_000).expect("kept");
    assert_eq!(
        kept.view(&node(), at(10_000)).measured_cpu_millicores,
        Some(500)
    );
    assert!(!kept.stale(at(16_000)), "three windows is not yet stale");
    assert!(kept.stale(at(16_001)));
    let view = kept.view(&node(), at(16_001));
    assert!(view.stale);
    assert_eq!(view.measured_cpu_millicores, None);
    assert!(!kept.view(&node(), at(16_000)).stale);
    // A clock that reads earlier than the reading was taken is not stale.
    assert!(!kept.stale(at(0)));
}

/// Catches the admission working set subtracting the active file pages whole, ignoring the
/// quarter of memory they are allowed, going below zero, or the view dropping a field.
#[test]
fn the_admission_working_set_drops_active_file_beyond_a_quarter_of_memory() {
    let s = process("d");
    let cap = Resources::new(8_000, 64 * GIB);
    let quarter = 16 * GIB;
    let view = |working_set: u64, active: u64| {
        let load = NodeLoad {
            memory_working_set_bytes: working_set,
            memory_active_file_bytes: active,
            actions_cpu_limited: true,
            cpu_throttled_usec: Some(3),
            cpu_pressure_some_total_usec: Some(4),
            covered: vec![ProtoLease { term: 2, seq: 7 }],
            ..reading(1, SECOND, 0)
        };
        take(None, &s, &load, &cap, 0)
            .expect("accepted")
            .view(&cap, at(0))
    };
    // Active file pages within a quarter of memory: nothing is subtracted.
    assert_eq!(
        view(30 * GIB, quarter).admission_working_set_bytes,
        30 * GIB
    );
    assert_eq!(view(30 * GIB, 0).admission_working_set_bytes, 30 * GIB);
    // 10 GiB over the quarter come off.
    assert_eq!(
        view(30 * GIB, quarter + 10 * GIB).admission_working_set_bytes,
        20 * GIB
    );
    // More than the working set: floored at zero.
    assert_eq!(
        view(5 * GIB, quarter + 10 * GIB).admission_working_set_bytes,
        0
    );
    // The rest of the view is the reading's.
    let v = view(30 * GIB, quarter + 10 * GIB);
    assert_eq!(v.memory_working_set_bytes, 30 * GIB);
    assert_eq!(v.memory_active_file_bytes, quarter + 10 * GIB);
    assert_eq!(v.covered, ["2.7"]);
    assert!(v.actions_cpu_limited && !v.stale);
    assert_eq!(
        (v.cpu_throttled_usec, v.cpu_pressure_some_total_usec),
        (Some(3), Some(4))
    );
    assert_eq!(v.measured_cpu_millicores, None);
    // A node that reported no memory subtracts all of the active file pages.
    let none = Resources::default();
    let load = NodeLoad {
        memory_working_set_bytes: 100,
        memory_active_file_bytes: 30,
        ..reading(1, SECOND, 0)
    };
    let v = take(None, &s, &load, &none, 0)
        .expect("accepted")
        .view(&none, at(0));
    assert_eq!(v.admission_working_set_bytes, 70);
}

// The farm's wiring.

/// A farm on the core under test: the farm's own, and the js core in a build with
/// `farm-core-js`.
fn farm() -> Farm {
    Farm::new(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
}

fn serving() -> Reported {
    let report = [("arch", "x86_64"), ("os", "linux")];
    let caps = kbf_caps::NodeCaps::from_report(report).expect("caps");
    Reported::serving(
        Resources::new(4_000, 64 * GIB),
        caps.with_drivers(["container"]),
    )
}

/// `name` registered by daemon process `instance` on a new stream, kept open for the rest of
/// the test.
fn register(farm: &Farm, name: &str, instance: &str) -> StreamId {
    let (outbound, inbox) = tokio::sync::mpsc::unbounded_channel();
    std::mem::forget(inbox);
    farm.register(
        &WorkerId::new(name),
        DaemonInstance::new(instance),
        DaemonInstance::new(""),
        serving(),
        outbound,
        ServerMessage::default(),
    )
}

fn detail(farm: &Farm, name: &str) -> crate::fleet::NodeDetail {
    farm.node_detail(&WorkerId::new(name))
        .expect("readable")
        .expect("known")
}

/// Asserts the wiring: a node with no accepted reading shows no load, readings of a heartbeat
/// reach the node view, a heartbeat without one changes nothing, a reading on a replaced stream
/// or of a node that never registered is ignored, the same daemon process on a new stream
/// keeps its baseline, a restarted one resets it, and the view goes stale on the farm's clock.
#[tokio::test(start_paused = true)]
async fn readings_reach_the_node_view() {
    let farm = farm();
    let a = WorkerId::new("a");
    let first = register(&farm, "a", "proc-1");
    assert_eq!(detail(&farm, "a").load, None);

    farm.note_node_load(&a, first, Some(&reading(1, SECOND, 0)));
    farm.note_node_load(&a, first, Some(&reading(2, 2 * SECOND, 2 * SECOND)));
    farm.note_node_load(&a, first, None);
    let load = detail(&farm, "a").load.expect("a load");
    assert_eq!(load.measured_cpu_millicores, Some(2_000));
    assert!(!load.stale);

    // The same process on a new stream keeps its baseline: seq 2 is not above 2.
    let second = register(&farm, "a", "proc-1");
    farm.note_node_load(&a, first, Some(&reading(9, 9 * SECOND, 9 * SECOND)));
    farm.note_node_load(&a, second, Some(&reading(2, 3 * SECOND, 3 * SECOND)));
    assert_eq!(farm.kept_load(&a).expect("kept").last.seq, 2);
    farm.note_node_load(&a, second, Some(&reading(3, 2 * SECOND, 3 * SECOND)));
    assert_eq!(
        farm.kept_load(&a).expect("kept").last.seq,
        2,
        "the clock did not move"
    );
    farm.note_node_load(&a, second, Some(&reading(3, 4 * SECOND, 3 * SECOND)));
    assert_eq!(
        detail(&farm, "a")
            .load
            .expect("load")
            .measured_cpu_millicores,
        Some(2_000)
    );

    // A restarted daemon counts again from 1.
    let third = register(&farm, "a", "proc-2");
    farm.note_node_load(&a, third, Some(&reading(1, SECOND / 2, 0)));
    let load = detail(&farm, "a").load.expect("a load");
    assert_eq!(load.measured_cpu_millicores, None);

    // A node that never registered keeps nothing.
    farm.note_node_load(
        &WorkerId::new("ghost"),
        StreamId(0),
        Some(&reading(1, 1, 1)),
    );
    assert!(farm.kept_load(&WorkerId::new("ghost")).is_none());

    // Two readings a second apart, then no heartbeat for more than three seconds.
    farm.note_node_load(&a, third, Some(&reading(2, SECOND / 2 + SECOND, SECOND)));
    assert!(!detail(&farm, "a").load.expect("load").stale);
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(!detail(&farm, "a").load.expect("load").stale);
    tokio::time::advance(Duration::from_millis(1)).await;
    let stale = detail(&farm, "a").load.expect("load");
    assert!(stale.stale);
    assert_eq!(stale.measured_cpu_millicores, None);
}

// The lists of a reading, and the feed to the scheduler.

fn lease(seq: u64) -> ProtoLease {
    ProtoLease { term: 1, seq }
}

fn clock(seq: u64, started_mono_usec: u64) -> LeaseClock {
    LeaseClock {
        lease: Some(lease(seq)),
        started_mono_usec,
    }
}

fn minute(seq: u64, minute: u32) -> LeaseMinute {
    LeaseMinute {
        lease: Some(lease(seq)),
        minute,
        cpu_mean_millis: 100,
        cpu_peak_window_millis: 200,
        ws_max_bytes: 1 << 20,
    }
}

/// `reading(seq, mono, 0)` carrying `minutes`.
fn with_minutes(seq: u64, mono: u64, minutes: Vec<LeaseMinute>) -> NodeLoad {
    NodeLoad {
        minutes,
        ..reading(seq, mono, 0)
    }
}

fn counts(
    (clocks, minutes): (u64, u64),
    (invalid, duplicate, over_cap): (u64, u64, u64),
    refused_minutes: u64,
) -> ListsView {
    ListsView {
        clocks,
        minutes,
        dropped: DroppedView {
            invalid,
            duplicate,
            over_cap,
        },
        refused_minutes,
    }
}

/// Asserts what the accept rule does with the lists of a reading: the records that pass the
/// vet are returned and counted, each reason of a drop on its own count, the counts add up
/// over the readings of a session, the minutes of a refused reading are counted as refused
/// and nothing else, a refused first reading counts nowhere, and a new session starts the
/// counts again. The kept `covered` is the vetted one: at most 96 leases, each once.
#[test]
fn the_lists_of_an_accepted_reading_are_vetted_and_counted() {
    let s = process("d");
    let broken = LeaseMinute {
        cpu_mean_millis: 300,
        ..minute(7, 1)
    };
    let load = NodeLoad {
        covered: (0..100).map(lease).collect(),
        // The second is a repeat of the first; the third starts after the reading.
        lease_clocks: vec![
            clock(1, SECOND),
            clock(1, 2 * SECOND),
            clock(2, 11 * SECOND),
        ],
        // The second has a mean above its peak; the third is a repeat of the first.
        minutes: vec![minute(7, 0), broken, minute(7, 0)],
        ..reading(1, 10 * SECOND, 0)
    };
    let taken = Kept::take(None, &s, &load, &node(), at(0));
    let vetted = taken.vetted.expect("accepted");
    assert_eq!(vetted.minutes, [minute(7, 0)]);
    assert_eq!(vetted.clocks, [clock(1, SECOND)]);
    let kept = taken.kept.expect("kept");
    assert_eq!(kept.last.covered.len(), 96);
    assert_eq!(kept.lists.view(), counts((1, 1), (2, 2, 4), 0));

    // The next accepted reading adds to the counts.
    let next = with_minutes(2, 11 * SECOND, vec![minute(7, 1), minute(7, 2)]);
    let kept = take(Some(kept), &s, &next, &node(), 0).expect("kept");
    assert_eq!(kept.lists.view(), counts((1, 3), (2, 2, 4), 0));

    // A refused reading counts its minutes as refused, and changes nothing else.
    let again = with_minutes(2, 12 * SECOND, vec![minute(7, 3); 5]);
    let taken = Kept::take(Some(kept), &s, &again, &node(), at(0));
    assert!(taken.vetted.is_none());
    let kept = taken.kept.expect("kept");
    assert_eq!(kept.last.seq, 2);
    assert_eq!(kept.lists.view(), counts((1, 3), (2, 2, 4), 5));

    // A reading of a new session starts the counts again, and a refused first one has no
    // state to count in.
    let fresh = with_minutes(1, SECOND, vec![minute(7, 0)]);
    let kept = take(Some(kept), &process("new"), &fresh, &node(), 0).expect("kept");
    assert_eq!(kept.lists.view(), counts((0, 1), (0, 0, 0), 0));
    let refused = with_minutes(0, SECOND, vec![minute(7, 0)]);
    let taken = Kept::take(None, &s, &refused, &node(), at(0));
    assert!(taken.kept.is_none());
    assert!(taken.vetted.is_none());
}

/// A heartbeat carrying `load`, as the worker session handles one: the accepted reading goes to
/// the scheduler with the heartbeat, and a refused one leaves it with none.
async fn beat(farm: &Farm, worker: &WorkerId, stream: StreamId, seq: u64, load: &NodeLoad) -> bool {
    let fed = farm.note_node_load(worker, stream, Some(load));
    farm.heartbeat_with_load(worker, stream, seq, Vec::new(), fed)
        .await
}

fn accepted(farm: &Farm) -> u64 {
    farm.load_counters().readings_accepted
}

/// Asserts that an accepted reading comes back from `note_node_load` as the load the scheduler
/// takes, with the lists that passed the vet and the ids of the scheduler's own type, that it
/// is the heartbeat carrying it that the scheduler hears (the scheduler counts it as accepted
/// then and not before), and that a heartbeat with no reading feeds it nothing.
#[tokio::test(start_paused = true)]
async fn an_accepted_reading_is_handed_to_the_scheduler_with_its_lists() {
    let farm = farm();
    let a = WorkerId::new("a");
    let stream = register(&farm, "a", "proc");
    let load = NodeLoad {
        covered: vec![lease(7)],
        lease_clocks: vec![clock(7, 5 * SECOND)],
        minutes: vec![minute(7, 0)],
        ..reading(1, 10 * SECOND, 0)
    };
    let fed = farm.note_node_load(&a, stream, Some(&load)).expect("fed");
    assert_eq!(fed.seq, 1);
    assert_eq!(fed.covered, [LeaseId::new(1, 7)]);
    assert_eq!(fed.lease_clocks.len(), 1);
    assert_eq!(fed.minutes.len(), 1);
    assert_eq!(accepted(&farm), 0, "noting a reading is not feeding it");
    assert!(
        farm.heartbeat_with_load(&a, stream, 1, Vec::new(), Some(fed))
            .await
    );
    assert_eq!(accepted(&farm), 1);

    assert!(beat(&farm, &a, stream, 2, &reading(2, 11 * SECOND, SECOND)).await);
    assert_eq!(accepted(&farm), 2);
    assert!(farm.heartbeat(&a, stream, 3, Vec::new()).await);
    assert_eq!(accepted(&farm), 2, "a heartbeat with no reading feeds none");
}

/// Asserts that a reading the accept rule refuses (a repeat of the last seq, an older one, a
/// clock that went back) is not handed to the scheduler, and that the minutes it carried are
/// counted as refused in the node view; and that the refused first reading of a restarted
/// daemon feeds nothing either and leaves the node without a load.
#[tokio::test(start_paused = true)]
async fn a_refused_reading_feeds_nothing_and_counts_its_minutes() {
    let farm = farm();
    let a = WorkerId::new("a");
    let stream = register(&farm, "a", "proc");
    let first = with_minutes(5, 10 * SECOND, vec![minute(7, 0)]);
    assert!(beat(&farm, &a, stream, 1, &first).await);
    assert_eq!(accepted(&farm), 1);

    let three = |seq, mono| with_minutes(seq, mono, vec![minute(7, 1), minute(7, 2), minute(7, 3)]);
    let refused = [
        ("a repeat of the last seq", three(5, 11 * SECOND)),
        ("an older seq", three(4, 11 * SECOND)),
        ("a clock that went back", three(6, 9 * SECOND)),
    ];
    for (n, (why, load)) in refused.iter().enumerate() {
        let fed = farm.note_node_load(&a, stream, Some(load));
        assert!(fed.is_none(), "{why}");
        let seq = 2 + n as u64;
        let acked = farm
            .heartbeat_with_load(&a, stream, seq, Vec::new(), fed)
            .await;
        assert!(acked, "{why}");
    }
    assert_eq!(
        accepted(&farm),
        1,
        "no refused reading reached the scheduler"
    );
    let lists = detail(&farm, "a").load.expect("a load").lists;
    assert_eq!(lists, counts((0, 1), (0, 0, 0), 9));

    // The first reading of a restarted daemon that is refused has no counts to add to: it
    // feeds nothing, and the node has no load to show until one is accepted.
    let restarted = register(&farm, "a", "proc-2");
    let bad = with_minutes(0, SECOND, vec![minute(7, 0)]);
    assert!(farm.note_node_load(&a, restarted, Some(&bad)).is_none());
    assert_eq!(detail(&farm, "a").load, None);
}

/// Asserts that a heartbeat of a replaced stream feeds the scheduler nothing, even when it
/// carries the load a reading came to: it is not taken (not acknowledged) and the scheduler's
/// count of accepted readings does not move; the same load on the stream that replaced it is
/// fed.
#[tokio::test(start_paused = true)]
async fn a_heartbeat_of_a_replaced_stream_feeds_nothing() {
    let farm = farm();
    let a = WorkerId::new("a");
    let old = register(&farm, "a", "proc-1");
    let fed = farm
        .note_node_load(&a, old, Some(&reading(1, SECOND, 0)))
        .expect("fed");
    let current = register(&farm, "a", "proc-2");

    let taken = farm
        .heartbeat_with_load(&a, old, 1, Vec::new(), Some(fed))
        .await;
    assert!(!taken, "a heartbeat of a replaced stream was taken");
    assert_eq!(accepted(&farm), 0, "a replaced stream fed the scheduler");

    assert!(beat(&farm, &a, current, 1, &reading(1, SECOND, 0)).await);
    assert_eq!(accepted(&farm), 1);
}

/// Asserts that the scheduler is handed accepted readings under `--placement-load off` and
/// under `on`: it keeps them either way, so the feed is warm before the flag is raised.
#[tokio::test(start_paused = true)]
async fn the_feed_reaches_the_scheduler_in_every_placement_mode() {
    for mode in [PlacementLoad::Off, PlacementLoad::On] {
        let farm = farm().with_placement_load(mode);
        let a = WorkerId::new("a");
        let stream = register(&farm, "a", "proc");
        assert!(beat(&farm, &a, stream, 1, &reading(1, SECOND, 0)).await);
        assert!(beat(&farm, &a, stream, 2, &reading(2, 2 * SECOND, SECOND)).await);
        assert_eq!(accepted(&farm), 2, "{mode:?}");
    }
}

/// Asserts that a restarted daemon whose first reading fails validation leaves the node with
/// no load: the earlier process's reading is not shown for it, and a first reading that
/// cannot be believed is not kept as a baseline. Catches the old session's reading kept
/// after a restart, and a refused first reading stored anyway.
#[tokio::test(start_paused = true)]
async fn a_restarted_daemons_unbelievable_first_reading_leaves_no_load() {
    let farm = farm();
    let a = WorkerId::new("a");
    let first = register(&farm, "a", "proc-1");
    farm.note_node_load(&a, first, Some(&reading(1, SECOND, 0)));
    farm.note_node_load(&a, first, Some(&reading(2, 2 * SECOND, 2 * SECOND)));
    assert!(detail(&farm, "a").load.is_some());

    let restarted = register(&farm, "a", "proc-2");
    farm.note_node_load(&a, restarted, Some(&reading(0, SECOND, 0)));
    assert_eq!(detail(&farm, "a").load, None);
    assert!(farm.kept_load(&a).is_none());

    // Its next believable reading is a first one again: no rate yet.
    farm.note_node_load(&a, restarted, Some(&reading(1, 2 * SECOND, SECOND)));
    let load = detail(&farm, "a").load.expect("a load");
    assert_eq!(load.measured_cpu_millicores, None);
}
