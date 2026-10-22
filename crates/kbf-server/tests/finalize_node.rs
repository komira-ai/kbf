//! A storage node's side of `kbf cluster finalize` (`kbf_server::finalize`): what a node
//! does with the target its frontend names, over a log in an in-memory raft directory
//! (`FaultFs`), and a started node against a real frontend that a test raises the farm's
//! target on (`Members::raise_finalize`), end to end through the frontend's answers, the
//! node's joiner, its task and the facts its heartbeats bring back.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::Cache;
use kbf_log::{FarmLogId, MachineVersion};
use kbf_map::MemberId;
use kbf_meta::Retention;
use kbf_server::finalize::{Decision, FinalizeTarget, Outcome, apply, at_start, decide};
use kbf_server::members::{Members, MembershipConfig, NodeFacts};
use kbf_server::raft_log::RUNS;
use kbf_server::storage_node::{
    COLD_RELEASE_DEADLINE, StorageFlags, command, parse, start_storage,
};
use kbf_server::stores::FILE_PREFIX_ROOT;
use kbf_server::{EpochPolicy, Listeners, RaftMetaLog, StateStore, bind_server_with_state};
use kbf_state::Store;
use kbf_store::FaultFs;

const MS: Duration = Duration::from_millis(1);

const V1: MachineVersion = MachineVersion::V1;
const V2: MachineVersion = MachineVersion::V2;
const V3: MachineVersion = MachineVersion::V3;
const V5: MachineVersion = MachineVersion::V5;

async fn open(fs: &FaultFs) -> RaftMetaLog<FaultFs> {
    let id = FarmLogId::new(NonZeroU64::new(9).expect("non-zero"));
    RaftMetaLog::open(fs.clone(), Retention::default(), "kbf/", move || Ok(id))
        .await
        .expect("open the log")
}

/// Asserts that a target asks for a raise only when the log is below it and the build runs
/// it: a target above the log and within what the build runs is a raise to it, one at or
/// below the log (or 0) asks for nothing, one past what the build runs is `Beyond`, and a log
/// already past the target is current even when the build runs less.
#[test]
fn a_target_asks_for_a_raise_only_above_the_log_and_within_the_build() {
    let cases = [
        ((3, 1, 5), Decision::Raise(V3)),
        ((5, 4, 5), Decision::Raise(V5)),
        ((2, 1, 5), Decision::Raise(V2)),
        ((4, 4, 5), Decision::Current),
        ((2, 4, 5), Decision::Current),
        ((0, 4, 5), Decision::Current),
        ((6, 4, 5), Decision::Beyond),
        ((6, 1, 3), Decision::Beyond),
        ((6, 6, 5), Decision::Current),
    ];
    for ((target, at, runs), want) in cases {
        assert_eq!(
            decide(target, at, runs),
            want,
            "{target} at {at}, runs {runs}"
        );
    }
}

/// Asserts that a node keeps the highest target it was told: a frontend that names none, a
/// target of 0, a lower one and the same one change nothing and wake no one, a higher one is
/// kept and wakes the task that acts on it.
#[tokio::test]
async fn the_node_keeps_the_highest_target_it_is_told() {
    let state = FinalizeTarget::new();
    assert_eq!(state.target(), 0);
    let mut woken = state.subscribe();
    state.learn(None);
    state.learn(Some(0));
    assert_eq!(state.target(), 0);
    assert!(!woken.has_changed().expect("sender"), "nothing learned");

    state.learn(Some(4));
    assert_eq!(state.target(), 4);
    woken.changed().await.expect("woken by the first target");

    state.learn(Some(3));
    state.learn(Some(4));
    state.learn(None);
    assert_eq!(state.target(), 4);
    assert!(!woken.has_changed().expect("sender"), "woken for no rise");

    state.learn(Some(5));
    assert_eq!(state.target(), 5);
    woken.changed().await.expect("woken by a higher target");
}

/// Asserts that a log below the target is raised to it, past the log's start ceiling, and its
/// version handle (the number a node reports at each heartbeat) follows; that a target at or
/// below the log's version commits nothing (the applied index does not move) and never
/// lowers it; and that a target past what the build runs leaves the log as it is.
#[tokio::test(start_paused = true)]
async fn a_log_below_the_target_is_raised_and_one_at_or_above_is_left() {
    let log = open(&FaultFs::new()).await.with_start_ceiling(V2);
    let handle = log.version_handle();
    assert_eq!(apply(&log, 3, 5), Outcome::Raised { from: 1, to: 3 });
    assert_eq!(log.machine_version(), V3);
    assert_eq!(handle.load(Ordering::Relaxed), 3);
    assert_eq!(apply(&log, 5, 5), Outcome::Raised { from: 3, to: 5 });
    assert_eq!(log.machine_version(), V5, "past the start ceiling");
    assert_eq!(handle.load(Ordering::Relaxed), 5);

    let applied = log.applied_index();
    assert_eq!(apply(&log, 5, 5), Outcome::Current);
    assert_eq!(apply(&log, 3, 5), Outcome::Current);
    assert_eq!(apply(&log, 0, 5), Outcome::Current);
    assert_eq!(apply(&log, 6, 5), Outcome::Beyond { runs: 5 });
    assert_eq!(log.machine_version(), V5);
    assert_eq!(log.applied_index(), applied, "a raise committed again");

    let fresh = open(&FaultFs::new()).await;
    assert_eq!(apply(&fresh, 6, 5), Outcome::Beyond { runs: 5 });
    assert_eq!(fresh.machine_version(), V1);
}

/// Asserts that a raise the log refuses (here a log opened by a build that runs version 3 and
/// told to go to 5) is reported as failed with the log's reason, and leaves the log as it was.
#[tokio::test(start_paused = true)]
async fn a_raise_the_log_refuses_is_reported_and_changes_nothing() {
    let id = FarmLogId::new(NonZeroU64::new(9).expect("non-zero"));
    let older = RaftMetaLog::open_running(
        FaultFs::new(),
        Retention::default(),
        "kbf/",
        move || Ok(id),
        V3,
    )
    .await
    .expect("opens");
    let Outcome::Failed(why) = apply(&older, 5, 5) else {
        panic!("a log that runs 3 was raised to 5");
    };
    assert!(why.contains("past 3"), "{why}");
    assert_eq!(older.machine_version(), V1);
}

/// Asserts that `--finalize-log` raises the log once and leaves a log that is there already,
/// and refuses a version past what the build runs, or one the log refuses, with an error that
/// names the flag.
#[tokio::test(start_paused = true)]
async fn the_flag_raises_the_log_or_says_why_not() {
    let log = open(&FaultFs::new()).await;
    at_start(&log, 4).expect("a raise");
    assert_eq!(log.machine_version(), MachineVersion::V4);
    at_start(&log, 4).expect("already there");
    at_start(&log, 2).expect("below the log");
    assert_eq!(log.machine_version(), MachineVersion::V4);
    let newest = RUNS.get();
    let said = at_start(&log, newest + 1).expect_err("past what the build runs");
    assert!(said.contains("--finalize-log"), "{said}");
    assert!(said.contains(&format!("up to {newest}")), "{said}");

    let id = FarmLogId::new(NonZeroU64::new(9).expect("non-zero"));
    let older = RaftMetaLog::open_running(
        FaultFs::new(),
        Retention::default(),
        "kbf/",
        move || Ok(id),
        V3,
    )
    .await
    .expect("opens");
    let said = at_start(&older, newest).expect_err("the log runs less");
    let flag = format!("--finalize-log {newest}");
    assert!(said.contains(&flag), "{said}");
    assert!(said.contains("past 3"), "{said}");
}

fn parsed(more: &[&str]) -> Result<StorageFlags, clap::Error> {
    let mut argv = vec!["storage"];
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv)?)
}

/// Asserts that `--finalize-log` takes a version from 1 to the newest the build runs, is
/// absent by default, and is refused for 0, for a version past the build, for a word that is
/// not a number, and together with `--frontend` (a registered node is raised by its
/// frontend).
#[test]
fn the_flag_takes_a_version_the_build_runs_and_not_with_a_frontend() {
    assert_eq!(parsed(&[]).expect("flags").finalize_log, None);
    for version in 1..=RUNS.get() {
        let flag = format!("--finalize-log={version}");
        let flags = parsed(&[flag.as_str()]).expect("flags");
        assert_eq!(flags.finalize_log, Some(version));
    }
    let past = format!("--finalize-log={}", RUNS.get() + 1);
    let invalid = clap::error::ErrorKind::ValueValidation;
    for refused in ["--finalize-log=0", past.as_str(), "--finalize-log=five"] {
        let err = parsed(&[refused]).expect_err(refused);
        assert_eq!(err.kind(), invalid, "{refused}");
    }
    let err = parsed(&["--finalize-log=4", "--frontend=http://127.0.0.1:1"])
        .expect_err("a registered node is raised by its frontend");
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
}

/// A new, empty directory for one test, removed when it drops.
fn dir(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("finalize-{name}"))
}

/// The flags of `kbf storage` over stores in `dir`, plain text on a loopback port, and these.
fn flags(dir: &Path, more: &[String]) -> StorageFlags {
    let mut argv = vec![
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--meta-dir={}", dir.join("meta").display()),
    ];
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv).expect("flags")).expect("parsed")
}

/// Starts a node over `dir` with `more`, stops it at once, and waits for it to release its
/// files.
async fn start_and_stop(dir: &Path, more: &[String]) {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let node = start_storage(&flags(dir, more), || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
    .expect("starts");
    let serving = tokio::spawn(node.serve_checked(COLD_RELEASE_DEADLINE));
    stop.send(()).expect("stop");
    let (served, released) = serving.await.expect("task");
    served.expect("serves to the end");
    released.expect("the cold store is released");
}

/// Like [`start_and_stop`], for the storage half of `kbf storage-and-daemon`, which has no cold
/// store to wait for: the stop ends when it has served to the end.
async fn start_and_stop_half(dir: &Path, more: &[String]) {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let half = kbf_server::storage_half::start(&flags(dir, more), || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
    .expect("starts");
    let serving = tokio::spawn(half.serve());
    stop.send(()).expect("stop");
    serving.await.expect("task").expect("serves to the end");
}

/// The machine version of the log in `dir`'s `meta`, opened as the node opens it. The half
/// of a node that is stopping may hold the directory a moment longer, so the open is tried
/// again.
async fn version_on_disk(dir: &Path) -> MachineVersion {
    let meta = dir.join("meta");
    let open = || RaftMetaLog::open_dir(&meta, Retention::default(), FILE_PREFIX_ROOT);
    let mut opened = open().await;
    for _ in 0..100 {
        if opened.is_ok() {
            break;
        }
        tokio::time::sleep(20 * MS).await;
        opened = open().await;
    }
    opened.expect("opens the node's log").machine_version()
}

/// Asserts that a node with no frontend raises its log at start only when `--finalize-log`
/// says so: started with `--finalize-log=4` its log is at version 4 after, and started
/// without the flag it is at version 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_with_no_frontend_raises_its_log_only_when_the_flag_says_so() {
    let raised = dir("flag");
    start_and_stop(&raised, &["--finalize-log=4".to_owned()]).await;
    assert_eq!(version_on_disk(&raised).await, MachineVersion::V4);

    let left = dir("no-flag");
    start_and_stop(&left, &[]).await;
    assert_eq!(version_on_disk(&left).await, V1);
}

/// Asserts that the storage half of `kbf storage-and-daemon`, started with no frontend, raises
/// its log at start when `--finalize-log` says so, and leaves it when it does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_half_with_no_frontend_raises_its_log_only_when_the_flag_says_so() {
    let raised = dir("half-flag");
    start_and_stop_half(&raised, &["--finalize-log=3".to_owned()]).await;
    assert_eq!(version_on_disk(&raised).await, V3);

    let left = dir("half-no-flag");
    start_and_stop_half(&left, &[]).await;
    assert_eq!(version_on_disk(&left).await, V1);
}

/// A TTL of 60 ms (beats every 20 ms) on a clock that moves only when a test moves it.
fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: 60 * MS,
        dwell: Duration::from_secs(1),
        replication: 2,
    }
}

/// A frontend over a new fake state store: its worker address and its members.
fn frontend() -> (SocketAddr, Members) {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let loopback: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let listeners = Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: Duration::from_secs(5),
        hello_wait: Duration::from_secs(10),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        default_platform: kbf_front::DefaultPlatform::NONE,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamTiming::default(),
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::memory()),
        listeners,
        None,
        kbf_auth::Policy::allow_all(),
        Some(state),
        std::future::pending::<()>(),
    )
    .expect("binds");
    let worker = bound.worker;
    tokio::spawn(bound.serving);
    let members = Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config());
    (worker, members)
}

/// The flags of a node registering with the frontend at `worker`.
fn registered(dir: &Path, worker: SocketAddr) -> StorageFlags {
    flags(dir, &[format!("--frontend=http://{worker}")])
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

/// The facts the row of member 1 says once its log is at `version`, or the last it said after
/// five seconds.
async fn log_at(members: &Members, version: u32) -> NodeFacts {
    let mut seen = NodeFacts::default();
    for _ in 0..1000 {
        let mut all = members.node_facts().expect("rows");
        seen = all.remove(&MemberId(1)).unwrap_or_default();
        if seen.log_version == Some(version) {
            break;
        }
        tokio::time::sleep(5 * MS).await;
    }
    seen
}

/// Asserts that a registered node raises its log when the farm's target is raised, through
/// the frontend's next heartbeat answer: its log is at 4 once it has opened (a registered
/// node's log is raised to 4 as it opens), the target is then raised to 5 on the frontend, and
/// the member's row says 5, with the commit and the version its build runs unchanged.
#[tokio::test]
async fn a_registered_node_raises_its_log_to_the_target_its_heartbeat_brings() {
    let (worker, members) = frontend();
    let dir = dir("heartbeat");
    let node = start_storage(&registered(&dir, worker), never)
        .await
        .expect("starts");
    let serving = tokio::spawn(node.serve());
    let opened = log_at(&members, 4).await;
    assert_eq!(opened.log_version, Some(4));
    assert_eq!(members.finalize_target().expect("read"), None);

    assert!(members.raise_finalize(5).expect("raise the target"));
    let raised = log_at(&members, 5).await;
    serving.abort();
    assert_eq!(raised.log_version, Some(5));
    assert_eq!(raised.runs_log, opened.runs_log);
    assert_eq!(raised.build_commit, opened.build_commit);
}

/// Asserts that a node started after the farm's target was raised learns it when it
/// registers and raises its log then, with no heartbeat needed to carry it.
#[tokio::test]
async fn a_node_started_after_the_target_was_raised_raises_its_log() {
    let (worker, members) = frontend();
    assert!(members.raise_finalize(5).expect("raise the target"));
    let dir = dir("register");
    let node = start_storage(&registered(&dir, worker), never)
        .await
        .expect("starts");
    let serving = tokio::spawn(node.serve());
    let raised = log_at(&members, 5).await;
    serving.abort();
    assert_eq!(raised.log_version, Some(5));
}

/// Asserts that the storage half of `kbf storage-and-daemon` acts on the target as `kbf
/// storage` does: told by its frontend's registration, its log is raised to it.
#[tokio::test]
async fn the_storage_half_raises_its_log_to_the_target_too() {
    let (worker, members) = frontend();
    assert!(members.raise_finalize(5).expect("raise the target"));
    let dir = dir("half");
    let half = kbf_server::storage_half::start(&registered(&dir, worker), never)
        .await
        .expect("starts");
    let serving = tokio::spawn(half.serve());
    let raised = log_at(&members, 5).await;
    serving.abort();
    assert_eq!(raised.log_version, Some(5));
}
