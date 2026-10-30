//! `--learned-sizes` through a running server: a daemon runs and reports actions, and the
//! test reads what the farm logged (`size estimate:`), what it counted
//! (`kbf_estimator_shadow_total`), what it booked (the `Start`) and what it wrote to the
//! state store. The log is read through a global subscriber, so each test picks its lines
//! out by the shape key of its own actions.

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use kbf_front::metrics::{ShadowBucket, ShadowLevel};
use kbf_proto::reapi::{Action, Command, Directory, FileNode, Platform, platform};
use kbf_proto::worker::{self, CgroupUsage, MemoryKill, ResourceUsage, UsageBasis};
use kbf_server::action_usage::{Shape, ShapeKey};
use kbf_server::{EpochPolicy, LearnedSizes, MemoryLadder, StateStore};
use kbf_state::Store;
use support::{Blob, Cell, FakeDaemon, Job, done, output, ran};
use tonic::Code;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The binary's log, its subscriber installed by the first test to ask.
fn log() -> &'static Log {
    static LOG: OnceLock<Log> = OnceLock::new();
    let log = LOG.get_or_init(|| {
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");
        log
    });
    // A callsite another test's thread reached while the subscriber was being installed
    // may have cached "never" (tests/thread_log_capture.rs).
    tracing_core::callsite::rebuild_interest_cache();
    log
}

/// The `size estimate:` lines of the actions whose argv is `argv`, oldest first.
fn estimates(argv: &str) -> Vec<String> {
    let key = format!("key={}", shape(argv));
    let text = String::from_utf8(log().0.lock().expect("log").clone()).expect("UTF-8");
    text.lines()
        .filter(|l| l.contains("size estimate:") && l.split(' ').any(|w| w == key))
        .map(str::to_owned)
        .collect()
}

fn shape(argv: &str) -> ShapeKey {
    Shape::of(&[argv.to_owned()], &[]).key
}

/// A job that runs `argv` on `platform`; `tag` makes its input, and so its digest, its
/// own. Every job of one `argv` has one shape.
fn job(argv: &str, tag: &str, platform: &[(&str, &str)]) -> Job {
    let input = Blob::new(format!("input of {argv} {tag}"));
    let root = Blob::of(&Directory {
        files: vec![FileNode {
            name: "in".to_owned(),
            digest: Some(input.proto.clone()),
            ..Default::default()
        }],
        ..Default::default()
    });
    let command = Blob::of(&Command {
        arguments: vec![argv.to_owned()],
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(root.proto.clone()),
        platform: Some(Platform {
            properties: platform
                .iter()
                .map(|(name, value)| platform::Property {
                    name: (*name).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
        }),
        ..Default::default()
    });
    Job {
        action,
        command,
        root,
        input,
    }
}

const LINUX: &[(&str, &str)] = &[("OSFamily", "linux")];

fn ladder() -> MemoryLadder {
    MemoryLadder::from_gib(&[4, 16, 32]).expect("a ladder")
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

fn with_usage(
    mut result: kbf_proto::reapi::ActionResult,
    usage: &ResourceUsage,
) -> kbf_proto::reapi::ActionResult {
    result.execution_metadata = Some(kbf_proto::reapi::ExecutedActionMetadata {
        auxiliary_metadata: vec![usage.to_any()],
        ..Default::default()
    });
    result
}

/// Runs `job` to the end on `daemon`, which reports `peak`; returns what the `Start`
/// booked.
async fn run(cell: &Cell, daemon: &mut FakeDaemon, job: &Job, peak: u64) -> u64 {
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let result = with_usage(output(cell, &format!("out {peak}"), 0).await, &usage(peak));
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;
    start.memory_bytes
}

fn tally(cell: &Cell, level: ShadowLevel, bucket: ShadowBucket) -> u64 {
    cell.cache.metrics().estimator_shadow.get(level, bucket)
}

/// Catches: `shadow` booking the estimate (the
/// first six `Start`s would show 1 GiB, not 4), `shadow` not logging it, the estimate at
/// five samples not being the 1 GiB the quiet runs earn and different from the 4 GiB
/// default the farm books (so that applying it would show), the fallback of the first two
/// operations not named, a log line without its shape key, and the tally not following
/// the buckets: two attempts with no estimate, four against one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_books_the_default_and_logs_what_it_would_book() {
    log();
    let cell = Cell::start_learned(LearnedSizes::Shadow, ladder(), None).await;
    let mut daemon = cell.daemon("node-shadow", 8, 64).await;
    let argv = "compile shadow";
    for i in 0..6 {
        let booked = run(
            &cell,
            &mut daemon,
            &job(argv, &i.to_string(), LINUX),
            100 * MIB,
        )
        .await;
        assert_eq!(booked, 4 * GIB, "run {i} books the default");
    }
    let lines = estimates(argv);
    assert_eq!(lines.len(), 6, "{lines:?}");
    let said = |i: usize, text: &str| assert!(lines[i].contains(text), "line {i}: {}", lines[i]);
    said(0, "fallback=Missing default_mib=4096");
    said(1, "fallback=Cold default_mib=4096");
    // From the third the estimator answers with the booking itself (it lowers from five).
    // These runs read no CPU, so the CPU is the estimator's cold cost, two cores.
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
    cell.tick().await;
    assert_eq!(tally(&cell, ShadowLevel::None, ShadowBucket::Cold), 2);
    assert_eq!(tally(&cell, ShadowLevel::L2, ShadowBucket::Covered), 4);
    assert_eq!(tally(&cell, ShadowLevel::L2, ShadowBucket::WouldOom), 0);
}

/// Catches: `off` calling the estimator: a `size estimate:` line, a
/// count in any of the twelve cells, or a booking other than the default. Its twin above
/// shows the same runs do all three in `shadow`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_calls_nothing() {
    log();
    let cell = Cell::start_learned(LearnedSizes::Off, ladder(), None).await;
    let mut daemon = cell.daemon("node-off", 8, 64).await;
    let argv = "compile off";
    for i in 0..6 {
        let booked = run(
            &cell,
            &mut daemon,
            &job(argv, &i.to_string(), LINUX),
            100 * MIB,
        )
        .await;
        assert_eq!(booked, 4 * GIB);
    }
    // A tick of the farm after the last result publishes what it counted.
    cell.tick().await;
    assert_eq!(estimates(argv), Vec::<String>::new());
    for level in [ShadowLevel::L1, ShadowLevel::L2, ShadowLevel::None] {
        for bucket in [
            ShadowBucket::Covered,
            ShadowBucket::WouldOom,
            ShadowBucket::Censored,
            ShadowBucket::Cold,
        ] {
            assert_eq!(tally(&cell, level, bucket), 0, "{level:?} {bucket:?}");
        }
    }
}

/// Catches: a caller that joined a twin counted as a new operation: two Executes of
/// one action while it runs are one operation and get one line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_joined_twin_is_not_a_new_operation() {
    log();
    let cell = Cell::start_learned(LearnedSizes::Shadow, ladder(), None).await;
    let mut daemon = cell.daemon("node-twin", 8, 64).await;
    let argv = "compile twin";
    let twin = job(argv, "one", LINUX);
    cell.upload(&twin.blobs()).await;
    let mut first = cell.execute(&twin.action).await;
    let mut second = cell.execute(&twin.action).await;
    let start = daemon.start().await;
    let result = with_usage(output(&cell, "twin", 0).await, &usage(100 * MIB));
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut first).await;
    done(&mut second).await;
    assert_eq!(estimates(argv).len(), 1);
}

/// Catches: an estimate asked for an action whose `needs` do not pin the OS, or that
/// asks for a GPU, and skips that are not counted. A Linux action with no GPU in the same
/// cell is the control: it is asked about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_actions_that_pin_the_os_and_ask_no_gpu_are_asked_about() {
    log();
    let cell = Cell::start_learned(LearnedSizes::Shadow, ladder(), None).await;
    let skipped = |reason: &str| {
        let at = kbf_front::metrics::SHADOW_SKIPS
            .iter()
            .position(|r| *r == reason)
            .expect("a reason");
        cell.cache.metrics().estimator_shadow.skipped(at)
    };
    let submit = |argv: &'static str, platform: &'static [(&'static str, &'static str)]| {
        let cell = &cell;
        async move {
            let job = job(argv, "a", platform);
            cell.upload(&job.blobs()).await;
            cell.execute(&job.action).await
        }
    };

    let _no_os = submit("compile no os", &[]).await;
    let _gpu = submit("compile gpu", &[("OSFamily", "linux"), ("gpu", "1")]).await;
    let _control = submit("compile control", LINUX).await;
    assert_eq!(estimates("compile control").len(), 1);
    cell.tick().await;
    assert_eq!(skipped("no_os"), 1);
    assert_eq!(skipped("kind"), 1);
    assert_eq!(estimates("compile no os"), Vec::<String>::new());
    assert_eq!(estimates("compile gpu"), Vec::<String>::new());
    // Nothing of the three has a node: the control is asked about with no bound.
    assert!(estimates("compile control")[0].contains("fallback=Missing"));
}

/// Catches: a Mac action asked about with the bounds of the Linux nodes (or with none at
/// all once a Mac node is there), and a request for an OS no node has registered failing
/// instead of being asked about without a bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mac_action_is_asked_about_with_or_without_a_mac_node() {
    log();
    let cell = Cell::start_learned(LearnedSizes::Shadow, ladder(), None).await;
    let argv = "compile for the mac";
    let mac = [("OSFamily", "macos")];
    let _linux_node = cell.daemon("node-linux", 4, 8).await;
    let first = job(argv, "a", &mac);
    cell.upload(&first.blobs()).await;
    let _first = cell.execute(&first.action).await;
    assert_eq!(estimates(argv).len(), 1, "asked with no Mac node");

    let hello = support::hello_on("node-mac", 4, 8, &[("os", "macos"), ("arch", "arm64")]);
    let _mac_node = FakeDaemon::connect(cell.worker_addr, hello)
        .await
        .expect("registered");
    let second = job(argv, "b", &mac);
    cell.upload(&second.blobs()).await;
    let _second = cell.execute(&second.action).await;
    assert_eq!(estimates(argv).len(), 2, "asked with a Mac node");
}

/// Catches: a size the client named not passed as a hint (the line would say 1 GiB
/// for an action that asked for 8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_size_the_client_named_is_a_hint() {
    log();
    let cell = Cell::start_learned(LearnedSizes::Shadow, ladder(), None).await;
    let mut daemon = cell.daemon("node-hint", 8, 64).await;
    // Five quiet runs, then one that names its memory.
    let argv = "compile hinted";
    for i in 0..5 {
        run(
            &cell,
            &mut daemon,
            &job(argv, &i.to_string(), LINUX),
            100 * MIB,
        )
        .await;
    }
    let named = job(
        argv,
        "named",
        &[("OSFamily", "linux"), ("kbf-book-mem-gib", "8")],
    );
    let booked = run(&cell, &mut daemon, &named, 100 * MIB).await;
    assert_eq!(booked, 8 * GIB, "the booking is the client's");
    let lines = estimates(argv);
    assert!(
        lines[5].contains(" level=l2 memory_mib=8192 "),
        "{}",
        lines[5]
    );
}

/// Catches: attempts learned in the order the operations were submitted rather
/// than the order their results commit. Two operations of one shape are in flight; the
/// one submitted second reports first. Learned in commit order the first report is the
/// fifth sample and the second is judged against the 1 GiB the five earn (its 1.95 GiB is
/// within 5% of that estimate's 2 GiB limit: censored); learned in submit order it would
/// be judged against the 4 GiB default (covered).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn results_are_learned_in_the_order_they_commit() {
    log();
    let cell = Cell::start_learned(LearnedSizes::Shadow, ladder(), None).await;
    let mut daemon = cell.daemon("node-order", 8, 64).await;
    let argv = "compile order";
    for i in 0..4 {
        run(
            &cell,
            &mut daemon,
            &job(argv, &i.to_string(), LINUX),
            100 * MIB,
        )
        .await;
    }
    let (slow, quick) = (job(argv, "slow", LINUX), job(argv, "quick", LINUX));
    cell.upload(&slow.blobs()).await;
    cell.upload(&quick.blobs()).await;
    let mut slow_ops = cell.execute(&slow.action).await;
    let mut quick_ops = cell.execute(&quick.action).await;
    let (first, second) = (daemon.start().await, daemon.start().await);
    // The lease that started second belongs to `quick`; report it first.
    let quick_result = with_usage(output(&cell, "quick", 0).await, &usage(100 * MIB));
    let slow_result = with_usage(output(&cell, "slow", 0).await, &usage(GIB + 950 * MIB));
    assert!(
        daemon
            .report(ran(second.lease_id, &quick_result))
            .await
            .accepted
    );
    assert!(
        daemon
            .report(ran(first.lease_id, &slow_result))
            .await
            .accepted
    );
    done(&mut slow_ops).await;
    done(&mut quick_ops).await;
    cell.tick().await;
    assert_eq!(tally(&cell, ShadowLevel::L2, ShadowBucket::Censored), 1);
    assert_eq!(tally(&cell, ShadowLevel::None, ShadowBucket::Cold), 2);
}

fn state() -> (Arc<kbf_state::fake::Fake>, StateStore) {
    let store = Arc::new(kbf_state::fake::Fake::with_flaw(
        Arc::new(kbf_state::ManualClock::default()),
        kbf_state::fake::Flaw::None,
    ));
    let state = StateStore {
        store: store.clone(),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    };
    (store, state)
}

fn killed(lease: Option<worker::LeaseId>, peak: u64) -> worker::Result {
    let mut result = worker::Result {
        lease_id: lease,
        status: Some(kbf_proto::google::rpc::Status {
            code: Code::ResourceExhausted as i32,
            message: "memory".to_owned(),
            details: Vec::new(),
        }),
        action_result: Some(with_usage(Default::default(), &usage(peak))),
        ..worker::Result::default()
    };
    result.memory_kill = MemoryKill::OwnLimit as i32;
    result
}

fn rows(store: &kbf_state::fake::Fake) -> Vec<(String, Vec<u8>)> {
    store
        .scan_prefix("est/")
        .expect("scan")
        .into_iter()
        .map(|(key, row)| (key, row.value))
        .collect()
}

/// Catches: the floor an own-limit kill sets not written when its requeue is (it is in the
/// store by the time the daemon is acknowledged, before the write-behind is due), a
/// floor below the booking the retry runs at, and `off` writing a summary at all. A kill
/// at 4 GiB retries at 16.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kills_floor_is_in_the_store_with_its_requeue_and_off_writes_nothing() {
    log();
    for (mode, wrote) in [(LearnedSizes::Shadow, true), (LearnedSizes::Off, false)] {
        let (store, state) = state();
        let cell = Cell::start_learned(mode, ladder(), Some(state)).await;
        let mut daemon = cell.daemon("node-kill", 8, 64).await;
        let blob = job("compile kill", "a", LINUX);
        cell.upload(&blob.blobs()).await;
        let mut ops = cell.execute(&blob.action).await;
        let start = daemon.start().await;
        assert_eq!(start.memory_bytes, 4 * GIB);
        // The acknowledgement comes after the requeue is written.
        assert!(
            daemon
                .report(killed(start.lease_id, 6 * GIB))
                .await
                .accepted
        );
        let rows_now = rows(&store);
        assert_eq!(!rows_now.is_empty(), wrote, "{mode:?}");
        if wrote {
            let summary = kbf_estimator::Summary::decode(&rows_now[0].1[1..]).expect("a summary");
            assert!(
                summary.oom_floor() >= 16 * GIB,
                "floor {}",
                summary.oom_floor()
            );
        }
        let rerun = daemon.start().await;
        assert_eq!(rerun.memory_bytes, 16 * GIB);
        let result = with_usage(output(&cell, "kill", 0).await, &usage(6 * GIB));
        assert!(daemon.report(ran(rerun.lease_id, &result)).await.accepted);
        done(&mut ops).await;
    }
}

fn killed_bare(lease: Option<worker::LeaseId>, kill: MemoryKill) -> worker::Result {
    worker::Result {
        lease_id: lease,
        status: Some(kbf_proto::google::rpc::Status {
            code: Code::ResourceExhausted as i32,
            message: "memory".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: kill as i32,
        ..worker::Result::default()
    }
}

fn floor_in(rows: &[(String, Vec<u8>)]) -> u64 {
    let summary = kbf_estimator::Summary::decode(&rows[0].1[1..]).expect("a summary");
    summary.oom_floor()
}

/// Catches: an own-limit kill whose report carries no usage dropped (the one event that
/// must raise a floor never reaches the estimator), and one given a basis or a limit it
/// did not have: its summary is written with the requeue's, with a floor at the rung the
/// retry runs at.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_with_no_usage_still_teaches_the_estimator() {
    log();
    let (store, state) = state();
    let cell = Cell::start_learned(LearnedSizes::Shadow, ladder(), Some(state)).await;
    let mut daemon = cell.daemon("node-bare", 8, 64).await;
    let blob = job("compile bare kill", "a", LINUX);
    cell.upload(&blob.blobs()).await;
    let mut ops = cell.execute(&blob.action).await;
    let start = daemon.start().await;
    let ack = daemon
        .report(killed_bare(start.lease_id, MemoryKill::OwnLimit))
        .await;
    assert!(ack.accepted);
    assert!(!rows(&store).is_empty(), "the kill is in the store");
    assert!(floor_in(&rows(&store)) >= 16 * GIB);
    let rerun = daemon.start().await;
    let result = with_usage(output(&cell, "bare", 0).await, &usage(MIB));
    assert!(daemon.report(ran(rerun.lease_id, &result)).await.accepted);
    done(&mut ops).await;
}

/// Catches: an own-limit kill that ends the operation (the ladder is spent: no requeue
/// carries it) left to the write-behind, so that its floor is lost to a crash in the
/// next seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kill_that_ends_the_operation_is_written_at_once() {
    log();
    let (store, state) = state();
    let one_rung = MemoryLadder::from_gib(&[4]).expect("a ladder");
    let cell = Cell::start_learned(LearnedSizes::Shadow, one_rung, Some(state)).await;
    let mut daemon = cell.daemon("node-final", 4, 4).await;
    let blob = job("compile final kill", "a", LINUX);
    cell.upload(&blob.blobs()).await;
    let mut ops = cell.execute(&blob.action).await;
    let start = daemon.start().await;
    assert!(
        daemon
            .report(killed(start.lease_id, 5 * GIB))
            .await
            .accepted
    );
    done(&mut ops).await;
    assert!(!rows(&store).is_empty(), "the kill is in the store");
}

/// Catches `--learned-sizes on` with `--no-memory-climb` leaving a floor for a kill that
/// raised nothing: the combination is allowed, and an own-limit kill that ends the operation
/// at the first rung writes no floor, so the next submission of the key books the first rung
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn learned_sizes_on_with_no_climb_leaves_no_floor() {
    log();
    let (_store, state) = state();
    let cell = Cell::start_no_climb(LearnedSizes::On, ladder(), Some(state)).await;
    let mut daemon = cell.daemon("node-fixed", 8, 64).await;
    let blob = job("compile fixed kill", "a", LINUX);
    cell.upload(&blob.blobs()).await;
    let mut ops = cell.execute(&blob.action).await;
    let start = daemon.start().await;
    assert_eq!(start.memory_bytes, 4 * GIB);
    assert!(
        daemon
            .report(killed(start.lease_id, 5 * GIB))
            .await
            .accepted
    );
    done(&mut ops).await;
    let _again = cell.execute(&blob.action).await;
    assert_eq!(daemon.start().await.memory_bytes, 4 * GIB);
}

/// Catches: a kill by the node (not by the action's own limit) teaching the estimator a
/// floor. The run's later success is written behind after the write-behind interval, and
/// no row the store holds then has a floor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_pressure_kill_sets_no_floor() {
    log();
    let (store, state) = state();
    let cell = Cell::start_learned(LearnedSizes::Shadow, ladder(), Some(state)).await;
    let mut daemon = cell.daemon("node-pressure", 8, 64).await;
    let blob = job("compile pressure kill", "a", LINUX);
    cell.upload(&blob.blobs()).await;
    let mut ops = cell.execute(&blob.action).await;
    let start = daemon.start().await;
    let mut hit = killed(start.lease_id, 5 * GIB);
    hit.memory_kill = MemoryKill::NodePressure as i32;
    assert!(daemon.report(hit).await.accepted);
    let rerun = daemon.start().await;
    assert_eq!(rerun.memory_bytes, 4 * GIB, "the same booking");
    let result = with_usage(output(&cell, "pressure", 0).await, &usage(MIB));
    assert!(daemon.report(ran(rerun.lease_id, &result)).await.accepted);
    done(&mut ops).await;
    // The write-behind of the success's summary, run now instead of when its interval is
    // up.
    cell.write_behind();
    let held = rows(&store);
    assert!(!held.is_empty(), "the write-behind has run");
    for row in &held {
        let summary = kbf_estimator::Summary::decode(&row.1[1..]).expect("a summary");
        assert_eq!(summary.oom_floor(), 0, "{}", row.0);
    }
}
