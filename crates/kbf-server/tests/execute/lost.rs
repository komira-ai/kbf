//! Runs the farm lost, end to end in one process (issue #22): a REAPI client, the
//! server, and fake daemons. A daemon that restarted mid-lease and names the process
//! before it (whose runs its drivers killed) has that process's leases run again at
//! once, on another node when one can run them; a `Result` that says the run was lost
//! (`Result.lost`: fenced, or a whole-machine action whose result a restart took) runs
//! the action again; three such runs answer the client INTERNAL, naming the node and
//! the runs. Nothing a lost run produced reaches the client or the action cache.
//!
//! A module of the `execute` test binary, not a binary of its own: `Farm` is generic,
//! and coverage counts each generic function by its best-covered instantiation.

use std::time::Duration;

use kbf_proto::worker::{self, Hello, LeaseId, NodePhase, Phase, Start, daemon_message};
use tonic::Code;

use crate::support::{
    Cell, Client, FakeDaemon, Job, done, failed, hello, hello_on, output, ran, response,
};

/// Well under the handover grace (45 s), and long enough for a rerun to be placed.
const FAST: Duration = Duration::from_secs(2);

/// Long enough for the server to see a closed stream.
const CLOSING: Duration = Duration::from_millis(300);

/// `node`'s Hello from daemon process `instance`, naming `predecessor` before it.
fn restarted(node: &str, instance: &str, predecessor: &str) -> Hello {
    Hello {
        instance_id: instance.to_owned(),
        predecessor_instance_id: predecessor.to_owned(),
        ..hello(node, 4, 8)
    }
}

/// A Result for `lease` that says the farm lost the run as `why` says.
fn lost(lease: Option<LeaseId>, why: worker::Lost) -> worker::Result {
    worker::Result {
        lost: why as i32,
        ..failed(lease, Code::Unavailable)
    }
}

/// The next `Start` `daemon` gets within [`FAST`], if any.
async fn start_within_fast(daemon: &mut FakeDaemon) -> Option<Start> {
    daemon
        .expect_within(FAST, |m| match m {
            worker::server_message::Message::Start(s) => Some(s.clone()),
            _ => None,
        })
        .await
}

/// A cell with `node-a` (daemon process `i1`) and `node-b`, and `job` started on
/// `node-a`; its operation stream, both daemons and the `Start`.
async fn running_on_a(
    job: &Job,
) -> (
    Cell,
    tonic::Streaming<kbf_proto::google::longrunning::Operation>,
    FakeDaemon,
    FakeDaemon,
    Start,
) {
    let cell = Cell::start().await;
    let mut a = cell.daemon_process("node-a", "i1", 4, 8).await;
    let b = cell.daemon("node-b", 4, 8).await;
    cell.upload(&job.blobs()).await;
    let ops = cell.execute(&job.action).await;
    let start = a.start().await;
    (cell, ops, a, b, start)
}

/// Connects `node-a` again as `hello`, and sends its first heartbeat, which lists
/// nothing.
async fn come_back(cell: &Client, hello: Hello) -> FakeDaemon {
    let mut a = FakeDaemon::connect(cell.worker_addr, hello)
        .await
        .expect("registered");
    assert!(a.heartbeat(&[]).await);
    a
}

/// Catches the fast path missing (buck2 would wait out the 45 s handover grace, or get
/// INTERNAL): a daemon that restarts mid-lease and names its predecessor, whose stream
/// has closed, has the lease run again within seconds, on the other node (the "avoid
/// ignored" mutant would send it back to `node-a`). The client gets the second run's
/// OK result, which the action cache holds; the first run's late `Result` is refused,
/// answers nobody and leaves the cache alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_restart_mid_lease_reruns_at_once_on_another_node() {
    let job = Job::new("restarted under it", &[]);
    let (cell, mut ops, a, mut b, first) = running_on_a(&job).await;
    a.close();
    tokio::time::sleep(CLOSING).await;
    let mut a = come_back(&cell, restarted("node-a", "i2", "i1")).await;
    let again = start_within_fast(&mut b)
        .await
        .expect("no rerun on node-b within 2 s");
    assert_eq!(again.action_digest, first.action_digest);
    assert_ne!(again.lease_id, first.lease_id);
    let result = output(&cell, "from node-b", 0).await;
    assert!(b.report(ran(again.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    assert_eq!(cell.cached(&job.action).await, Ok(result.clone()));

    let late = output(&cell, "from the killed run", 0).await;
    assert!(!a.report(ran(first.lease_id, &late)).await.accepted);
    assert_eq!(cell.cached(&job.action).await, Ok(result));
    a.no_work().await;
}

/// Catches a predecessor claim honoured while the named process's stream is still open
/// (the mutant "no closed-stream check"): that process may be alive on another machine
/// with a copied disk, and its run would run beside the rerun. The lease is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_on_a_process_whose_stream_is_open_waits() {
    let job = Job::new("its first copy still runs", &[]);
    let (cell, _ops, _a, mut b, _first) = running_on_a(&job).await;
    let _a2 = come_back(&cell, restarted("node-a", "i2", "i1")).await;
    assert_eq!(
        start_within_fast(&mut b).await,
        None,
        "rerun beside a live run"
    );
}

/// Catches any non-empty predecessor honoured (the mutant "trust the claim"): a
/// daemon naming a process that is not the one the node's last stream came from is
/// not taken at its word.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_on_another_process_waits() {
    let job = Job::new("the wrong predecessor", &[]);
    let (cell, _ops, a, mut b, _first) = running_on_a(&job).await;
    a.close();
    tokio::time::sleep(CLOSING).await;
    let _a2 = come_back(&cell, restarted("node-a", "i2", "i0")).await;
    assert_eq!(
        start_within_fast(&mut b).await,
        None,
        "rerun on a wrong claim"
    );
}

/// Catches a lost `Result` answered as the farm's failure at once (buck2 never retries
/// INTERNAL), and the farm's answer once the reruns are used up that does not name the
/// last node, the cause and the runs. Each node fences its run in turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_lost_runs_answer_internal_naming_the_runs() {
    let cell = Cell::start().await;
    let mut daemons = Vec::new();
    for node in ["node-a", "node-b", "node-c"] {
        daemons.push(cell.daemon(node, 4, 8).await);
    }
    let job = Job::new("fenced everywhere", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    for daemon in &mut daemons {
        let start = daemon.start().await;
        let ack = daemon
            .report(lost(start.lease_id, worker::Lost::Fenced))
            .await;
        assert!(ack.accepted, "a lost run refused: {ack:?}");
    }
    let status = response(&done(&mut ops).await).status.expect("a status");
    assert_eq!(status.code, Code::Internal as i32, "{status:?}");
    assert_eq!(
        status.message,
        "kbf farm fault on node-c: the node lost contact with the server and fenced the \
         run, and its reruns are used up. Operator fix: check the node's network path to \
         the server. Runs: 3 (node-a, fenced; node-b, fenced; node-c, fenced)."
    );
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
}

/// Catches a budget used up on the heartbeat path that answers nobody (the mutant
/// "heartbeat stays quiet": the server's answer is dropped, and the client waits for
/// ever): two runs fenced, the third lost to a proven restart, found by the restarted
/// daemon's first heartbeat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_past_the_budget_answers_the_client() {
    let job = Job::new("lost three ways", &[]);
    let (cell, mut ops, mut a, mut b, first) = running_on_a(&job).await;
    assert!(
        a.report(lost(first.lease_id, worker::Lost::Fenced))
            .await
            .accepted
    );
    let second = b.start().await;
    assert!(
        b.report(lost(second.lease_id, worker::Lost::Fenced))
            .await
            .accepted
    );
    // Both nodes are avoided now: the first that fits, `node-a`.
    let _third = a.start().await;
    a.close();
    tokio::time::sleep(CLOSING).await;
    let _a2 = come_back(&cell, restarted("node-a", "i2", "i1")).await;
    let status = response(&done(&mut ops).await).status.expect("a status");
    assert_eq!(status.code, Code::Internal as i32, "{status:?}");
    assert!(
        status.message.starts_with(
            "kbf farm fault on node-a: the daemon restarted while the action ran, and its \
             reruns are used up."
        ),
        "{status:?}"
    );
    assert!(
        status
            .message
            .ends_with("Runs: 3 (node-a, fenced; node-b, fenced; node-a, daemon restarted)."),
        "{status:?}"
    );
}

const MAC: &[(&str, &str)] = &[
    ("arch", "arm64"),
    ("os", "macos"),
    ("drivers", "native"),
    ("drivers", "native-whole-machine"),
];

const WHOLE: &[(&str, &str)] = &[("OSFamily", "macos"), ("kbf-lease", "whole_machine")];

/// The Mac's Hello from daemon process `instance`, naming `predecessor`, in `phase`.
fn mac(instance: &str, predecessor: &str, phase: Phase, lease: Option<LeaseId>) -> Hello {
    Hello {
        instance_id: instance.to_owned(),
        predecessor_instance_id: predecessor.to_owned(),
        phase: Some(NodePhase {
            phase: phase.into(),
            lease_id: lease,
            reason: String::new(),
        }),
        ..hello_on("mac-1", 12, 32, MAC)
    }
}

/// Catches a whole-machine restart mid-action answered INTERNAL (#459's "its result
/// is lost" taken as a plain failure), and a proven restart that gives up the
/// restoring lease before its `Start` is sent again (the mutant "requeue at
/// registration": the node is still being cleaned). The restarted daemon gets the
/// `Start` again, restores, reports the run lost, and is sent the action again once
/// it serves: the only node, so the rerun goes back to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_whole_machine_restart_mid_action_runs_the_action_again() {
    let cell = Cell::start().await;
    let mut before = FakeDaemon::connect(cell.worker_addr, mac("one", "", Phase::Serving, None))
        .await
        .expect("registered");
    let job = Job::new("a GPU test cut short", WHOLE);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = before.start().await;
    let lease = start.lease_id.expect("a lease id");
    before.send(daemon_message::Message::Hello(mac(
        "one",
        "",
        Phase::Preparing,
        Some(lease),
    )));
    assert!(before.heartbeat(&[lease]).await);
    before.close();
    tokio::time::sleep(CLOSING).await;

    let restoring = mac("two", "one", Phase::Restoring, Some(lease));
    let mut after = FakeDaemon::connect(cell.worker_addr, restoring)
        .await
        .expect("registered");
    let again = after.start().await;
    assert_eq!(
        again.lease_id,
        Some(lease),
        "the restoring lease was given up"
    );
    assert!(after.heartbeat(&[lease]).await);
    let ack = after
        .report(lost(Some(lease), worker::Lost::DaemonRestarted))
        .await;
    assert!(ack.accepted);
    after.send(daemon_message::Message::Hello(mac(
        "two",
        "one",
        Phase::Serving,
        None,
    )));
    let rerun = after.start().await;
    assert_ne!(rerun.lease_id, Some(lease));
    assert_eq!(rerun.action_digest, start.action_digest);
    let result = output(&cell, "the GPU test ran", 0).await;
    assert!(after.report(ran(rerun.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
}
