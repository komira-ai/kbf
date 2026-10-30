//! Tests of learned sizes in shadow on the js core (`crate::farm_js::learning`), ported from
//! `tests/learned_sizes.rs`: the estimator is shown what happens, says what it would book,
//! and changes no booking.

use std::sync::Arc;

use kbf_front::metrics::{ShadowBucket, ShadowLevel};
use kbf_front::{Backend, Described, Dispatch as _, Submission};
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::server_message::Message;
use kbf_proto::worker::{self, CgroupUsage, MemoryKill, ResourceUsage, UsageBasis};
use kbf_sched::MemoryLadder;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources};

use super::hot::{Joined, backend, farm_on, join, killed};
use crate::SchedulerCore;
use crate::farm::durable::ESTIMATE;
use crate::farm::{EpochPolicy, Farm, Reported};
use crate::learned::LearnedSizes;
use crate::state_store::StateStore;
use crate::test_log::capture;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn shadow_farm(mode: LearnedSizes, cache: &Backend) -> Farm {
    farm_on(SchedulerCore::Js, cache)
        .with_memory_ladder(MemoryLadder::from_gib(&[4, 16, 32]).expect("a ladder"))
        .with_learned_sizes(mode, 4 * GIB)
}

/// A Linux node of 8 cores and 64 GiB.
fn big_node() -> Reported {
    let mut reported = super::hot::linux();
    reported.capacity = Resources::new(8_000, 64 * GIB);
    reported
}

/// The action `tag` runs `argv`, asking for 4 GiB on Linux.
fn submission(tag: u8, argv: &str) -> Submission {
    let key = ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
    };
    let request = kbf_sched::Request {
        key: key.into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 4 * GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("a platform"),
        whole_worker: false,
    };
    Submission {
        request,
        call: None,
        described: Described {
            arguments: vec![argv.to_owned()],
            ..Described::default()
        },
        evidence: Default::default(),
        max_wall: None,
    }
}

/// What a daemon reports for a lease that peaked at `peak`.
fn usage(peak: u64) -> ResourceUsage {
    ResourceUsage {
        wall_micros: 2_000_000,
        basis: UsageBasis::Cgroup as i32,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(1_000_000),
            memory_peak_bytes: Some(peak),
            ..CgroupUsage::default()
        }),
        ..ResourceUsage::default()
    }
}

/// The `Start` among the messages sent on the stream since the last call.
fn start_of(joined: &mut Joined) -> worker::Start {
    joined
        .sent()
        .into_iter()
        .find_map(|message| match message {
            Message::Start(start) => Some(start),
            _ => None,
        })
        .expect("a Start")
}

/// A `Result` that ran and exited 0, measured at `peak`.
fn measured_ok(lease: worker::LeaseId, peak: u64) -> worker::Result {
    worker::Result {
        lease_id: Some(lease),
        action_result: Some(ActionResult {
            exit_code: 0,
            execution_metadata: Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![usage(peak).to_any()],
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..worker::Result::default()
    }
}

/// Runs the action `tag` of `argv` to the end on `name`, which reports `peak`; returns what
/// the `Start` booked.
async fn run(farm: &Farm, name: &str, joined: &mut Joined, tag: u8, argv: &str, peak: u64) -> u64 {
    let _ticket = farm.submit(submission(tag, argv)).expect("queued");
    let start = start_of(joined);
    let lease = start.lease_id.expect("a lease");
    let ack = farm
        .report(&super::hot::node(name), measured_ok(lease, peak))
        .await
        .expect("an ack");
    assert!(ack.accepted, "{ack:?}");
    start.memory_bytes
}

fn estimates(text: &str, argv: &str) -> Vec<String> {
    let key = format!(
        "key={}",
        crate::action_usage::Shape::of(&[argv.to_owned()], &[]).key
    );
    text.lines()
        .filter(|l| l.contains("size estimate:") && l.split(' ').any(|w| w == key))
        .map(str::to_owned)
        .collect()
}

fn tally(cache: &Backend, level: ShadowLevel, bucket: ShadowBucket) -> u64 {
    cache.metrics().estimator_shadow.get(level, bucket)
}

/// Asserts `shadow` on the js core books the default for every operation, logs what the
/// estimator would book for each new operation, and counts the attempts in the buckets of
/// the estimate they were judged against. Catches: shadow booking the estimate (the later
/// `Start`s would show 1 GiB, not 4), no `size estimate:` line, no shape key in it, the
/// fallbacks of the first two operations unnamed, and a tally that does not follow the
/// buckets: two attempts with no estimate, four against one.
#[tokio::test(start_paused = true)]
async fn shadow_books_the_default_and_logs_what_it_would_book() {
    let log = capture();
    let cache = backend();
    let farm = shadow_farm(LearnedSizes::Shadow, &cache);
    let mut joined = join(&farm, "node-shadow", big_node());
    let argv = "compile shadow";
    for i in 0..6_u8 {
        let booked = run(&farm, "node-shadow", &mut joined, i + 1, argv, 100 * MIB).await;
        assert_eq!(booked, 4 * GIB, "run {i} books the default");
    }
    let lines = estimates(&log.text(), argv);
    assert_eq!(lines.len(), 6, "{lines:?}");
    let said = |i: usize, text: &str| assert!(lines[i].contains(text), "line {i}: {}", lines[i]);
    said(0, "fallback=Missing default_mib=4096");
    said(1, "fallback=Cold default_mib=4096");
    said(
        2,
        "level=l2 memory_mib=4096 cpu_millis=2000 default_mib=4096",
    );
    said(4, "level=l2 memory_mib=4096");
    said(
        5,
        "level=l2 memory_mib=1024 cpu_millis=2000 default_mib=4096 floor_mib=0",
    );
    // The farm publishes its counts on its tick.
    farm.tick().await;
    assert_eq!(tally(&cache, ShadowLevel::None, ShadowBucket::Cold), 2);
    assert_eq!(tally(&cache, ShadowLevel::L2, ShadowBucket::Covered), 4);
    assert_eq!(tally(&cache, ShadowLevel::L2, ShadowBucket::WouldOom), 0);
}

/// Asserts `off` on the js core calls the estimator never: no `size estimate:` line, no
/// count in any cell, the default booked. Its twin above shows the same runs do all three
/// in `shadow`.
#[tokio::test(start_paused = true)]
async fn off_calls_nothing() {
    let log = capture();
    let cache = backend();
    let farm = shadow_farm(LearnedSizes::Off, &cache);
    let mut joined = join(&farm, "node-off", big_node());
    let argv = "compile off";
    for i in 0..6_u8 {
        let booked = run(&farm, "node-off", &mut joined, i + 1, argv, 100 * MIB).await;
        assert_eq!(booked, 4 * GIB);
    }
    farm.tick().await;
    assert_eq!(estimates(&log.text(), argv), Vec::<String>::new());
    for level in [ShadowLevel::L1, ShadowLevel::L2, ShadowLevel::None] {
        for bucket in [
            ShadowBucket::Covered,
            ShadowBucket::WouldOom,
            ShadowBucket::Censored,
            ShadowBucket::Cold,
        ] {
            assert_eq!(tally(&cache, level, bucket), 0, "{level:?} {bucket:?}");
        }
    }
}

/// Asserts a caller that joined a twin is not counted as a new operation: two submits of one
/// action while it runs are one operation and get one line.
#[tokio::test(start_paused = true)]
async fn a_joined_twin_is_not_a_new_operation() {
    let log = capture();
    let farm = shadow_farm(LearnedSizes::Shadow, &backend());
    let mut joined = join(&farm, "node-twin", big_node());
    let argv = "compile twin";
    let _first = farm.submit(submission(1, argv)).expect("queued");
    let _second = farm.submit(submission(1, argv)).expect("joined");
    let lease = start_of(&mut joined).lease_id.expect("a lease");
    let ack = farm
        .report(
            &super::hot::node("node-twin"),
            measured_ok(lease, 100 * MIB),
        )
        .await
        .expect("an ack");
    assert!(ack.accepted);
    assert_eq!(estimates(&log.text(), argv).len(), 1);
}

/// A shadow farm over a store on a node of 4 GiB, whose ladder has the one rung of 4 GiB.
fn killed_farm(store: &Arc<Fake>) -> (Farm, Joined) {
    let farm = farm_on(SchedulerCore::Js, &backend())
        .with_memory_ladder(MemoryLadder::from_gib(&[4]).expect("a ladder"))
        .with_learned_sizes(LearnedSizes::Shadow, 4 * GIB)
        .with_state(Some(StateStore {
            store: Arc::clone(store) as Arc<dyn Store>,
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
    let mut reported = super::hot::linux();
    reported.capacity = Resources::new(4_000, 4 * GIB);
    let joined = join(&farm, "node-final", reported);
    (farm, joined)
}

fn store() -> Arc<Fake> {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    kbf_state::share(&*store).expect("the shared marker");
    store
}

fn estimate_rows(store: &Fake) -> usize {
    store.scan_prefix(ESTIMATE).expect("a scan").len()
}

/// Asserts an own-limit kill that ends the operation (the ladder is spent, no requeue
/// carries it) is written to the store at once, with no usage reported (the kill teaches the
/// estimator by its node's OS and the limit it died at), and that a farm started over the
/// same store takes the summary up again: the action is known to it (known, with the floor the kill left, not missing).
/// Catches: the kill left to the write-behind, so a crash in the next seconds loses it; no
/// restore on start; a store that is written with no store.
#[tokio::test(start_paused = true)]
async fn a_kill_that_ends_the_operation_is_written_at_once_and_a_restart_reads_it() {
    let log = capture();
    let store = store();
    let (farm, mut joined) = killed_farm(&store);
    let argv = "compile final kill";
    let ticket = farm.submit(submission(1, argv)).expect("queued");
    let lease = start_of(&mut joined).lease_id.expect("a lease");
    assert_eq!(estimate_rows(&store), 0);
    let ack = farm
        .report(
            &super::hot::node("node-final"),
            killed(&lease, MemoryKill::OwnLimit),
        )
        .await
        .expect("an ack");
    assert!(ack.accepted, "{ack:?}");
    assert!(
        matches!(&*ticket.stage.borrow(), kbf_front::Stage::Done(_)),
        "the operation ended"
    );
    assert!(estimate_rows(&store) > 0, "the kill is in the store");

    let (second, _joined) = killed_farm(&store);
    let _ticket = second.submit(submission(2, argv)).expect("queued");
    let lines = estimates(&log.text(), argv);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("fallback=Missing"), "{}", lines[0]);
    // The kill left a floor the 4 GiB node cannot meet: the action is known, with a floor.
    assert!(lines[1].contains("fallback=OverCap"), "{}", lines[1]);
    assert!(lines[1].contains("floor_mib=3840"), "{}", lines[1]);
}

/// Asserts `Farm::write_behind_now` on the js core writes the changed summaries to the state
/// store before it returns (the farm's own driver does), where the report alone writes none:
/// the case `tests/learned_sizes.rs` and `farm/learned_tests.rs` rely on. Catches: an arm that
/// writes nothing.
#[tokio::test(start_paused = true)]
async fn write_behind_now_writes_the_summaries_of_runs_to_the_store() {
    let store = store();
    let farm = shadow_farm(LearnedSizes::Shadow, &backend())
        .with_state(Some(StateStore {
            store: Arc::clone(&store) as Arc<dyn Store>,
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
    let mut joined = join(&farm, "node-now", big_node());
    for i in 0..3_u8 {
        run(
            &farm,
            "node-now",
            &mut joined,
            i + 1,
            "compile now",
            100 * MIB,
        )
        .await;
    }
    assert!(store.scan_prefix(ESTIMATE).expect("a scan").is_empty());
    farm.write_behind_now();
    assert!(!store.scan_prefix(ESTIMATE).expect("a scan").is_empty());
}
