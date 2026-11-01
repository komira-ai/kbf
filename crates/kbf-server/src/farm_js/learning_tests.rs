//! Tests of learned sizes in shadow on a core driven through `farm_js::dispatch`
//! (`learning`): what the estimator is shown, what it says, and the summaries in the state
//! store. The farm-level cases that need the feature are in `farm::js::tests::learned`.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kbf_front::metrics::{ShadowBucket, ShadowLevel};
use kbf_front::{Cache, Described, Recorder, Submission};
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::{self, CgroupUsage, ResourceUsage, UsageBasis};
use kbf_sched::Request;
use kbf_state::Store;
use kbf_types::{ActionKey, Digest, DigestFunction, FarmTime, LeaseKind, Qos, Resources, WorkerId};

use super::dispatch;
use super::testing::{
    Flaky, Rig, Wire, overwrite, register, result_with, started, submit as plain_submit, wall,
};
use super::{Gate, write_learned};
use super::{KbfAdapter, open};
use crate::PlacementLoad;
use crate::action_usage::Shapes;
use crate::farm::durable::ESTIMATE;
use crate::learned::{Learned, LearnedSizes};
use crate::test_log::capture;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn at(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis)
}

/// A core of term 7 over `store` whose adapter learns as `mode` says.
fn learning_rig(store: &Arc<Flaky>, mode: LearnedSizes) -> Rig {
    learning_rig_stored(store, mode, true)
}

/// [`learning_rig`], whose adapter is told whether the core has a state store to write to.
fn learning_rig_stored(store: &Arc<Flaky>, mode: LearnedSizes, stored: bool) -> Rig {
    let shared: Arc<dyn Store> = store.clone();
    learning_rig_over(shared, store, mode, stored)
}

/// A core over `shared`, which is `store` or a wrapper of it.
fn learning_rig_over(
    shared: Arc<dyn Store>,
    store: &Arc<Flaky>,
    mode: LearnedSizes,
    stored: bool,
) -> Rig {
    let backend: kbf_front::Backend = Arc::new(Cache::memory()).into();
    let adapter = KbfAdapter::new(7, backend.clone(), Arc::clone(&shared), wall())
        .recording(Recorder::off())
        .learning_with(Learned::new(mode, 4 * GIB, 0), stored);
    let config = open::config(
        7,
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
        PlacementLoad::Off,
    );
    let core = open::open(
        &config,
        adapter,
        shared,
        crate::EpochPolicy::Keep,
        FarmTime::default(),
    )
    .expect("a core over the store");
    Rig {
        core,
        store: Arc::clone(store),
        backend,
    }
}

fn request(tag: u8, gpus: u64) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 4 * GIB).with_gpus(gpus),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("a platform"),
        whole_worker: false,
    }
}

fn submit(rig: &Rig, tag: u8, argv: &str, gpus: u64) -> kbf_front::Ticket {
    let submission = Submission {
        request: request(tag, gpus),
        call: None,
        described: Described {
            arguments: vec![argv.to_owned()],
            ..Described::default()
        },
        evidence: Default::default(),
        max_wall: None,
    };
    dispatch::submit(&rig.core, at(0), &Shapes::default(), submission).expect("queued")
}

/// A `Result` that exited 0 and peaked at `peak`.
fn measured(lease: kbf_types::LeaseId, peak: u64) -> worker::Result {
    let usage = ResourceUsage {
        wall_micros: 2_000_000,
        basis: UsageBasis::Cgroup as i32,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(1_000_000),
            memory_peak_bytes: Some(peak),
            ..CgroupUsage::default()
        }),
        ..ResourceUsage::default()
    };
    result_with(
        lease,
        ActionResult {
            exit_code: 0,
            execution_metadata: Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![usage.to_any()],
                ..Default::default()
            }),
            ..Default::default()
        },
    )
}

async fn report(rig: &Rig, worker: &str, result: worker::Result) -> worker::ResultAck {
    let ack = report_only(rig, worker, result).await;
    // As the farm does after the report: the summaries a kill changed, outside the lock.
    write(rig, false).await;
    ack
}

/// The core's answer to `result`, with no write of summaries after it.
async fn report_only(rig: &Rig, worker: &str, result: worker::Result) -> worker::ResultAck {
    dispatch::report(
        &rig.core,
        &rig.backend,
        &wall(),
        &|| at(0),
        &WorkerId::new(worker),
        result,
    )
    .await
    .expect("a host the core hears")
    .expect("an ack")
}

thread_local! {
    /// The gate of the writers of this test's core: a test runs on one thread.
    static GATE: Rc<Gate> = Rc::new(Gate::default());
}

/// [`write_learned`] on the gate of this test.
async fn write(rig: &Rig, tick: bool) {
    let gate = GATE.with(Rc::clone);
    write_learned(&rig.core, &gate, tick).await;
}

/// Runs the action `tag` of `argv` to the end on `node`, which reports `peak`.
async fn run(rig: &Rig, node: &mut Wire, name: &str, tag: u8, argv: &str, peak: u64) {
    let _ticket = submit(rig, tag, argv, 0);
    let lease = started(&node.sent()).expect("started");
    assert!(report(rig, name, measured(lease, peak)).await.accepted);
}

fn estimates(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.contains("size estimate:"))
        .map(str::to_owned)
        .collect()
}

fn tally(rig: &Rig, level: ShadowLevel, bucket: ShadowBucket) -> u64 {
    rig.core.with(|_, adapter| adapter.publish_learned());
    rig.backend.metrics().estimator_shadow.get(level, bucket)
}

fn rows(rig: &Rig) -> usize {
    rig.store
        .plain()
        .scan_prefix(ESTIMATE)
        .expect("a scan")
        .len()
}

/// Asserts `shadow` logs what the estimator would book for each new operation (the fallback
/// of the first two, then the estimate) and counts each attempt in the bucket of the estimate
/// it was judged against, and that a request for a GPU is no operation the estimator is asked
/// about. Catches: no line, the tally not published, and a GPU request asked about.
#[tokio::test(start_paused = true)]
async fn shadow_logs_the_estimate_of_each_new_operation_and_counts_the_attempts() {
    let log = capture();
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Shadow);
    let mut node = register(&rig, "node-shadow", 0);
    for tag in 1..=6_u8 {
        run(&rig, &mut node, "node-shadow", tag, "compile", 100 * MIB).await;
    }
    let _gpu = submit(&rig, 9, "compile", 1);

    let lines = estimates(&log.text());
    assert_eq!(lines.len(), 6, "{lines:?}");
    assert!(lines[0].contains("fallback=Missing"), "{}", lines[0]);
    assert!(lines[1].contains("fallback=Cold"), "{}", lines[1]);
    assert!(
        lines[5].contains("level=l2 memory_mib=1024"),
        "{}",
        lines[5]
    );
    assert_eq!(tally(&rig, ShadowLevel::None, ShadowBucket::Cold), 2);
    assert_eq!(tally(&rig, ShadowLevel::L2, ShadowBucket::Covered), 4);
}

/// Asserts `off` calls the estimator never. Catches: a line or a count with the estimator
/// off.
#[tokio::test(start_paused = true)]
async fn off_logs_nothing_and_counts_nothing() {
    let log = capture();
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Off);
    let mut node = register(&rig, "node-off", 0);
    for tag in 1..=3_u8 {
        run(&rig, &mut node, "node-off", tag, "compile", 100 * MIB).await;
    }
    assert!(estimates(&log.text()).is_empty());
    assert_eq!(tally(&rig, ShadowLevel::None, ShadowBucket::Cold), 0);
    assert_eq!(rows(&rig), 0);
}

/// Asserts the summaries are written behind at the tick, not at the report; that the sweep an
/// hour later leaves fresh rows; and that a core opened over the same store reads them, so
/// the action is no longer missing to it. Catches: summaries never written, written at every
/// report, a sweep that deletes fresh rows, and no restore on start.
#[tokio::test(start_paused = true)]
async fn summaries_are_written_behind_swept_and_read_back_on_start() {
    let log = capture();
    let store = Flaky::new();
    let rig = learning_rig(&store, LearnedSizes::Shadow);
    let mut node = register(&rig, "node-behind", 0);
    for tag in 1..=3_u8 {
        run(&rig, &mut node, "node-behind", tag, "compile", 100 * MIB).await;
    }
    assert_eq!(rows(&rig), 0, "nothing is written at the report");

    rig.core.tick(at(10_000)).await;
    write(&rig, true).await;
    let written = rows(&rig);
    assert!(written > 0, "written behind at the tick");

    // The first look at the sweep starts the hour; the next one after it sweeps.
    rig.core.tick(at(3_700_000)).await;
    write(&rig, true).await;
    assert_eq!(rows(&rig), written, "fresh rows are not swept");

    let second = learning_rig(&store, LearnedSizes::Shadow);
    let _node = register(&second, "node-again", 0);
    let _ticket = submit(&second, 7, "compile", 0);
    let lines = estimates(&log.text());
    let last = lines.last().expect("a line");
    assert!(!last.contains("fallback=Missing"), "{last}");
}

/// Asserts an own-limit kill that the scheduler requeues is written to the store at once,
/// before any tick, and teaches the estimator. Catches: the kill left to the write-behind, so
/// that a crash in the next seconds loses it.
#[tokio::test(start_paused = true)]
async fn an_own_limit_kill_is_written_at_once() {
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Shadow);
    let mut node = register(&rig, "node-kill", 0);
    let _ticket = submit(&rig, 1, "compile kill", 0);
    let lease = started(&node.sent()).expect("started");
    let kill = worker::Result {
        status: Some(kbf_proto::google::rpc::Status {
            code: tonic::Code::ResourceExhausted as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: worker::MemoryKill::OwnLimit as i32,
        ..result_with(lease, ActionResult::default())
    };
    assert_eq!(rows(&rig), 0);
    assert!(report(&rig, "node-kill", kill).await.accepted);
    assert!(rows(&rig) > 0, "the kill is in the store");
}

/// Three successful runs of `argv` on a node of `rig`, so that the estimator holds a summary.
async fn teach(rig: &Rig, name: &str, argv: &str) {
    let mut node = register(rig, name, 0);
    for tag in 1..=3_u8 {
        run(rig, &mut node, name, tag, argv, 100 * MIB).await;
    }
}

/// Flushes the summaries written behind at farm time `millis`.
async fn flush_at(rig: &Rig, millis: u64) {
    rig.core.tick(at(millis)).await;
    write(rig, true).await;
}

/// Asserts a summary the store refuses is kept and written at a later tick, and one a newer
/// writer's fence refuses is dropped and not kept. Catches: a refused write lost for good, a
/// fenced write retried at every tick.
#[tokio::test(start_paused = true)]
async fn a_refused_write_is_written_again_and_a_fenced_one_is_not_kept() {
    let store = Flaky::new();
    let rig = learning_rig(&store, LearnedSizes::Shadow);
    teach(&rig, "node-refused", "compile refused").await;

    store.set_down(true);
    flush_at(&rig, 10_000).await;
    assert_eq!(rows(&rig), 0, "the store refused");
    store.set_down(false);
    flush_at(&rig, 20_000).await;
    assert!(rows(&rig) > 0, "written again");

    let fenced = Flaky::new();
    let rig = learning_rig(&fenced, LearnedSizes::Shadow);
    teach(&rig, "node-fenced", "compile fenced").await;
    fenced.set_fenced(true);
    flush_at(&rig, 10_000).await;
    fenced.set_fenced(false);
    flush_at(&rig, 20_000).await;
    assert_eq!(rows(&rig), 0, "a fenced write is not kept");
}

/// Asserts summaries the store holds that cannot be read, or decoded, leave the action cold
/// and stop nothing: a core opens over a store whose `est/` rows cannot be read, and over one
/// that holds a row that is no summary, which the sweep skips. Catches: a start that fails
/// on a summary, a sweep that stops at a row it cannot decode.
#[tokio::test(start_paused = true)]
async fn summaries_that_cannot_be_read_or_decoded_are_skipped() {
    let store = Flaky::new();
    store.fail_reads(Some(ESTIMATE));
    let rig = learning_rig(&store, LearnedSizes::Shadow);
    teach(&rig, "node-unread", "compile unread").await;
    store.fail_reads(None);

    overwrite(store.plain(), "est/garbage", vec![1, 2, 3]);
    let rig = learning_rig(&store, LearnedSizes::Shadow);
    teach(&rig, "node-garbage", "compile garbage").await;
    flush_at(&rig, 10_000).await;
    flush_at(&rig, 3_700_000).await;
    assert!(rows(&rig) > 0);
}

/// Asserts the hourly sweep removes the summaries idle past the estimator's staleness (60 days) and
/// keeps the others, and does it from the store, not only from memory. Catches: a sweep that
/// never deletes, or one that deletes rows a day old.
#[tokio::test(start_paused = true)]
async fn the_sweep_deletes_stale_summaries_from_the_store() {
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Shadow);
    teach(&rig, "node-stale", "compile stale").await;
    flush_at(&rig, 10_000).await;
    let written = rows(&rig);
    assert!(written > 0);

    let seventy_days = 70 * 24 * 3_600_000;
    flush_at(&rig, seventy_days).await;
    flush_at(&rig, seventy_days + 3_700_000).await;
    assert!(rows(&rig) < written, "stale rows are swept");
}

/// Asserts what the estimator is not shown: a report with no usage that is no memory kill, an
/// operation whose platform is not pinned, a caller that joined a twin, and a lease given up
/// for a silent node. Catches: a panic or a made-up sample for any of them.
#[tokio::test(start_paused = true)]
async fn what_the_estimator_cannot_use_is_skipped() {
    let log = capture();
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Shadow);
    let mut node = register(&rig, "node-skip", 0);

    // No usage, no memory kill: nothing to learn.
    let _ticket = submit(&rig, 1, "compile skip", 0);
    let lease = started(&node.sent()).expect("started");
    let failed = worker::Result {
        status: Some(kbf_proto::google::rpc::Status {
            code: tonic::Code::Internal as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        ..result_with(lease, ActionResult::default())
    };
    assert!(report(&rig, "node-skip", failed).await.accepted);

    // The platform is not pinned: no key.
    let _unpinned = plain_submit(&rig, 2, 0).expect("queued");
    // A twin joined is no new operation.
    let _first = submit(&rig, 3, "compile twin", 0);
    let _twin = submit(&rig, 3, "compile twin", 0);
    let asked = estimates(&log.text()).len();
    assert_eq!(asked, 2, "{:?}", estimates(&log.text()));

    // A node that falls silent loses its lease: the estimator forgets the booking.
    rig.core.tick(at(600_000)).await;
}

/// A state store that checks the core's lock is free whenever the learning code calls it for
/// size summaries: a probe thread takes the core's lock (`Core::with`) while the call is made,
/// and the call fails if the probe cannot get it. Calls for anything else, which the core
/// makes under its own lock, are not checked.
struct Probed {
    armed: AtomicBool,
    inner: Arc<Flaky>,
    asking: AtomicBool,
    answered: AtomicBool,
    checked: AtomicUsize,
}

impl Probed {
    fn check(&self, what: &str) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        self.checked.fetch_add(1, Ordering::SeqCst);
        self.answered.store(false, Ordering::SeqCst);
        self.asking.store(true, Ordering::SeqCst);
        // The probe answers in a few yields when the lock is free; held, it never does.
        let mut spins = 0_u32;
        while !self.answered.load(Ordering::SeqCst) {
            spins += 1;
            assert!(spins < 2_000_000, "the core's lock is held across {what}");
            std::thread::yield_now();
        }
        self.asking.store(false, Ordering::SeqCst);
    }
}

impl Store for Probed {
    fn apply_batch(
        &self,
        batch: Vec<kbf_state::Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, kbf_state::Conflict>>, kbf_state::StoreError> {
        if format!("{batch:?}").contains("\"est/") {
            self.check("a write of size summaries");
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<kbf_state::Versioned>, kbf_state::StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, kbf_state::Versioned)>, kbf_state::StoreError> {
        if prefix == ESTIMATE {
            self.check("a scan of size summaries");
        }
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(
        &self,
        scan: kbf_state::ScanRange<'_>,
    ) -> Result<kbf_state::ScanPage, kbf_state::StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(
        &self,
        after: kbf_state::Revision,
        limit: usize,
    ) -> Result<kbf_state::Changes, kbf_state::StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, kbf_state::StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> kbf_state::CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> kbf_state::StoreTime {
        self.inner.now()
    }
}

/// Asserts no call the learning code makes to the state store (the write of a kill's
/// summaries after its report, the write behind at the tick, the hourly sweep's scan and
/// commits, and the read at the core's start) is made while the core's lock is held, so that
/// a slow store cannot hold up heartbeats and reports. Catches: a write or sweep moved back
/// into a hook or `Core::with`, which fails the probe; the test also requires that each of
/// the four was made, so that it cannot pass by checking nothing.
#[tokio::test(start_paused = true)]
async fn no_store_call_for_size_summaries_is_made_under_the_cores_lock() {
    let flaky = Flaky::new();
    let probed = Arc::new(Probed {
        inner: Arc::clone(&flaky),
        armed: AtomicBool::new(false),
        asking: AtomicBool::new(false),
        answered: AtomicBool::new(false),
        checked: AtomicUsize::new(0),
    });
    let shared: Arc<dyn Store> = probed.clone();
    // Leaked, so that the probe thread may hold the core for as long as the test runs.
    let rig: &'static Rig = Box::leak(Box::new(learning_rig_over(
        shared,
        &flaky,
        LearnedSizes::Shadow,
        true,
    )));
    let quit = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (probed, quit) = (Arc::clone(&probed), Arc::clone(&quit));
        std::thread::spawn(move || {
            while !quit.load(Ordering::SeqCst) {
                if probed.asking.load(Ordering::SeqCst) && !probed.answered.load(Ordering::SeqCst) {
                    rig.core.with(|_, _| ());
                    probed.answered.store(true, Ordering::SeqCst);
                }
                std::thread::yield_now();
            }
        })
    };
    probed.armed.store(true, Ordering::SeqCst);

    // A kill's summaries are written after its report; the rest at the tick, and the sweep
    // an hour after the first look.
    let mut node = register(rig, "node-probe", 0);
    let _ticket = submit(rig, 1, "compile probe", 0);
    let lease = started(&node.sent()).expect("started");
    let kill = worker::Result {
        status: Some(kbf_proto::google::rpc::Status {
            code: tonic::Code::ResourceExhausted as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: worker::MemoryKill::OwnLimit as i32,
        ..result_with(lease, ActionResult::default())
    };
    assert!(report(rig, "node-probe", kill).await.accepted);
    let after_kill = probed.checked.load(Ordering::SeqCst);
    assert!(after_kill > 0, "the kill's summaries were written");
    for tag in 2..=4_u8 {
        run(
            rig,
            &mut node,
            "node-probe",
            tag,
            "compile probe",
            100 * MIB,
        )
        .await;
    }
    flush_at(rig, 10_000).await;
    let after_tick = probed.checked.load(Ordering::SeqCst);
    assert!(after_tick > after_kill, "the write behind was made");
    flush_at(rig, 3_700_000).await;
    assert!(
        probed.checked.load(Ordering::SeqCst) > after_tick,
        "the sweep was made"
    );

    quit.store(true, Ordering::SeqCst);
    watcher.join().expect("the probe thread");
}

/// Asserts a writer takes each summary as it is when it writes, not as it was when it was
/// asked to: a tick's write behind that waits for the gate while a run changes the summary
/// writes the newer one. Catches: rows taken before the gate, which would let an older
/// summary land after a newer one.
#[tokio::test(start_paused = true)]
async fn a_writer_that_waited_for_the_gate_writes_the_newest_summary() {
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Shadow);
    let mut node = register(&rig, "node-race", 0);
    for tag in 1..=3_u8 {
        run(&rig, &mut node, "node-race", tag, "compile race", 100 * MIB).await;
    }
    rig.core.tick(at(10_000)).await;

    let gate = GATE.with(Rc::clone);
    let held = gate.0.lock().await;
    // The writer of the tick is waiting for the gate; a run changes the summary meanwhile.
    let waiting = write_learned(&rig.core, &gate, true);
    tokio::pin!(waiting);
    assert!(futures_poll_once(waiting.as_mut()).is_none(), "it waits");
    for tag in 4..=5_u8 {
        let _ticket = submit(&rig, tag, "compile race", 0);
        let lease = started(&node.sent()).expect("started");
        assert!(
            report_only(&rig, "node-race", measured(lease, 300 * MIB))
                .await
                .accepted
        );
    }
    drop(held);
    waiting.await;

    let stored = rig.store.plain().scan_prefix(ESTIMATE).expect("a scan");
    assert!(!stored.is_empty());
    for (name, row) in stored {
        let (key, written) =
            crate::farm::durable::estimate::decode(&name, &row.value).expect("a summary");
        let now = rig
            .core
            .with(|_, adapter| adapter.summary_now(&key))
            .expect("the summary is held");
        assert_eq!(written, now, "{name} holds an older summary");
    }
}

/// Polls `future` once: its output if it is ready, `None` if it is waiting.
fn futures_poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> Option<F::Output> {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match future.poll(&mut cx) {
        std::task::Poll::Ready(out) => Some(out),
        std::task::Poll::Pending => None,
    }
}

/// A macOS node with room for the actions of the tests.
fn register_mac(rig: &Rig, name: &str) -> Wire {
    let caps = kbf_caps::NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = crate::farm::Reported::serving(Resources::new(4_000, 8 * GIB), caps);
    let (outbound, inbox) = tokio::sync::mpsc::unbounded_channel();
    let welcome = worker::ServerMessage::default();
    let stream = dispatch::register(
        &rig.core,
        at(0),
        &WorkerId::new(name),
        kbf_sched::DaemonInstance::new(format!("daemon-{name}")),
        kbf_sched::DaemonInstance::new(""),
        reported,
        super::Connection { outbound, welcome },
    )
    .expect("a host the core hears");
    Wire { stream, inbox }
}

/// Asserts what an estimate is asked against: with no node of the operation's OS registered
/// the bounds are the unbounded ones; with a node of another OS only, the same (that node's
/// memory does not bound a Linux action); and a lease kind other than an action is not asked
/// about. Catches: another OS's node bounding the estimate, and a whole-machine lease given
/// an estimate for a share of a machine.
#[tokio::test(start_paused = true)]
async fn an_estimate_is_bounded_by_the_nodes_of_its_os_and_asked_for_actions_only() {
    let log = capture();
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Shadow);
    let _none = submit(&rig, 1, "compile bounds", 0);
    let _mac = register_mac(&rig, "node-mac");
    let _other = submit(&rig, 2, "compile bounds", 0);
    let whole = Submission {
        request: Request {
            kind: LeaseKind::Vm,
            ..request(3, 0)
        },
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    };
    let _vm = dispatch::submit(&rig.core, at(0), &Shapes::default(), whole).expect("queued");
    let lines = estimates(&log.text());
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().all(|l| l.contains("floor_mib=0")), "{lines:?}");
}

/// Asserts a core whose estimator is off takes nothing to write at a tick, and one whose
/// adapter has no state store to write to learns without writing and ends its sweeps at once.
/// Catches: summaries written to a store the core was not given, a sweep that never ends.
#[tokio::test(start_paused = true)]
async fn an_estimator_that_is_off_or_has_no_store_writes_nothing() {
    let off = learning_rig(&Flaky::new(), LearnedSizes::Off);
    write(&off, true).await;
    assert_eq!(rows(&off), 0);

    let store = Flaky::new();
    let rig = learning_rig_stored(&store, LearnedSizes::Shadow, false);
    teach(&rig, "node-bare", "compile bare").await;
    flush_at(&rig, 10_000).await;
    flush_at(&rig, 3_700_000).await;
    assert_eq!(rows(&rig), 0);
}

/// Asserts a store that is fenced as the sweep is due stops the sweep and keeps nothing: the
/// summaries are not written, and the sweep is over for the hour. Catches: a sweep that goes
/// on against a fenced store.
#[tokio::test(start_paused = true)]
async fn a_fence_that_comes_as_the_sweep_is_due_ends_it() {
    let store = Flaky::new();
    let rig = learning_rig(&store, LearnedSizes::Shadow);
    let mut node = register(&rig, "node-fence", 0);
    for tag in 1..=3_u8 {
        run(
            &rig,
            &mut node,
            "node-fence",
            tag,
            "compile fence",
            100 * MIB,
        )
        .await;
    }
    flush_at(&rig, 10_000).await;
    let written = rows(&rig);
    run(&rig, &mut node, "node-fence", 4, "compile fence", 100 * MIB).await;
    store.set_fenced(true);
    flush_at(&rig, 3_700_000).await;
    store.set_fenced(false);
    assert_eq!(rows(&rig), written, "nothing was written or swept");
}

/// Asserts the sweep leaves a row it cannot read as a summary alone, and strips the samples
/// of a summary that holds a floor without deleting the floor. Catches: a sweep that stops
/// at a bad row, and one that deletes a floor.
#[tokio::test(start_paused = true)]
async fn the_sweep_skips_a_bad_summary_and_keeps_a_floor() {
    let store = Flaky::new();
    let rig = learning_rig(&store, LearnedSizes::Shadow);
    let mut node = register(&rig, "node-floor", 0);
    for tag in 1..=3_u8 {
        run(
            &rig,
            &mut node,
            "node-floor",
            tag,
            "compile floor",
            100 * MIB,
        )
        .await;
    }
    let _ticket = submit(&rig, 7, "compile killed", 0);
    let lease = started(&node.sent()).expect("started");
    let kill = worker::Result {
        status: Some(kbf_proto::google::rpc::Status {
            code: tonic::Code::ResourceExhausted as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: worker::MemoryKill::OwnLimit as i32,
        ..result_with(lease, ActionResult::default())
    };
    assert!(report(&rig, "node-floor", kill).await.accepted);
    flush_at(&rig, 10_000).await;
    // One row is damaged where the store holds it.
    let held = store.plain().scan_prefix(ESTIMATE).expect("a scan");
    let (name, row) = held.first().expect("a row");
    let mut damaged = row.value.clone();
    let last = damaged.len() - 1;
    damaged[last] ^= 0xFF;
    overwrite(store.plain(), name, damaged);

    let seventy_days = 70 * 24 * 3_600_000;
    flush_at(&rig, seventy_days).await;
    flush_at(&rig, seventy_days + 3_700_000).await;
    assert!(rows(&rig) > 0, "the floor of the kill is kept");
}

/// Asserts a report the estimator was not shown the start of, or whose operation is gone, is
/// skipped without a sample. Catches: a panic or a made-up booking for a lease the farm never
/// started.
#[tokio::test(start_paused = true)]
async fn a_report_without_a_known_start_or_operation_teaches_nothing() {
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Shadow);
    let mut node = register(&rig, "node-unknown", 0);
    let _ticket = submit(&rig, 1, "compile unknown", 0);
    let lease = started(&node.sent()).expect("started");
    let outcome = kbf_types::Outcome::Failed(kbf_types::Failure::Infra);
    let worker = WorkerId::new("node-unknown");
    rig.core.with(|view, adapter| {
        let unknown = kbf_types::LeaseId::new(7, 999);
        adapter.stage_learning(
            view.sched(),
            kbf_types::OperationId(0),
            unknown,
            &worker,
            outcome,
            None,
        );
        adapter.stage_learning(
            view.sched(),
            kbf_types::OperationId(999),
            lease,
            &worker,
            outcome,
            None,
        );
    });
    assert!(
        report(&rig, "node-unknown", measured(lease, 100 * MIB))
            .await
            .accepted
    );
    assert_eq!(tally(&rig, ShadowLevel::None, ShadowBucket::Cold), 1);
}

/// Asserts the rows taken for keys the estimator holds no summary of are none, so that a key
/// that was swept from memory between a report and its write is not written as an empty row.
/// Catches: a row made for an unknown key.
#[test]
fn a_key_with_no_summary_has_no_row_to_write() {
    let mut learned = Learned::new(LearnedSizes::Shadow, 4 * GIB, 0);
    let platform = crate::machine::Platform::of(
        &kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("a platform"),
    );
    let described = Described {
        arguments: vec!["compile".to_owned()],
        ..Described::default()
    };
    let subject = crate::action_usage::Subject::of(described).0;
    let basis = crate::learned::basis_of_os("linux").expect("a basis");
    let key = crate::learned::key_of("main", &platform, basis, &subject).expect("a key");
    let keys = std::collections::BTreeSet::from([key]);
    assert!(learned.take_rows(keys).is_empty());
}

/// Asserts `write_behind_now` writes the changed summaries at once, before the write-behind
/// interval and without a tick, and returns with them in the store; and that a second call
/// with nothing changed writes nothing more. Catches: a call that leaves the summaries to the
/// next tick, and one that rewrites summaries no run changed.
#[tokio::test(start_paused = true)]
async fn write_behind_now_writes_the_changed_summaries_before_returning() {
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Shadow);
    teach(&rig, "node-now", "compile now").await;
    assert_eq!(rows(&rig), 0, "nothing is written at the report");

    let gate = GATE.with(Rc::clone);
    super::write_behind_now(&rig.core, &gate);
    let written = rows(&rig);
    assert!(written > 0, "written at once");

    // Overwritten with a marker: a second call that writes nothing leaves it alone.
    let (name, _) = rig.store.plain().scan_prefix(ESTIMATE).expect("a scan")[0].clone();
    overwrite(rig.store.plain(), &name, vec![9]);
    super::write_behind_now(&rig.core, &gate);
    let held = rig.store.plain().get(&name).expect("a get").expect("a row");
    assert_eq!(
        held.value,
        vec![9],
        "an unchanged summary is not written again"
    );
}

/// Asserts an operation whose caller the machine does not know is not asked about, without a
/// panic or a line. Catches: an estimate made for a waiter that is gone.
#[tokio::test(start_paused = true)]
async fn an_operation_whose_caller_is_unknown_is_not_asked_about() {
    let log = capture();
    let rig = learning_rig(&Flaky::new(), LearnedSizes::Shadow);
    let admitted = rig
        .core
        .submit(at(0), request(1, 0), &super::testing::parcel(), |_, _| {})
        .expect("queued");
    let sized = super::learning::Sized {
        kind: LeaseKind::Action,
        gpus: 0,
        memory_bytes: 4 * GIB,
        cpu_millis: 1_000,
        evidence: Default::default(),
    };
    rig.core
        .with(|view, adapter| adapter.learn_submit(view, &admitted, &sized, None));
    assert!(estimates(&log.text()).is_empty());
}

/// Asserts a writer that finds the gate held waits until it is let go and then takes it,
/// without a runtime. Catches: a wait that gives up, or a take that does not wait.
#[test]
fn a_writer_without_a_runtime_waits_for_the_gate_to_be_let_go() {
    let gate = Gate::default();
    let mut held = Some(gate.0.try_lock().expect("the gate is free"));
    let mut waited = 0_u32;
    let taken = super::learning::hold(&gate, || {
        waited += 1;
        held.take();
    });
    assert_eq!(waited, 1, "it waited once, for the one holder");
    assert!(gate.0.try_lock().is_err(), "and now holds the gate");
    drop(taken);
}

/// Asserts the hourly sweep is bounded per tick and carries on at the next: more stale
/// summaries than one tick commits are not all deleted at once, and the next tick finishes
/// them. Catches: a sweep that does all of a large store in one tick (the lock-free write
/// still holds the writer's gate and the store), and one that forgets what is left.
#[tokio::test(start_paused = true)]
async fn a_large_sweep_is_bounded_per_tick_and_continues_at_the_next() {
    let store = Flaky::new();
    let rig = learning_rig(&store, LearnedSizes::Shadow);
    teach(&rig, "node-large", "compile large").await;
    flush_at(&rig, 10_000).await;
    let (name, row) = store.plain().scan_prefix(ESTIMATE).expect("a scan")[0].clone();
    let first = name.strip_prefix(ESTIMATE).expect("a row name");
    let (instance, rest) = first.split_once('/').expect("an instance segment");
    for n in 0..1_100 {
        // The instance is hex of its text: the copies name instances "main0000", "main0001"...
        let suffix: String = format!("{n:04}")
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        let copy = format!("{ESTIMATE}{instance}{suffix}/{rest}");
        overwrite(store.plain(), &copy, row.value.clone());
    }
    let before = rows(&rig);

    let seventy_days = 70 * 24 * 3_600_000;
    flush_at(&rig, seventy_days).await;
    let after_one = rows(&rig);
    assert!(
        after_one < before && after_one > 0,
        "{before} -> {after_one}"
    );
    flush_at(&rig, seventy_days + 1_000).await;
    assert!(rows(&rig) < after_one, "the next tick goes on");
}

/// Asserts the estimator's record of which size keys it named is bounded: past the bound the
/// oldest estimator key is dropped with its size keys. Catches: a record that grows without
/// end for a farm that runs for months.
#[test]
fn the_names_of_size_keys_are_bounded() {
    let mut learned = Learned::new(LearnedSizes::On, 4 * GIB, 0);
    let platform = crate::machine::Platform::of(
        &kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("a platform"),
    );
    let described = Described {
        arguments: vec!["compile".to_owned()],
        ..Described::default()
    };
    let subject = crate::action_usage::Subject::of(described).0;
    let basis = crate::learned::basis_of_os("linux").expect("a basis");
    let key_of = |n: usize| {
        crate::learned::key_of(&format!("instance-{n}"), &platform, basis, &subject).expect("a key")
    };
    learned.register(&key_of(0), 0);
    assert!(learned.summary_key(0).is_some());
    for n in 1..=65_536_u64 {
        learned.register(&key_of(n as usize), n);
    }
    assert!(
        learned.summary_key(0).is_none(),
        "the oldest name is dropped"
    );
    assert!(learned.summary_key(65_536).is_some());
}
