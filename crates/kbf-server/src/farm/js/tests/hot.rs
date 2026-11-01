//! Tests of the farm's hot path on the js core (`super::super::hot`). A scenario that has an
//! observable the legacy farm also gives is run on a legacy farm and a js farm alike, and the
//! two are required to answer alike; the rest assert what the legacy suites assert of the
//! same behaviour (`tests/action_usage_log.rs`, `tests/execute/memory.rs`,
//! `tests/execute/invocations.rs`, `tests/sched_gauges.rs`).

use std::sync::Arc;

use kbf_caps::NodeCaps;
use kbf_front::{
    Backend, Cache, CallMetadata, Dispatch as _, ExecuteCall, Finished, Recorder, Stage,
    Submission, Ticket,
};
use kbf_invocation::{InvocationId, RecordKey};
use kbf_proto::google::rpc;
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::server_message::Message;
use kbf_proto::worker::{self, CgroupUsage, MemoryKill, ResourceUsage, ServerMessage, Welcome};
use kbf_sched::{DaemonInstance, FINISHED_RETENTION, UNSERVABLE_WAIT};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::mpsc;
use tonic::{Code, Status};

use crate::SchedulerCore;
use crate::farm::{Farm, Reported, StreamId};
use crate::test_log::capture;

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, Status>>;

/// A node's stream as a test holds it: dropping the inbox ends the stream.
pub(super) struct Joined {
    pub(super) stream: StreamId,
    pub(super) inbox: Inbox,
}

impl Joined {
    /// The messages sent on the stream since the last call.
    pub(super) fn sent(&mut self) -> Vec<Message> {
        let mut sent = Vec::new();
        while let Ok(Ok(ServerMessage {
            message: Some(message),
        })) = self.inbox.try_recv()
        {
            sent.push(message);
        }
        sent
    }

    /// The lease of the `Start` sent since the last call, if one was.
    pub(super) fn started(&mut self) -> Option<worker::LeaseId> {
        self.sent().into_iter().find_map(|message| match message {
            Message::Start(start) => start.lease_id,
            _ => None,
        })
    }
}

pub(super) fn farm_on(core: SchedulerCore, cache: &Backend) -> Farm {
    Farm::on_core(core, cache.clone(), UNSERVABLE_WAIT, FINISHED_RETENTION)
}

pub(super) fn backend() -> Backend {
    Arc::new(Cache::memory()).into()
}

pub(super) fn node(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn hello(os: &str, arch: &str) -> Reported {
    let caps = NodeCaps::from_report([("arch", arch), ("os", os)])
        .expect("caps")
        .with_drivers(["container"]);
    Reported::serving(Resources::new(4_000, 8 << 30), caps)
}

pub(super) fn linux() -> Reported {
    hello("linux", "x86_64")
}

/// `name` joined to `farm` on a new stream.
pub(super) fn join(farm: &Farm, name: &str, reported: Reported) -> Joined {
    let (outbound, inbox) = mpsc::unbounded_channel();
    let welcome = ServerMessage {
        message: Some(Message::Welcome(Welcome::default())),
    };
    let stream = farm.register(
        &node(name),
        DaemonInstance::new(format!("daemon-{name}")),
        DaemonInstance::new(""),
        reported,
        outbound,
        welcome,
    );
    Joined { stream, inbox }
}

pub(super) fn request(tag: u8) -> kbf_sched::Request {
    let key = ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
    };
    kbf_sched::Request {
        key: key.into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
        whole_worker: false,
    }
}

pub(super) fn submission(tag: u8, call: Option<ExecuteCall>) -> Submission {
    Submission {
        request: request(tag),
        call,
        described: kbf_front::Described::default(),
        evidence: Default::default(),
        max_wall: None,
    }
}

fn submit(farm: &Farm, tag: u8) -> Ticket {
    farm.submit(submission(tag, None)).expect("queued")
}

/// The `Result` of a run that exited 0, carrying `usage` the way a daemon sends it.
pub(super) fn ok(lease: &worker::LeaseId, usage: Option<&ResourceUsage>) -> worker::Result {
    let execution_metadata = usage.map(|usage| ExecutedActionMetadata {
        auxiliary_metadata: vec![usage.to_any()],
        ..ExecutedActionMetadata::default()
    });
    worker::Result {
        lease_id: Some(*lease),
        action_result: Some(ActionResult {
            exit_code: 0,
            execution_metadata,
            ..ActionResult::default()
        }),
        ..worker::Result::default()
    }
}

/// A `Result` for `lease` killed for memory as `kill` says, with the status a driver sends.
pub(super) fn killed(lease: &worker::LeaseId, kill: MemoryKill) -> worker::Result {
    worker::Result {
        lease_id: Some(*lease),
        status: Some(rpc::Status {
            code: Code::ResourceExhausted as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: kill as i32,
        ..worker::Result::default()
    }
}

pub(super) fn usage() -> ResourceUsage {
    ResourceUsage {
        wall_micros: 2_000_000,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(3_000_000),
            cpu_user_micros: Some(2_000_000),
            cpu_system_micros: Some(1_000_000),
            cpu_periods: Some(0),
            cpu_throttled_periods: Some(0),
            cpu_throttled_micros: Some(0),
            memory_peak_bytes: Some(4096),
            ..CgroupUsage::default()
        }),
        ..ResourceUsage::default()
    }
}

fn gauges(cache: &Backend) -> (u64, u64) {
    let metrics = cache.metrics();
    (metrics.sched_queued.get(), metrics.sched_leases.get())
}

/// Asserts that a second `Hello` of a node makes its stream the newest, that a heartbeat and
/// a resent `Hello` on the replaced stream change nothing, and that the newest stream's
/// resent `Hello` changes the node's `os` and `arch`; the legacy farm gives the same
/// answers. Catches: a replaced stream's heartbeat acknowledged or heard, its `Hello`
/// taken, or the newest stream's resent `Hello` not recorded.
#[tokio::test(start_paused = true)]
async fn a_replaced_streams_heartbeat_and_hello_are_ignored_and_the_newest_streams_resize_is_taken()
{
    fn look(farm: &Farm) -> (String, &'static str) {
        let looks = farm.update_look();
        let node = looks.first().expect("the node");
        (node.os.clone(), node.arch)
    }
    async fn scenario(core: SchedulerCore) -> (Vec<bool>, Vec<(String, &'static str)>) {
        let farm = farm_on(core, &backend());
        let name = node("n1");
        let old = join(&farm, "n1", linux());
        let new = join(&farm, "n1", linux());
        assert_ne!(old.stream, new.stream);
        let beats = vec![
            farm.heartbeat(&name, old.stream, 1, Vec::new()).await,
            farm.heartbeat(&name, new.stream, 1, Vec::new()).await,
        ];
        let mut looks = vec![look(&farm)];
        farm.resize(&name, old.stream, hello("macos", "arm64"));
        looks.push(look(&farm));
        farm.resize(&name, new.stream, hello("macos", "arm64"));
        looks.push(look(&farm));
        (beats, looks)
    }
    let legacy = scenario(SchedulerCore::Legacy).await;
    let js = scenario(SchedulerCore::Js).await;
    assert_eq!(js, legacy);
    assert!(
        !js.0[0],
        "the replaced stream's heartbeat is not acknowledged"
    );
    assert_eq!(
        js.1,
        [
            ("linux".to_owned(), "x86_64"),
            ("linux".to_owned(), "x86_64"),
            ("macos".to_owned(), "arm64"),
        ]
    );
}

/// Asserts the queue and lease gauges of `/metrics` follow the scheduler as the legacy farm
/// does: an operation queued with no node is on the queue gauge after the submit, a node
/// that takes it moves it to the lease gauge by the next tick, and the lease ending on a
/// result clears the lease gauge by the next tick (the lease ends on a result, which is not
/// a submit); the `Result` is acknowledged as accepted and its caller is told the run's exit
/// code. Catches: a gauge never published by the js arms, one published only by the submit,
/// a `Result` the farm does not take, and a caller never answered.
#[tokio::test(start_paused = true)]
async fn the_queue_and_lease_gauges_follow_a_submit_a_lease_and_its_result() {
    async fn scenario(core: SchedulerCore) -> (Vec<(u64, u64)>, (bool, i32), bool) {
        let cache = backend();
        let farm = farm_on(core, &cache);
        let ticket = submit(&farm, 1);
        let mut seen = vec![gauges(&cache)];
        let mut node_one = join(&farm, "n1", linux());
        farm.tick().await;
        seen.push(gauges(&cache));
        let lease = node_one.started().expect("a Start");
        let ack = farm
            .report(&node("n1"), ok(&lease, None))
            .await
            .expect("an ack");
        farm.tick().await;
        seen.push(gauges(&cache));
        let exit = match &*ticket.stage.borrow() {
            Stage::Done(Finished::Ran(result)) => result.exit_code,
            other => panic!("not answered: {other:?}"),
        };
        let known = farm.wait(&ticket.name).is_some();
        (seen, (ack.accepted, exit), known)
    }
    let legacy = scenario(SchedulerCore::Legacy).await;
    let js = scenario(SchedulerCore::Js).await;
    assert_eq!(js.0, legacy.0);
    assert_eq!(js.0, [(1, 0), (0, 1), (0, 0)]);
    assert_eq!(js.1, (true, 0));
    assert!(js.2, "a finished operation is kept for its retention");
}

/// Asserts the `action usage:` line of a report the scheduler took, once, naming the node and
/// how the lease ended, and none for a copy of it that is refused. Catches: no line (the
/// report's measurement lost between the host and the hook), a line for a refused copy, and
/// two lines for one report.
#[tokio::test(start_paused = true)]
async fn an_accepted_report_logs_one_action_usage_line_and_its_copy_none() {
    let log = capture();
    let farm = farm_on(SchedulerCore::Js, &backend());
    let mut joined = join(&farm, "usage-node", linux());
    let _ticket = submit(&farm, 2);
    let lease = joined.started().expect("a Start");
    let result = ok(&lease, Some(&usage()));

    let first = farm
        .report(&node("usage-node"), result.clone())
        .await
        .expect("an ack");
    let copy = farm
        .report(&node("usage-node"), result)
        .await
        .expect("an ack");

    assert!(first.accepted);
    assert!(!copy.accepted, "{copy:?}");
    let text = log.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("action usage:"))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    for part in [
        "exit_code=0",
        "wall_us=2000000",
        "outcome=ok",
        "node=usage-node",
    ] {
        assert!(lines[0].contains(part), "{part}: {}", lines[0]);
    }
}

/// Asserts a run killed for memory is told to the invocation record with the booking of the
/// kill (the memory booked and, for a kill at its own limit, its cap), as the legacy farm
/// tells it. Catches: the `Ran` event never sent from the js hook, or sent without the kill's
/// booking.
#[tokio::test(start_paused = true)]
async fn a_run_killed_for_memory_is_recorded_with_its_booking() {
    async fn scenario(core: SchedulerCore) -> (Option<u64>, Option<u64>) {
        let (recorder, recording) = Recorder::new(1 << 20, 64);
        tokio::spawn(recording.run());
        let farm = farm_on(core, &backend()).recording(recorder.clone());
        let mut joined = join(&farm, "oom-node", linux());
        let metadata = CallMetadata {
            invocation_id: kbf_front::InvocationId::parse("build-1"),
            ..Default::default()
        };
        let action = ActionKey::try_from(&request(3).key)
            .expect("an action")
            .action;
        let call = ExecuteCall::new(
            &recorder,
            &metadata,
            &Default::default(),
            "main",
            action,
            action,
        )
        .expect("a recorded call");
        let _ticket = farm.submit(submission(3, Some(call))).expect("queued");
        let lease = joined.started().expect("a Start");

        let ack = farm
            .report(&node("oom-node"), killed(&lease, MemoryKill::OwnLimit))
            .await
            .expect("an ack");
        assert!(ack.accepted, "{ack:?}");

        recorder.caught_up().await;
        let key = RecordKey::owner(InvocationId::new("build-1"));
        let record = recorder.get(&key).expect("a record");
        let row = record.row(&action).expect("a row");
        let run = row.attempts[0].run.as_ref().expect("the killed run");
        (run.booked_memory_bytes, run.cap_bytes)
    }
    let legacy = scenario(SchedulerCore::Legacy).await;
    let js = scenario(SchedulerCore::Js).await;
    assert_eq!(js, legacy);
    assert!(js.0.is_some(), "the booking of the kill: {js:?}");
    assert!(js.1.is_some(), "the cap of an own-limit kill: {js:?}");
}

/// Asserts a busy node's memory kills are each logged for operators with the node's running
/// count, and the caller is told the farm's fault once the reruns are used up (the case of
/// `tests/execute/memory.rs`). Catches: no pressure line from the js hook, a count that does
/// not rise, or the line read before the scheduler counted the kill.
#[tokio::test(start_paused = true)]
async fn a_busy_nodes_kills_are_logged_with_their_count_and_the_reruns_run_out() {
    let log = capture();
    let farm = farm_on(SchedulerCore::Js, &backend());
    let mut joined = join(&farm, "busy-node", linux());
    let ticket = submit(&farm, 4);
    let mut lease = joined.started().expect("a Start");
    for kills in 1..=3_u64 {
        let ack = farm
            .report(&node("busy-node"), killed(&lease, MemoryKill::NodePressure))
            .await
            .expect("an ack");
        assert!(ack.accepted, "kill {kills}: {ack:?}");
        if kills < 3 {
            lease = rerun_start(&farm, &mut joined).await;
        }
    }
    let text = log.text();
    for kills in 1..=3 {
        let count = format!("kills={kills}");
        let logged: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("node=busy-node") && l.contains(&count))
            .filter(|l| l.contains("node memory pressure"))
            .collect();
        assert_eq!(logged.len(), 1, "kill {kills}: {logged:?}");
    }
    match &*ticket.stage.borrow() {
        Stage::Done(Finished::Failed(status)) => {
            assert_eq!(status.code, Code::Internal as i32, "{status:?}");
        }
        other => panic!("not answered with the farm's fault: {other:?}"),
    }
}

/// Asserts a farm is quiet until a submit is between its request and its queueing (the
/// gauge the front raises), and quiet again after, as the legacy farm is. Catches: `quiet`
/// that ignores the front's in-flight gauge, or one that never reads true.
#[tokio::test(start_paused = true)]
async fn the_farm_is_quiet_only_when_no_submit_is_between_its_request_and_its_queueing() {
    fn scenario(core: SchedulerCore) -> Vec<bool> {
        let cache = backend();
        let farm = farm_on(core, &cache);
        let mut seen = vec![farm.quiet()];
        {
            let _presubmit = cache.metrics().execute_presubmit_in_flight.raise();
            seen.push(farm.quiet());
        }
        seen.push(farm.quiet());
        seen
    }
    assert_eq!(scenario(SchedulerCore::Js), scenario(SchedulerCore::Legacy));
    assert_eq!(scenario(SchedulerCore::Js), [true, false, true]);
}

/// Asserts that ending the worker streams counts only the streams still open and sends each
/// the status, as the legacy farm does. Catches: a count of closed streams, or a stream
/// never told.
#[tokio::test(start_paused = true)]
async fn ending_the_worker_streams_tells_the_open_ones_and_counts_them() {
    fn scenario(core: SchedulerCore) -> (usize, Option<Code>) {
        let farm = farm_on(core, &backend());
        let mut open = join(&farm, "open", linux());
        let closed = join(&farm, "closed", linux());
        drop(closed.inbox);
        let ended = farm.end_worker_streams(&Status::unavailable("going away"));
        let told = std::iter::from_fn(|| open.inbox.try_recv().ok())
            .find_map(Result::err)
            .map(|status| status.code());
        (ended, told)
    }
    let js = scenario(SchedulerCore::Js);
    assert_eq!(js, scenario(SchedulerCore::Legacy));
    assert_eq!(js, (1, Some(Code::Unavailable)));
}

/// Asserts the name a js farm gives is `operations/<id>`, that it is answered while the
/// operation is held, and that a name that is not of that form, or names no operation, is
/// not found. Catches: a name of the old per-process form, a lookup that panics on a bad
/// name, and a held operation that cannot be waited for.
#[tokio::test(start_paused = true)]
async fn an_operation_is_named_by_its_id_and_a_name_that_is_not_one_is_not_found() {
    let farm = farm_on(SchedulerCore::Js, &backend());
    let ticket = submit(&farm, 5);
    let id = ticket
        .name
        .strip_prefix("operations/")
        .expect("the name form");
    assert!(id.chars().all(|c| c.is_ascii_digit()), "{}", ticket.name);
    let again = farm.wait(&ticket.name).expect("a held operation");
    assert_eq!(again.action, ticket.action);
    assert!(farm.wait("operations/not-a-number").is_none());
    assert!(farm.wait("operations/9999999").is_none());
    assert!(farm.wait("somewhere/else").is_none());
}

/// The `Start` of the rerun after a kill: the scheduler places it at the next tick, so the
/// clock is moved on (paused, so no real time passes) until one is sent.
pub(super) async fn rerun_start(farm: &Farm, joined: &mut Joined) -> worker::LeaseId {
    for _ in 0..30 {
        farm.tick().await;
        if let Some(lease) = joined.started() {
            return lease;
        }
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
    }
    panic!("no rerun was started");
}

/// Asserts which acknowledgement the js farm's report keeps from the core's: a stale
/// refusal of a lease the core still calls current is dropped (the daemon keeps that
/// `Result` and sends it again), and every other answer stands: an acceptance, a refusal
/// while the farm is fenced (which refuses every `Result` as stale by design), a stale
/// refusal of a lease that is not current, and a refusal for another reason. Catches a
/// check that drops the wrong acks, or that asks whether the lease is current when the
/// ack alone decides.
#[test]
fn only_a_stale_refusal_of_the_current_lease_loses_its_ack() {
    use kbf_proto::worker::{ResultAck, ResultAckReason};
    use kbf_types::LeaseId;

    use super::super::hot::answered;

    let lease = LeaseId::new(1, 1);
    let worker = node("n1");
    let ack = |accepted, reason: ResultAckReason| ResultAck {
        lease_id: Some(worker::LeaseId { term: 1, seq: 1 }),
        accepted,
        reason: reason as i32,
    };
    let unasked = || -> bool { panic!("the ack alone decides") };
    let stale = ack(false, ResultAckReason::Stale);

    assert_eq!(answered(stale, false, &|| true, lease, &worker), None);
    let accepted = ack(true, ResultAckReason::Accepted);
    assert_eq!(
        answered(accepted, false, &unasked, lease, &worker),
        Some(accepted)
    );
    assert_eq!(answered(stale, true, &unasked, lease, &worker), Some(stale));
    assert_eq!(
        answered(stale, false, &|| false, lease, &worker),
        Some(stale)
    );
    let unknown = ack(false, ResultAckReason::UnknownLease);
    assert_eq!(
        answered(unknown, false, &unasked, lease, &worker),
        Some(unknown)
    );
}
