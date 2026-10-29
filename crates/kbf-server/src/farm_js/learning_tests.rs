//! Tests of learned sizes in shadow on a core driven through `farm_js::dispatch`
//! (`learning`): what the estimator is shown, what it says, and the summaries in the state
//! store. The farm-level cases that need the feature are in `farm::js::tests::learned`.

use std::sync::Arc;

use kbf_front::metrics::{ShadowBucket, ShadowLevel};
use kbf_front::{Cache, Described, Recorder, Submission};
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::{self, CgroupUsage, ResourceUsage, UsageBasis};
use kbf_sched::Request;
use kbf_state::Store;
use kbf_types::{ActionKey, Digest, DigestFunction, FarmTime, LeaseKind, Qos, Resources, WorkerId};

use super::dispatch;
use super::testing::{Flaky, Rig, Wire, register, result_with, started, wall};
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
    let shared: Arc<dyn Store> = store.clone();
    let backend: kbf_front::Backend = Arc::new(Cache::memory()).into();
    let adapter = KbfAdapter::new(7, backend.clone(), Arc::clone(&shared), wall())
        .recording(Recorder::off())
        .learning_with(Learned::new(mode, 4 * GIB, 0), true);
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
    dispatch::report(
        &rig.core,
        &rig.backend,
        &wall(),
        || at(0),
        &WorkerId::new(worker),
        result,
    )
    .await
    .expect("a host the core hears")
    .expect("an ack")
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
    rig.core.with(|_, adapter| adapter.flush_learned());
    let written = rows(&rig);
    assert!(written > 0, "written behind at the tick");

    // The first look at the sweep starts the hour; the next one after it sweeps.
    rig.core.tick(at(3_700_000)).await;
    rig.core.with(|_, adapter| adapter.flush_learned());
    rig.core.with(|_, adapter| adapter.write_behind_now());
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
