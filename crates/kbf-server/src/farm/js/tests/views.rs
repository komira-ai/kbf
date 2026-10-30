//! Tests of the farm's views and counters on the js core (`super::super::views`). Most steps
//! are taken on a legacy farm and a js farm alike, and the two are required to answer alike;
//! what cannot be compared (lease ids carry the farm's term, times carry the farm's start)
//! is asserted against the value the legacy farm gives for it.
//!
//! A node joins a js farm through the farm's calls on the core ([`dispatch`]) and the facts
//! the hot path records ([`KbfAdapter::note_registered`]), as it does a legacy farm through
//! `Farm::register`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Dispatch as _, Submission, Ticket};
use kbf_proto::update::UpdateStatus;
use kbf_proto::update::update_status::State;
use kbf_proto::worker::{
    NodeStatus, PackReadCounts, PackWriteCounts, ServerMessage, XcodeState, XcodeStatus,
};
use kbf_sched::{DaemonInstance, FINISHED_RETENTION, UNSERVABLE_WAIT, WholeNodeConfig};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::mpsc;
use tonic::Status;

use crate::SchedulerCore;
use crate::action_usage::Shapes;
use crate::farm::driver::Driver;
use crate::farm::{EpochPolicy, Farm, NodeUnread, Reported, StreamId};
use crate::farm_js::dispatch;
use crate::farm_js::host::{Connection, stream_number};
use crate::state_store::StateStore;

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, Status>>;

/// A node's stream as a test holds it: dropping the inbox ends the stream.
struct Joined {
    stream: StreamId,
    inbox: Inbox,
}

fn farm_on(core: SchedulerCore) -> Farm {
    Farm::on_core(
        core,
        Arc::new(Cache::memory()).into(),
        UNSERVABLE_WAIT,
        FINISHED_RETENTION,
    )
}

/// A legacy farm and a js farm, empty.
fn pair() -> (Farm, Farm) {
    (farm_on(SchedulerCore::Legacy), farm_on(SchedulerCore::Js))
}

fn worker(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// What a node of `os` and `arch` reports of itself in its `Hello`, with room for four
/// one-core leases.
fn hello(os: &str, arch: &str, protocol: Option<u32>) -> Reported {
    let caps = NodeCaps::from_report([("arch", arch), ("os", os)])
        .expect("caps")
        .with_drivers(["container"]);
    Reported {
        protocol,
        ..Reported::serving(Resources::new(4_000, 8 << 30), caps)
    }
}

fn linux(protocol: Option<u32>) -> Reported {
    hello("linux", "x86_64", protocol)
}

fn mac(protocol: Option<u32>) -> Reported {
    hello("macos", "arm64", protocol)
}

/// `name` joined to `farm` on a new stream.
fn join(farm: &Farm, name: &str, reported: Reported) -> Joined {
    let node = worker(name);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let welcome = ServerMessage::default();
    let instance = DaemonInstance::new(format!("daemon-{name}"));
    let predecessor = DaemonInstance::new("");
    let stream = match &farm.driver {
        Driver::Js(js) => {
            let connection = Connection { outbound, welcome };
            let stream = dispatch::register(
                &js.core,
                js.now(),
                &node,
                instance,
                predecessor,
                reported.clone(),
                connection,
            )
            .expect("a host the core hears");
            js.core
                .with(|_, adapter| adapter.note_registered(&node, &reported));
            StreamId(stream_number(stream).expect("a numbered stream"))
        }
        Driver::Legacy(_) => {
            farm.register(&node, instance, predecessor, reported, outbound, welcome)
        }
    };
    Joined { stream, inbox }
}

/// `name` joined to both farms, in the order of `farms`.
fn join_both(farms: [&Farm; 2], name: &str, reported: &Reported) -> [Joined; 2] {
    farms.map(|farm| join(farm, name, reported.clone()))
}

/// The action `tag` names, asking for one core and one GiB.
fn submission(tag: u8) -> Submission {
    let key = ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
    };
    let request = kbf_sched::Request {
        key: key.into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
        whole_worker: false,
    };
    Submission {
        request,
        call: None,
        described: kbf_front::Described::default(),
        evidence: Default::default(),
        max_wall: None,
    }
}

/// Submits the action `tag` names to `farm`. The caller keeps the ticket for as long as the
/// operation is to be waited on.
fn submit(farm: &Farm, tag: u8) -> Ticket {
    match &farm.driver {
        Driver::Js(js) => {
            let shapes = Shapes::default();
            dispatch::submit(&js.core, js.now(), &shapes, submission(tag)).expect("queued")
        }
        Driver::Legacy(_) => farm.submit(submission(tag)).expect("queued"),
    }
}

fn xcode_status(reason: &str) -> NodeStatus {
    NodeStatus {
        os_name: "macOS".to_owned(),
        xcodes: vec![XcodeStatus {
            app: "/Applications/Xcode_16.1.app".to_owned(),
            build: "16B40".to_owned(),
            state: XcodeState::LicenseNotAccepted.into(),
            reason: reason.to_owned(),
            fix: "sudo x -license accept".to_owned(),
        }],
        ..NodeStatus::default()
    }
}

fn update_status(running: &str, state: State) -> UpdateStatus {
    UpdateStatus {
        running_commit: running.to_owned(),
        state: state as i32,
        ..UpdateStatus::default()
    }
}

/// Asserts that the node list is the legacy farm's for the same nodes: in node-id order, each
/// with the protocol its stream spoke (the oldest and the count of unknown in the summary),
/// and that one node is listed alone as the legacy farm lists it, and an unknown one not at
/// all. Catches: the protocol read from the core's link, which has none, so that every node
/// is listed with an unknown one; or nodes listed in another order.
#[tokio::test(start_paused = true)]
async fn the_node_list_is_the_legacy_farms_with_the_protocol_each_stream_spoke() {
    let (legacy, js) = pair();
    let farms = [&legacy, &js];
    let _mac = join_both(farms, "mac-1", &mac(None));
    let _linux = join_both(farms, "linux-1", &linux(Some(3)));

    let listed = js.nodes();

    assert_eq!(listed, legacy.nodes());
    let ids: Vec<&str> = listed.nodes.iter().map(|n| n.node_id.as_str()).collect();
    assert_eq!(ids, ["linux-1", "mac-1"]);
    assert_eq!(listed.nodes[0].protocol, Some(3));
    assert_eq!(listed.nodes[1].protocol, None);
    assert_eq!(listed.protocols.oldest_protocol, Some(3));
    assert_eq!(listed.protocols.protocol_unknown, 1);
    assert!(listed.nodes.iter().all(|n| n.connected));
    assert_eq!(
        js.node_view(&worker("linux-1")),
        legacy.node_view(&worker("linux-1"))
    );
    assert_eq!(js.node_view(&worker("ghost")), None);
}

/// Asserts that a node whose stream ended is listed as not connected the next time anything
/// reads the nodes, by the list, the node and the update look alike, as on the legacy farm; the
/// node keeps its session, since a worker's link outlives its stream. Catches: a read that does
/// not first note the closed streams, which lists a gone node as connected.
#[tokio::test(start_paused = true)]
async fn a_node_whose_stream_ended_is_listed_as_disconnected_and_keeps_its_session() {
    let (legacy, js) = pair();
    let farms = [&legacy, &js];
    let gone = join_both(farms, "linux-1", &linux(Some(3)));
    let _kept = join_both(farms, "linux-2", &linux(Some(3)));
    drop(gone);

    let listed = js.nodes();

    assert_eq!(listed, legacy.nodes());
    assert!(!listed.nodes[0].connected);
    assert!(listed.nodes[1].connected);
    assert_eq!(js.update_look(), legacy.update_look());
    assert!(!js.update_look()[0].connected);
    assert_eq!(js.open_sessions(), 2);
    assert_eq!(js.open_sessions(), legacy.open_sessions());
    assert!(!js.node_view(&worker("linux-1")).expect("listed").connected);
}

/// Asserts that a node's detail is the legacy farm's for the same node: what it can run, what
/// is left after one lease, its booked resources and lease count; and that its lease is the one
/// the lease list shows. A node nobody has heard of is `None`, as on the legacy farm, with no
/// state store to ask. Catches: free not capacity less booked; the lease list of another node.
#[tokio::test(start_paused = true)]
async fn a_nodes_detail_shows_what_it_can_run_what_is_left_and_its_leases() {
    let (legacy, js) = pair();
    let farms = [&legacy, &js];
    let _node = join_both(farms, "linux-1", &linux(Some(3)));
    let _waiting = farms.map(|farm| submit(farm, 1));

    let read = js.node_detail(&worker("linux-1")).expect("readable");
    let want = legacy.node_detail(&worker("linux-1")).expect("readable");

    let (read, want) = (read.expect("a node"), want.expect("a node"));
    assert_eq!(read.capacity, want.capacity);
    assert_eq!(read.free, want.free);
    assert_eq!(read.node.booked, want.node.booked);
    assert_eq!(read.node.lease_count, 1);
    assert_eq!(want.node.lease_count, 1);
    assert_eq!(read.node.placement, want.node.placement);
    assert_eq!(read.node.phase, want.node.phase);
    assert_eq!(read.node.protocol, Some(3));
    let held: Vec<String> = js
        .lease_views(Some(&worker("linux-1")))
        .leases
        .iter()
        .map(|lease| lease.lease.to_string())
        .collect();
    assert_eq!(read.leases, held);
    assert_eq!(read.leases.len(), 1);
    assert_eq!(js.node_detail(&worker("ghost")), Ok(None));
    assert_eq!(legacy.node_detail(&worker("ghost")), Ok(None));
}

/// A state store whose reads can be made to fail, over an in-memory one.
struct Unreadable {
    inner: Fake,
    reads_fail: AtomicBool,
}

impl Unreadable {
    fn check(&self) -> Result<(), StoreError> {
        if self.reads_fail.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store cannot be read".to_owned()));
        }
        Ok(())
    }
}

impl Store for Unreadable {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.check()?;
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.check()?;
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.check()?;
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.check()?;
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// A js farm over a state store whose reads fail once `reads_fail` is set.
fn farm_over_unreadable() -> (Farm, Arc<Unreadable>) {
    let inner = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
    kbf_state::share(&inner).expect("the shared marker");
    let store = Arc::new(Unreadable {
        inner,
        reads_fail: AtomicBool::new(false),
    });
    let farm = farm_on(SchedulerCore::Js)
        .with_state(Some(StateStore {
            store: store.clone(),
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
    (farm, store)
}

/// Asserts what a read of a node answers over a state store: a node nobody has registered is
/// `None` while the store can be read, an error while it cannot say and the node is not
/// connected here, and a node connected here is answered either way; one whose stream ended is
/// an error while the store cannot be read. The detail of a node another frontend holds is not
/// answered on the js core (it needs reads the core does not have): that is the known
/// difference the marked tests in `farm/durable` pin. Catches: a store error taken for "no
/// such node", which an updater takes for a node with no daemon and no lease.
#[tokio::test(start_paused = true)]
async fn an_unregistered_node_is_unread_when_the_store_cannot_say_and_a_connected_one_is_answered()
{
    let (farm, store) = farm_over_unreadable();
    let joined = join(&farm, "node-a", linux(Some(3)));
    assert_eq!(farm.node_detail(&worker("ghost")), Ok(None));

    store.reads_fail.store(true, Ordering::SeqCst);

    assert_eq!(
        farm.node_detail(&worker("ghost")),
        Err(NodeUnread("ghost".to_owned()))
    );
    let answered = farm.node_detail(&worker("node-a")).expect("connected here");
    assert!(answered.expect("a node").node.connected);
    drop(joined.inbox);
    assert_eq!(
        farm.node_detail(&worker("node-a")),
        Err(NodeUnread("node-a".to_owned()))
    );
}

/// Asserts that a `NodeStatus` and an `UpdateStatus` are kept only from the node's newest
/// stream, with the answers and attention lines the legacy farm gives: the newest stream's
/// status raises the item and its line is returned, the replaced stream's is ignored and
/// returns nothing, an unknown node's update is not kept; and the software and the update
/// look show what the newest stream sent. Catches: a status taken from a replaced stream, a
/// stream compared by something other than the number the farm handed out, the lines of one
/// driver not the other's.
#[tokio::test(start_paused = true)]
async fn a_status_is_kept_from_the_newest_stream_only_with_the_legacy_farms_answers() {
    let (legacy, js) = pair();
    let farms = [&legacy, &js];
    let node = worker("mac-1");
    let old = join_both(farms, "mac-1", &mac(Some(2)));
    let new = join_both(farms, "mac-1", &mac(Some(3)));
    let ghost = worker("ghost");

    let mut answers = Vec::new();
    for (i, farm) in farms.into_iter().enumerate() {
        let ignored = farm.node_status(&node, old[i].stream, xcode_status("not agreed"));
        let raised = farm.node_status(&node, new[i].stream, xcode_status("not agreed"));
        let again = farm.node_status(&node, new[i].stream, xcode_status("not agreed again"));
        let kept = [
            farm.update_status(
                &node,
                new[i].stream,
                update_status("bbbbbbbbbbbb", State::Idle),
            ),
            farm.update_status(
                &node,
                old[i].stream,
                update_status("aaaaaaaaaaaa", State::Refused),
            ),
            farm.update_status(
                &ghost,
                new[i].stream,
                update_status("cccccccccccc", State::Idle),
            ),
        ];
        answers.push((ignored, raised, again, kept));
    }

    assert_eq!(answers[0], answers[1]);
    let (ignored, raised, again, kept) = &answers[1];
    assert!(ignored.is_empty());
    assert_eq!(raised.len(), 1, "{raised:?}");
    assert!(raised[0].0, "the item is raised");
    assert!(
        raised[0].1.starts_with("node mac-1: Xcode 16B40"),
        "{raised:?}"
    );
    assert!(
        again.is_empty(),
        "the same item is not raised again: {again:?}"
    );
    assert_eq!(kept, &[true, false, false]);
    let looks = js.update_look();
    assert_eq!(looks, legacy.update_look());
    let report = looks[0]
        .update
        .as_ref()
        .expect("the newest stream's report");
    assert_eq!(report.running_commit, "bbbbbbbbbbbb");
    let software = js
        .node_view(&node)
        .expect("listed")
        .software
        .expect("a status");
    assert_eq!(software.xcodes.len(), 1);
    assert_eq!(
        js.nodes().nodes[0].needs_attention,
        legacy.nodes().nodes[0].needs_attention
    );
    assert_eq!(js.nodes().nodes[0].protocol, Some(3));
}

/// Asserts that the update look names each node's `os` and `arch` from its `Hello` (the update
/// step and safe-to-stop read them), its lease count, and that none of its leases is
/// undelivered once the `Start` was sent, as the legacy farm shows them. Catches: an `os` left
/// empty, which makes a Linux node look like one safe-to-stop need not ask the storage half.
#[tokio::test(start_paused = true)]
async fn the_update_look_names_each_nodes_os_and_arch_and_its_leases() {
    let (legacy, js) = pair();
    let farms = [&legacy, &js];
    let _linux = join_both(farms, "linux-1", &linux(Some(3)));
    let _waiting = farms.map(|farm| submit(farm, 1));
    let _mac = join_both(farms, "mac-1", &mac(None));

    let looks = js.update_look();

    assert_eq!(looks, legacy.update_look());
    assert_eq!(looks.len(), 2);
    assert_eq!(
        (looks[0].os.as_str(), looks[1].os.as_str()),
        ("linux", "macos")
    );
    assert_ne!(looks[0].arch, looks[1].arch);
    assert!(!looks[0].arch.is_empty() && !looks[1].arch.is_empty());
    assert_eq!((looks[0].leases, looks[0].undelivered), (1, 0));
    assert_eq!(looks[1].leases, 0);
    assert!(!looks[0].cordoned);
}

/// Asserts that the lease list is the scheduler's: the leader term the farm was built with,
/// the wall-clock time of farm time zero, every lease or only a node's, and none for a node
/// that holds none or does not exist; and that the count matches the legacy farm's. Catches:
/// a term other than the scheduler's (every lease carries it), a node filter that is ignored.
#[tokio::test(start_paused = true)]
async fn the_lease_list_is_the_schedulers_for_all_nodes_or_one() {
    let (legacy, js) = pair();
    let farms = [&legacy, &js];
    let _a = join_both(farms, "linux-1", &linux(None));
    let _b = join_both(farms, "linux-2", &linux(None));
    let _waiting = farms.map(|farm| submit(farm, 1));

    let all = js.lease_views(None);

    assert_eq!(all.leases.len(), 1);
    assert_eq!(all.leases.len(), legacy.lease_views(None).leases.len());
    assert_eq!(all.term, js.term());
    let Driver::Js(inner) = &js.driver else {
        panic!("the farm runs on the legacy driver");
    };
    assert_eq!(all.epoch_unix_ms, inner.epoch_unix_ms);
    let holder = all.leases[0].node.clone();
    assert_eq!(js.lease_views(Some(&holder)).leases.len(), 1);
    let other = if holder.as_str() == "linux-1" {
        "linux-2"
    } else {
        "linux-1"
    };
    assert!(js.lease_views(Some(&worker(other))).leases.is_empty());
    assert!(js.lease_views(Some(&worker("ghost"))).leases.is_empty());
}

/// Asserts that the queue views count what waits: operations no node can take yet, by the
/// operation count, the depth of the queue view and both reads of the pair; and that the
/// whole-node view is `None` without whole-node settings and present, with no request, with
/// them, the same as the legacy farm's. Catches: a depth read from a stale snapshot, a
/// whole-node view built when the farm has no such settings.
#[tokio::test(start_paused = true)]
async fn the_queue_views_count_the_waiting_operations_and_the_whole_node_view_follows_its_settings()
{
    let (legacy, js) = pair();
    let _waiting = [&legacy, &js].map(|farm| (submit(farm, 1), submit(farm, 2)));

    assert_eq!(js.queued_operations(), 2);
    assert_eq!(js.queued_operations(), legacy.queued_operations());
    assert_eq!(js.queue_view().depth, 2);
    assert_eq!(js.queue_view().depth, legacy.queue_view().depth);
    let (queue, whole) = js.queue_views();
    assert_eq!(queue.depth, 2);
    assert!(whole.is_none());
    assert!(js.whole_view().is_none());

    let (legacy, js) = (
        legacy.with_whole_node(Some(WholeNodeConfig::dedicated(2))),
        js.with_whole_node(Some(WholeNodeConfig::dedicated(2))),
    );
    let whole = js.whole_view().expect("whole-node settings");
    let want = legacy.whole_view().expect("whole-node settings");
    assert_eq!(whole.total, want.total);
    assert!(whole.requests.is_empty());
    assert_eq!(
        format!("{:?}", whole.summary),
        format!("{:?}", want.summary)
    );
    let (_, together) = js.queue_views();
    assert!(together.is_some());
}

/// Asserts the counters of an idle farm are the legacy farm's, and that the gauges follow what
/// they count: a stream that rejoins is one session, and an open Execute stream is counted
/// from the cache's metrics. Catches: sessions counted per stream, not per worker; the gauge
/// read from a metrics registry other than the farm's cache.
#[tokio::test(start_paused = true)]
async fn the_counters_of_an_idle_farm_are_the_legacy_farms_and_the_gauges_follow_what_they_count() {
    let (legacy, js) = pair();
    let farms = [&legacy, &js];

    assert_eq!(js.failed_actions(), legacy.failed_actions());
    assert_eq!(js.dropped_writes(), legacy.dropped_writes());
    assert_eq!(js.memory_runs(), legacy.memory_runs());
    assert_eq!(js.avoided(), legacy.avoided());
    assert_eq!(js.open_sessions(), 0);
    assert_eq!(js.open_executions(), legacy.open_executions());
    let _first = join_both(farms, "linux-1", &linux(None));
    let _again = join_both(farms, "linux-1", &linux(None));
    assert_eq!(js.open_sessions(), 1);
    assert_eq!(js.open_sessions(), legacy.open_sessions());
    let metrics = js.cache.metrics();
    let _open = metrics.execute.open_executions.raise();
    assert_eq!(js.open_executions(), 1);
}

/// Asserts that the pack counts are kept from a node's newest stream only: counts on it are
/// kept and listed in node-id order, ones on a replaced stream or an unknown node are
/// ignored, a heartbeat without counts forgets the node's, and the reads and the writes are
/// kept apart, as on the legacy farm. Catches: counts kept from a replaced stream; the reads
/// and the writes sharing a table.
#[tokio::test(start_paused = true)]
async fn the_pack_counts_are_kept_from_the_newest_stream_only() {
    let (legacy, js) = pair();
    let farms = [&legacy, &js];
    let old = join_both(farms, "linux-1", &linux(None));
    let new = join_both(farms, "linux-1", &linux(None));
    let other = join_both(farms, "linux-0", &linux(None));
    let reads = |served| PackReadCounts {
        served,
        ..PackReadCounts::default()
    };
    let writes = |packs| PackWriteCounts {
        packs,
        ..PackWriteCounts::default()
    };

    for (i, farm) in farms.into_iter().enumerate() {
        farm.note_pack_reads(&worker("linux-1"), new[i].stream, Some(reads(4)));
        farm.note_pack_reads(&worker("linux-1"), old[i].stream, Some(reads(9)));
        farm.note_pack_reads(&worker("ghost"), new[i].stream, Some(reads(9)));
        farm.note_pack_reads(&worker("linux-0"), other[i].stream, Some(reads(1)));
        farm.note_pack_writes(&worker("linux-1"), new[i].stream, Some(writes(7)));
        farm.note_pack_writes(&worker("linux-1"), old[i].stream, Some(writes(8)));
    }

    assert_eq!(js.pack_reads(), legacy.pack_reads());
    let counted = js.pack_reads();
    let listed: Vec<(&str, u64)> = counted
        .iter()
        .map(|(worker, counts)| (worker.as_str(), counts.served))
        .collect();
    assert_eq!(listed, [("linux-0", 1), ("linux-1", 4)]);
    assert_eq!(js.pack_writes(), legacy.pack_writes());
    assert_eq!(js.pack_writes().len(), 1);
    assert_eq!(js.pack_writes()[0].1.packs, 7);
    for (i, farm) in farms.into_iter().enumerate() {
        farm.note_pack_reads(&worker("linux-1"), new[i].stream, None);
    }
    assert_eq!(js.pack_reads(), legacy.pack_reads());
    assert_eq!(js.pack_reads().len(), 1);
    assert_eq!(js.pack_writes().len(), 1);
}
