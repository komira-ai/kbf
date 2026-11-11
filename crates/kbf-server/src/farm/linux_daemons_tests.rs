//! The farm's placement under `--executor js-agent`: no lease for a Linux daemon, a Mac daemon
//! and the executor's virtual workers placed as before, and nothing left behind for a restart
//! under `--executor kbf-daemon`. Placement runs at a submit and at a tick, so each test reads
//! what the scheduler holds right after one; time is tokio's paused clock.

use std::sync::Arc;

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Dispatch as _, Submission};
use kbf_proto::worker::server_message;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{LeaseKind, Resources};

use super::super::durable::EpochPolicy;
use super::super::{
    DaemonInstance, Digest, Farm, NodeAction, OperationId, Reported, ServerMessage, StreamId,
    WorkerId, mpsc,
};
use crate::executor::ExecutorKind;
use crate::state_store::StateStore;

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

/// The drivers a Linux node's daemon runs, and a virtual worker lists.
const LINUX_DRIVERS: [&str; 2] = ["container", "native-whole-machine"];

/// A node report of `os` (none if empty) on x86_64, running `drivers`.
fn caps(os: &str, drivers: &[&str]) -> NodeCaps {
    let report: &[(&str, &str)] = if os.is_empty() {
        &[("arch", "x86_64")]
    } else {
        &[("arch", "x86_64"), ("os", os)]
    };
    NodeCaps::from_report(report.iter().copied())
        .expect("caps")
        .with_drivers(drivers.iter().copied())
}

/// What a node of four cores and 8 GiB running `drivers` on `os` reports.
fn report(os: &str, drivers: &[&str]) -> Reported {
    Reported::serving(Resources::new(4_000, 8 << 30), caps(os, drivers))
}

/// A farm with no state store, placing for `executor`.
fn farm(executor: ExecutorKind) -> Farm {
    Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_executor(executor)
}

/// A state store whose clock does not move.
fn store() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

/// A frontend's farm over `store`, placing for `executor`: what a restart of the frontend
/// builds.
fn over(store: &Arc<Fake>, executor: ExecutorKind) -> Farm {
    Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_executor(executor)
    .with_state(Some(StateStore {
        store: store.clone(),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a farm over the store")
}

/// Registers the daemon `name` with `reported`, as its stream's first `Hello` does.
fn daemon(farm: &Farm, name: &str, reported: Reported) -> (StreamId, Inbox) {
    let (outbound, inbox) = mpsc::unbounded_channel();
    let stream = farm.register(
        &WorkerId::new(name),
        DaemonInstance::new(name),
        DaemonInstance::new(""),
        reported,
        outbound,
        ServerMessage::default(),
    );
    (stream, inbox)
}

/// Registers the executor's virtual worker `name`, with the report a Linux node's would be.
fn virtual_worker(farm: &Farm, name: &str) -> Inbox {
    let (outbound, inbox) = mpsc::unbounded_channel();
    farm.register_virtual(
        &WorkerId::new(name),
        DaemonInstance::new(name),
        DaemonInstance::new(""),
        report("linux", &LINUX_DRIVERS),
        outbound,
        ServerMessage::default(),
    );
    inbox
}

/// The request for the action `key` of `kind`, of one core and 1 GiB, needing `os` (anything
/// if empty).
fn request(key: u8, kind: LeaseKind, os: &str) -> kbf_sched::Request {
    let needs = if os.is_empty() {
        kbf_caps::Request::default()
    } else {
        kbf_caps::Request::from_platform([("OSFamily", os)]).expect("platform")
    };
    kbf_sched::Request {
        key: kbf_types::ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(kbf_types::DigestFunction::Sha256, [key; 32], 1),
        }
        .into(),
        qos: kbf_types::Qos::Ci,
        kind,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs,
        whole_worker: false,
        pin: None,
    }
}

/// Submits `request` to `farm` and returns the operation it is queued as.
fn submit(farm: &Farm, request: kbf_sched::Request) -> OperationId {
    let operation = farm.lock().sched.next_operation();
    farm.submit(Submission {
        request,
        call: None,
        described: kbf_front::Described::default(),
        evidence: Default::default(),
        max_wall: None,
    })
    .expect("submitted");
    operation
}

/// The worker that holds the lease of `operation`, if one does.
fn holder(farm: &Farm, operation: OperationId) -> Option<WorkerId> {
    let views = farm.lease_views(None);
    let mut holders = views.leases.iter().filter(|l| l.operation == operation);
    let found = holders.next().map(|l| l.node.clone());
    assert!(holders.next().is_none(), "{operation:?} holds two leases");
    found
}

/// How many `Start`s are waiting in `inbox`.
fn starts(inbox: &mut Inbox) -> usize {
    let messages = std::iter::from_fn(|| inbox.try_recv().ok());
    let start = |message: &Result<ServerMessage, tonic::Status>| {
        let message = message.as_ref().expect("no error").message.as_ref();
        matches!(message, Some(server_message::Message::Start(_)))
    };
    messages.filter(start).count()
}

/// The `cordon/` and `cordon-owner/` rows in `store`.
fn cordon_rows(store: &Arc<Fake>) -> Vec<String> {
    let rows = |prefix| store.scan_prefix(prefix).expect("scan");
    let mut keys: Vec<String> = rows("cordon/").into_iter().map(|(key, _)| key).collect();
    keys.extend(rows("cordon-owner/").into_iter().map(|(key, _)| key));
    keys
}

/// The drivers of each daemon the session rows of `store` list, as another frontend of the
/// store reads them: the frontend has no node and an operation of its own waits (for macOS,
/// which no node here runs), so its intake poll reads the rows.
async fn session_drivers(store: &Arc<Fake>) -> Vec<Vec<String>> {
    let other = over(store, ExecutorKind::KbfDaemon);
    submit(&other, request(9, LeaseKind::Action, "macos"));
    other.tick().await;
    other.intake();
    let state = other.lock();
    let remote = state.engine.intake.remote.iter();
    remote
        .map(|(caps, _)| caps.drivers.iter().cloned().collect())
        .collect()
}

/// Catches a Linux daemon offered a lease while the executor runs: an action and a
/// whole-machine lease, each of which the daemon could run, are not placed on it when it
/// registers, when it resends its `Hello`, or when an operator uncordons it (the filter is
/// not a cordon an uncordon lifts); the farm counts the request as one no live worker can
/// run, and places it on a virtual worker once one registers.
#[tokio::test(start_paused = true)]
async fn linux_daemons_get_no_lease_while_the_executor_runs() {
    for kind in [LeaseKind::Action, LeaseKind::WholeMachine] {
        let farm = farm(ExecutorKind::JsAgent);
        let (stream, mut inbox) = daemon(&farm, "linux-a", report("linux", &LINUX_DRIVERS));
        let operation = submit(&farm, request(1, kind, ""));
        assert_eq!(
            holder(&farm, operation),
            None,
            "{kind:?}: placed at registration"
        );
        let waiting = farm.lock().sched.waiting(operation).map(str::to_owned);
        assert!(waiting.is_some(), "{kind:?}: a live worker could run it");

        let worker = WorkerId::new("linux-a");
        farm.resize(&worker, stream, report("linux", &LINUX_DRIVERS));
        farm.place(&worker, NodeAction::Uncordon)
            .expect("uncordoned");
        farm.tick().await;
        assert_eq!(
            holder(&farm, operation),
            None,
            "{kind:?}: placed after a resent Hello"
        );
        assert_eq!(starts(&mut inbox), 0, "{kind:?}: a Start to the daemon");

        let mut placed = virtual_worker(&farm, "js:fe-0");
        farm.tick().await;
        assert_eq!(
            holder(&farm, operation),
            Some(WorkerId::new("js:fe-0")),
            "{kind:?}"
        );
        assert_eq!(starts(&mut placed), 1, "{kind:?}");
        assert_eq!(starts(&mut inbox), 0, "{kind:?}: a Start to the daemon");
    }
}

/// Catches a filter that takes more than the Linux daemons, or fewer: under the executor, an
/// action for macOS is placed on the Mac daemon, and five for Linux fill the virtual worker's
/// four cores and the fifth waits, while the Linux daemon beside them, with room for four
/// more, is given none.
#[tokio::test(start_paused = true)]
async fn a_mac_daemon_is_placed_beside_the_virtual_workers() {
    let farm = farm(ExecutorKind::JsAgent);
    let (_, mut mac) = daemon(&farm, "mac-a", report("macos", &["native"]));
    let (_, mut linux) = daemon(&farm, "linux-a", report("linux", &LINUX_DRIVERS));
    let mut placed = virtual_worker(&farm, "js:fe-0");

    let for_mac = submit(&farm, request(1, LeaseKind::Action, "macos"));
    let for_linux: Vec<OperationId> = (2..7)
        .map(|key| submit(&farm, request(key, LeaseKind::Action, "linux")))
        .collect();
    farm.tick().await;

    assert_eq!(holder(&farm, for_mac), Some(WorkerId::new("mac-a")));
    let holders: Vec<Option<WorkerId>> = for_linux.iter().map(|op| holder(&farm, *op)).collect();
    let fe = Some(WorkerId::new("js:fe-0"));
    assert_eq!(holders, [fe.clone(), fe.clone(), fe.clone(), fe, None]);
    assert_eq!(
        (starts(&mut mac), starts(&mut placed), starts(&mut linux)),
        (1, 4, 0)
    );
}

/// Catches the filter taking the executor's own workers: a virtual worker that resends its
/// report (as it does when it begins to drain, here with the node still serving) keeps its
/// drivers, and the next action is placed on it.
#[tokio::test(start_paused = true)]
async fn a_virtual_worker_that_resends_its_report_is_still_placed_on() {
    let farm = farm(ExecutorKind::JsAgent);
    let (outbound, mut placed) = mpsc::unbounded_channel();
    let worker = WorkerId::new("js:fe-0");
    let stream = farm.register_virtual(
        &worker,
        DaemonInstance::new("js:fe-0"),
        DaemonInstance::new(""),
        report("linux", &LINUX_DRIVERS),
        outbound,
        ServerMessage::default(),
    );
    farm.resize_virtual(&worker, stream, report("linux", &LINUX_DRIVERS));

    let operation = submit(&farm, request(1, LeaseKind::Action, ""));

    assert_eq!(holder(&farm, operation), Some(worker));
    assert_eq!(starts(&mut placed), 1);
}

/// Catches a filter wider than `os=linux`: a daemon that reports another `os`, or none, is
/// placed on under the executor as before.
#[tokio::test(start_paused = true)]
async fn a_daemon_reporting_another_os_or_none_is_placed_under_the_executor() {
    for os in ["freebsd", ""] {
        let farm = farm(ExecutorKind::JsAgent);
        let (_, mut inbox) = daemon(&farm, "node-a", report(os, &["container"]));
        let operation = submit(&farm, request(1, LeaseKind::Action, ""));
        assert_eq!(
            holder(&farm, operation),
            Some(WorkerId::new("node-a")),
            "{os:?}"
        );
        assert_eq!(starts(&mut inbox), 1, "{os:?}");
    }
}

/// Catches the filter left behind by the executor: a frontend over a state store, under the
/// executor, places nothing on the Linux daemon, writes no `cordon/` or `cordon-owner/` row
/// for it, and its session row lists no driver, so no other frontend of the store counts it
/// as able to run anything. Restarted under `--executor kbf-daemon` over the same store (the
/// back-out), the frontend places the queued action on the daemon when it registers again,
/// its session row lists its drivers, and there is still no cordon row.
#[tokio::test(start_paused = true)]
async fn a_restart_under_kbf_daemon_places_the_linux_daemons_and_leaves_no_cordon_row() {
    let store = store();
    let operation = {
        let first = over(&store, ExecutorKind::JsAgent);
        let (_, mut inbox) = daemon(&first, "linux-a", report("linux", &LINUX_DRIVERS));
        let operation = submit(&first, request(1, LeaseKind::Action, ""));
        first.tick().await;
        assert_eq!(holder(&first, operation), None);
        assert_eq!(starts(&mut inbox), 0);
        operation
    };
    assert_eq!(session_drivers(&store).await, [Vec::<String>::new()]);
    assert_eq!(cordon_rows(&store), Vec::<String>::new());

    let second = over(&store, ExecutorKind::KbfDaemon);
    let (_, mut inbox) = daemon(&second, "linux-a", report("linux", &LINUX_DRIVERS));
    second.tick().await;

    assert_eq!(holder(&second, operation), Some(WorkerId::new("linux-a")));
    assert_eq!(starts(&mut inbox), 1);
    assert_eq!(
        session_drivers(&store).await,
        [LINUX_DRIVERS.map(str::to_owned).to_vec()]
    );
    assert_eq!(cordon_rows(&store), Vec::<String>::new());
}

/// Catches the switch cutting work a Linux daemon was given before it: a lease the daemon
/// holds when the frontend restarts under the executor is taken up when the daemon registers
/// again, stays the daemon's when its heartbeat lists it, and is not placed again on the
/// virtual worker.
#[tokio::test(start_paused = true)]
async fn a_lease_a_linux_daemon_held_before_the_switch_stays_its_own() {
    let store = store();
    let (operation, lease) = {
        let first = over(&store, ExecutorKind::KbfDaemon);
        let (_, mut inbox) = daemon(&first, "linux-a", report("linux", &LINUX_DRIVERS));
        let operation = submit(&first, request(1, LeaseKind::Action, ""));
        assert_eq!(starts(&mut inbox), 1);
        let views = first.lease_views(None);
        let lease = views.leases.first().expect("a lease").lease;
        (operation, lease)
    };

    let second = over(&store, ExecutorKind::JsAgent);
    let worker = WorkerId::new("linux-a");
    let (stream, mut inbox) = daemon(&second, "linux-a", report("linux", &LINUX_DRIVERS));
    let mut placed = virtual_worker(&second, "js:fe-0");
    assert!(second.heartbeat(&worker, stream, 1, vec![lease]).await);
    second.tick().await;

    assert_eq!(holder(&second, operation), Some(worker));
    assert_eq!((starts(&mut inbox), starts(&mut placed)), (0, 0));
}
