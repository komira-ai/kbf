//! The controller's ports over the farm and its sessions (`docs/design/split-mode-updates.md`
//! section 6.1): a real farm with real node sessions (the stream is the channel the server
//! writes), the real update client (`kbf-update`) as the node, and the controller driven look
//! by look. The helper is the test: it writes `status.json` as the root helper does.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use kbf_caps::NodeCaps;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::update::{UpdateOrder, UpdateStatus};
use kbf_proto::worker::{ServerMessage, server_message};
use kbf_server::farm::{Farm, Reported, StreamId};
use kbf_server::members::{Members, MembershipConfig};
use kbf_server::update::{Controller, HostPhase, OrderId, Policy, Release, Rows, Transport, ports};
use kbf_state::Store;
use kbf_types::{FarmTime, Resources, WorkerId};
use kbf_update::{Component, Config, Expected, Handle, Offer, Order};
use tokio::sync::mpsc;

const A: &str = "0123456789abcdef0123456789abcdef01234567";
const OLD: &str = "fedcba9876543210fedcba9876543210fedcba98";
const PLATFORM: &str = "darwin-arm64";

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "kbf-update-session-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// What the root helper writes: `state`, with `installed` and `staged` as the commits it
/// holds, having acted on order `acked`.
fn helper_says(dir: &Path, state: &str, installed: &str, staged: &str, acked: u64) {
    let quote = |c: &str| format!("\"{c}\"").replace("\"\"", "null");
    let text = format!(
        "{{\"schema\":2,\"installed_commit\":{},\"previous_commit\":null,\"staged_commit\":{},\
         \"state\":\"{state}\",\"refusal\":\"\",\"acked_order_id\":{acked},\"staged\":null}}",
        quote(installed),
        quote(staged)
    );
    fs::write(dir.join("status.json"), text).expect("write status.json");
}

type Inbound = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

/// A Mac node: a session on the farm, and the update client beside it.
struct Node {
    farm: Arc<Farm>,
    id: WorkerId,
    dir: PathBuf,
    stream: StreamId,
    inbound: Inbound,
    client: Handle,
}

fn register(farm: &Farm, name: &str, os: &str) -> (StreamId, Inbound) {
    let (outbound, inbound) = mpsc::unbounded_channel();
    let caps = NodeCaps::from_report([("arch", "arm64"), ("os", os)]).expect("caps");
    let stream = farm.register(
        &WorkerId::new(name),
        kbf_sched::DaemonInstance::new(name),
        kbf_sched::DaemonInstance::default(),
        Reported::serving(Resources::new(8_000, 16 << 30), caps),
        outbound,
        ServerMessage::default(),
    );
    (stream, inbound)
}

fn client(dir: &Path, running: &str) -> Handle {
    let config = Config::new(dir.to_path_buf(), PLATFORM.to_owned(), running.to_owned());
    kbf_update::start(config).expect("the client starts").0
}

impl Node {
    /// A Mac running `running` (12 hex), whose helper holds `A` staged over `OLD`.
    fn new(farm: &Arc<Farm>, name: &str, running: &str) -> Self {
        let dir = scratch(name);
        helper_says(&dir, "staged", OLD, A, 0);
        let (stream, inbound) = register(farm, name, "macos");
        let node = Self {
            farm: Arc::clone(farm),
            id: WorkerId::new(name),
            client: client(&dir, running),
            dir,
            stream,
            inbound,
        };
        // The frontend's offer of the release, which an activation needs the node to hold.
        node.client.offer(Offer {
            generation: 1,
            commit: A.to_owned(),
            platform: PLATFORM.to_owned(),
            components: vec![Component {
                name: "kbf".to_owned(),
                sha256: "ab".repeat(32),
                size: 1000,
                attestation: b"{\"bundle\":1}".to_vec(),
            }],
            expected: Expected {
                protocol: 7,
                min_protocol: 6,
                state_schema: 3,
                profile: "release".to_owned(),
            },
        });
        node.report();
        node
    }

    /// The node sends its status, as its session does after every change.
    fn report(&self) {
        self.client.refresh();
        let status = UpdateStatus::from(&*self.client.status().borrow());
        self.farm.update_status(&self.id, self.stream, status);
    }

    /// The orders the server has sent since the last call.
    fn orders(&mut self) -> Vec<UpdateOrder> {
        let mut orders = Vec::new();
        while let Ok(sent) = self.inbound.try_recv() {
            if let Some(server_message::Message::UpdateOrder(order)) =
                sent.expect("no error").message
            {
                orders.push(order);
            }
        }
        orders
    }

    /// The client's decision on `order`, as the daemon's session takes it.
    fn take(&self, order: &UpdateOrder) -> kbf_update::Decision {
        self.client
            .order(&Order::from(order.clone()), self.farm.lease_epoch(), &|| 0)
    }
}

fn cordoned(farm: &Farm, node: &WorkerId) -> bool {
    matches!(
        farm.node_view(node).expect("listed").placement,
        kbf_server::fleet::PlacementView::Cordoned
    )
}

fn farm() -> Arc<Farm> {
    Arc::new(Farm::new(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ))
}

fn store() -> Arc<dyn Store> {
    hooked::Hooked::new()
}

fn hooked_store() -> (Arc<hooked::Hooked>, Arc<dyn Store>) {
    let hooked = hooked::Hooked::new();
    let store: Arc<dyn Store> = hooked.clone();
    (hooked, store)
}

fn members(store: &Arc<dyn Store>) -> Arc<Members> {
    Arc::new(Members::new(Arc::clone(store), MembershipConfig::default()))
}

fn controller(farm: &Arc<Farm>, store: &Arc<dyn Store>) -> Controller {
    let (transport, source) = ports(
        Arc::clone(farm),
        Rows::new(Arc::clone(store)),
        members(store),
    );
    Controller::new(
        Arc::clone(store),
        farm.lease_epoch(),
        Box::new(transport),
        Box::new(source),
    )
}

/// Looks until `done` says so, at most `looks` times; the orders sent meanwhile are `taken`.
fn look_until(
    controller: &mut Controller,
    farm: &Farm,
    looks: usize,
    mut done: impl FnMut() -> bool,
) {
    for _ in 0..looks {
        if done() {
            return;
        }
        controller.tick(farm.wall_now()).expect("a look");
    }
    assert!(done(), "not done after {looks} looks");
}

fn phase(store: &Arc<dyn Store>, node: &str) -> Option<HostPhase> {
    let hosts = Rows::new(Arc::clone(store)).hosts().expect("rows");
    hosts
        .get(&WorkerId::new(node))
        .map(|row| row.value.record.phase.clone())
}

/// A controller run over one Mac, the whole way: cordon, activate, the helper reports, the
/// daemon comes back at the release, confirm, uncordon. The node is the real update client;
/// the activation and the confirmation reach it as orders of the epoch `Welcome` names, the
/// client accepts each, and the helper's word comes back as the `UpdateStatus` that moves
/// the host on. The confirmation has an id of its own (the client would drop the
/// activation's as a replay), and the status that names it counts for the activation.
///
/// Mutants seen red: the order sent under another epoch (the client refuses it and the run
/// is held at `Activating`); the confirmation without its commit (refused: the commit is
/// neither staged nor installed); the confirmation under the activation's id (dropped as a
/// replay, never acted on).
#[test]
fn a_run_activates_one_node_and_confirms_it() {
    let farm = farm();
    let store = store();
    let rows = Rows::new(Arc::clone(&store));
    rows.offer(&Release::new(A), farm.wall_now())
        .expect("a run");
    rows.set_policy(&Policy {
        auto_nodes: true,
        ..Policy::default()
    })
    .expect("policy");
    let mut node = Node::new(&farm, "mac-1", "fedcba987654");
    let mut controller = controller(&farm, &store);

    // Cordon, wait for the leases (none), activate.
    let mut sent = Vec::new();
    look_until(&mut controller, &farm, 30, || {
        sent.extend(node.orders());
        !sent.is_empty()
    });
    let [activation] = &sent[..] else {
        panic!("one order expected: {sent:?}");
    };
    assert_eq!(
        activation.verb,
        kbf_proto::update::update_order::Verb::Activate as i32
    );
    assert_eq!(activation.epoch, farm.lease_epoch());
    assert_eq!(activation.commit, A);
    assert!(activation.order_id > 0);
    assert!(cordoned(&farm, &node.id));
    assert_eq!(node.take(activation), kbf_update::Decision::Accepted);

    // The helper flips and restarts the daemon: a new process, a new session, the new commit.
    helper_says(
        &node.dir,
        "activated_unconfirmed",
        A,
        "",
        activation.order_id,
    );
    let old_stream = node.stream;
    let (stream, inbound) = register(&farm, "mac-1", "macos");
    (node.stream, node.inbound) = (stream, inbound);
    node.client = client(&node.dir, &A[..12]);
    node.report();
    assert_ne!(old_stream, node.stream);

    let mut sent = Vec::new();
    look_until(&mut controller, &farm, 30, || {
        sent.extend(node.orders());
        !sent.is_empty()
    });
    let [confirmation] = &sent[..] else {
        panic!("one order expected: {sent:?}");
    };
    assert_eq!(
        confirmation.verb,
        kbf_proto::update::update_order::Verb::Confirm as i32
    );
    assert_eq!(confirmation.commit, A);
    assert_eq!(confirmation.epoch, activation.epoch);
    assert!(confirmation.order_id > activation.order_id);
    assert_eq!(node.take(confirmation), kbf_update::Decision::Accepted);

    // The helper confirms, naming the confirmation's id; the host is done and uncordoned.
    helper_says(&node.dir, "confirmed", A, "", confirmation.order_id);
    node.report();
    look_until(&mut controller, &farm, 30, || {
        phase(&store, "mac-1") == Some(HostPhase::Done)
    });
    assert!(!cordoned(&farm, &node.id));
}

/// Catches an order sent under an epoch the sessions did not name (a node would refuse it, or
/// worse, a restored store's old orders would be taken), and the right one not sent: the
/// activation of an order of another epoch is refused by the transport and nothing reaches the
/// node; of the right epoch it reaches it, with its id, its epoch and the commit.
///
/// Mutant seen red: the epoch check removed (the order goes out).
#[test]
fn an_order_of_another_epoch_is_refused_and_not_sent() {
    let farm = farm();
    let store = store();
    let mut node = Node::new(&farm, "mac-1", "fedcba987654");
    let (mut transport, _) = ports(
        Arc::clone(&farm),
        Rows::new(Arc::clone(&store)),
        members(&store),
    );
    let epoch = farm.lease_epoch();
    let release = Release::new(A);
    for wrong in [epoch + 1, epoch - 1] {
        let refused = transport
            .activate(&node.id, OrderId::new(wrong, 7), &release)
            .expect_err("another epoch");
        assert!(refused.0.contains("epoch"), "{refused}");
        let refused = transport
            .confirm(&node.id, OrderId::new(wrong, 7), &release)
            .expect_err("another epoch");
        assert!(refused.0.contains("epoch"), "{refused}");
    }
    assert_eq!(node.orders(), []);
    transport
        .activate(&node.id, OrderId::new(epoch, 7), &release)
        .expect("sent");
    let [sent] = &node.orders()[..] else {
        panic!("one order");
    };
    assert_eq!(
        (sent.order_id, sent.epoch, sent.commit.as_str()),
        (7, epoch, A)
    );
}

/// Catches an activation that is given twice and acted on twice: the controller repeats its
/// orders (after an error, after a restart), so the same order reaching the node again must be
/// answered as a replay, written once, and acknowledged under the same id.
#[test]
fn a_replayed_order_is_dropped_at_the_node() {
    let farm = farm();
    let store = store();
    let mut node = Node::new(&farm, "mac-1", "fedcba987654");
    let (mut transport, _) = ports(
        Arc::clone(&farm),
        Rows::new(Arc::clone(&store)),
        members(&store),
    );
    let order = OrderId::new(farm.lease_epoch(), 9);
    for _ in 0..2 {
        transport
            .activate(&node.id, order, &Release::new(A))
            .expect("sent");
    }
    let sent = node.orders();
    assert_eq!(sent.len(), 2);
    assert_eq!(node.take(&sent[0]), kbf_update::Decision::Accepted);
    assert_eq!(node.take(&sent[1]), kbf_update::Decision::Replayed);
    // The records in the spool, not the client's own state file beside them.
    let written = fs::read_dir(node.dir.join("orders"))
        .expect("orders")
        .filter(|e| {
            let name = e.as_ref().expect("entry").file_name();
            kbf_update::record::parse_file_name(&name.to_string_lossy()).is_some()
        })
        .count();
    assert_eq!(written, 1, "written once");
    assert_eq!(node.client.status().borrow().acked_order_id, 9);
}

/// Catches facts read from a stream the node has left: the status the old process sent
/// (`CONFIRMED` for the order) must not stand for the new process, which has said nothing, and
/// the old stream's late status must not reach the facts. With nothing from the new stream the
/// host has no helper report and no release; once the new stream reports, it has.
///
/// Mutant seen red: the current-stream check dropped from `Farm::update_status`.
#[test]
fn facts_come_from_the_current_stream_only() {
    let farm = farm();
    let store = store();
    let rows = Rows::new(Arc::clone(&store));
    rows.offer(&Release::new(A), farm.wall_now())
        .expect("a run");
    let old = Node::new(&farm, "mac-1", &A[..12]);
    helper_says(&old.dir, "confirmed", A, "", 5);
    old.report();
    let (_, mut source) = ports(Arc::clone(&farm), rows, members(&store));
    let look = |source: &mut kbf_server::update::FarmFacts| {
        use kbf_server::update::FactsSource as _;
        source.look(FarmTime::from_millis(0)).expect("a look").hosts
    };
    let [seen] = &look(&mut source)[..] else {
        panic!("one host");
    };
    assert_eq!(seen.release, Some(Release::new(A)));
    assert!(seen.helper.is_some());

    // The node reconnects and has said nothing; the old stream says more.
    let (stream, _inbound) = register(&farm, "mac-1", "macos");
    old.report();
    let [seen] = &look(&mut source)[..] else {
        panic!("one host");
    };
    assert_eq!((seen.release.clone(), seen.helper), (None, None));
    let status = UpdateStatus {
        running_commit: A[..12].to_owned(),
        state: kbf_proto::update::update_status::State::Confirmed as i32,
        acked_order_id: 5,
        ..UpdateStatus::default()
    };
    farm.update_status(&WorkerId::new("mac-1"), stream, status);
    let [seen] = &look(&mut source)[..] else {
        panic!("one host");
    };
    assert!(seen.helper.is_some());
}

/// Catches a state the controller reads wrong, a host class it mistakes, and a refusal that
/// is a pretence: every helper state maps to its report (the states of an idle or fetching
/// node to none), a Linux host is listed as a storage-and-daemon host linked to no member (so
/// never started), a Mac is daemon-only of the class of its arch, a host of another `os` is
/// not listed, a mark of a member that does not exist is refused in words that name the host,
/// the member and the cause, and the undo of it is nothing to do. An alert goes out at either
/// severity.
#[test]
fn the_ports_read_every_state_and_refuse_the_marks() {
    use kbf_proto::update::update_status::State;
    use kbf_server::update::{
        Alert, FactsSource as _, HelperStatus, HoldReason, HostKind, Platform,
    };
    let farm = farm();
    let store = store();
    let rows = Rows::new(Arc::clone(&store));
    let (mut transport, mut source) = ports(Arc::clone(&farm), rows, members(&store));
    let (mac, mac_rx) = register(&farm, "mac", "macos");
    let (_linux, _linux_rx) = register(&farm, "linux", "linux");
    let (_bsd, _bsd_rx) = register(&farm, "bsd", "freebsd");
    let table = [
        (
            State::ActivatedUnconfirmed,
            Some(HelperStatus::ActivatedUnconfirmed),
        ),
        (State::Confirmed, Some(HelperStatus::Confirmed)),
        (State::Reverted, Some(HelperStatus::Reverted)),
        (State::Refused, Some(HelperStatus::Refused)),
        (State::Idle, None),
        (State::Fetching, None),
        (State::Staged, None),
        (State::Manual, None),
        (State::Unspecified, None),
    ];
    for (state, want) in table {
        let status = UpdateStatus {
            running_commit: "0123456789ab".to_owned(),
            state: state as i32,
            acked_order_id: 4,
            ..UpdateStatus::default()
        };
        farm.update_status(&WorkerId::new("mac"), mac, status);
        let hosts = source.look(FarmTime::from_millis(0)).expect("a look").hosts;
        let seen = hosts
            .iter()
            .find(|h| h.id.as_str() == "mac")
            .expect("listed");
        assert_eq!(seen.helper.map(|h| h.status), want, "{state:?}");
    }
    // A process that does not know its commit runs no release the controller can name.
    let unknown = UpdateStatus {
        running_commit: "unknown".to_owned(),
        ..UpdateStatus::default()
    };
    farm.update_status(&WorkerId::new("mac"), mac, unknown);
    let hosts = source.look(FarmTime::from_millis(0)).expect("a look").hosts;
    let seen = hosts
        .iter()
        .find(|h| h.id.as_str() == "mac")
        .expect("listed");
    assert_eq!(seen.release, None);
    // Nor does a process whose text is the right length and no commit (a host chooses the
    // bytes; the report keeps them as data): the controller must not take it for a release
    // other than the desired one and move the host.
    for text in ["not-a-commit", "0123456789AB"] {
        let odd = UpdateStatus {
            running_commit: text.to_owned(),
            ..UpdateStatus::default()
        };
        farm.update_status(&WorkerId::new("mac"), mac, odd);
        let hosts = source.look(FarmTime::from_millis(0)).expect("a look").hosts;
        let seen = hosts
            .iter()
            .find(|h| h.id.as_str() == "mac")
            .expect("listed");
        assert_eq!(seen.release, None, "{text}");
    }
    let hosts = source.look(FarmTime::from_millis(0)).expect("a look").hosts;
    let ids: Vec<&str> = hosts.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, ["linux", "mac"], "freebsd is not an updatable host");
    assert_eq!(hosts[0].kind, HostKind::StorageAndDaemon);
    assert_eq!(
        (hosts[0].member, &hosts[0].platform),
        (None, &Platform::Linux)
    );
    assert_eq!(hosts[1].kind, HostKind::DaemonOnly);
    assert!(matches!(&hosts[1].platform, Platform::Mac(class) if class.as_str() == "macos-arm64"));

    let host = WorkerId::new("mac");
    // A member that has no row cannot be marked, and the error names the host, the member and
    // the cause; it has nothing to undo, which is not an error (it stopped, and registering
    // again deletes its mark).
    let refused = transport
        .mark_down(&host, kbf_map::MemberId(1))
        .expect_err("no such member");
    assert!(refused.0.contains("mac"), "{refused}");
    assert!(refused.0.contains("member m1"), "{refused}");
    assert!(refused.0.contains("no live row"), "{refused}");
    transport
        .undown(&host, kbf_map::MemberId(1))
        .expect("nothing to undo");
    transport
        .alert(&host, &Alert::Held(HoldReason::RevertRefused))
        .expect("paged");
    transport
        .alert(&host, &Alert::RebuildWaiting)
        .expect("warned");
    transport.cordon(&host).expect("cordon");
    assert!(cordoned(&farm, &host));
    transport.uncordon(&host).expect("uncordon");
    assert!(!cordoned(&farm, &host));
    assert!(transport.cordon(&WorkerId::new("ghost")).is_err());

    // An order for a node that never registered, or whose stream is gone, does not go.
    let order = OrderId::new(farm.lease_epoch(), 3);
    let release = Release::new(A);
    let ghost = transport.activate(&WorkerId::new("ghost"), order, &release);
    assert!(ghost.expect_err("unknown").0.contains("no node ghost"));
    drop(mac_rx);
    let gone = transport.activate(&host, order, &release);
    assert!(gone.expect_err("closed").0.contains("not connected"));
}

/// Catches a confirmation that gets a new id at every ask (the controller repeats its orders,
/// and the node drops a repeated id: each new one would be acted on as a fresh confirm), and
/// two activations sharing one: the same activation keeps its confirmation id across asks, a
/// later activation gets a higher one, and both go out under the epoch of the order.
///
/// Mutant seen red: the confirmation id drawn anew at every call.
#[test]
fn a_confirmation_keeps_its_id_for_its_activation() {
    let farm = farm();
    let store = store();
    let mut node = Node::new(&farm, "mac-1", "fedcba987654");
    let (mut transport, _) = ports(
        Arc::clone(&farm),
        Rows::new(Arc::clone(&store)),
        members(&store),
    );
    let epoch = farm.lease_epoch();
    let release = Release::new(A);
    let mut ids = Vec::new();
    for activation in [4, 4, 4, 9, 9] {
        transport
            .confirm(&node.id, OrderId::new(epoch, activation), &release)
            .expect("sent");
        let [sent] = &node.orders()[..] else {
            panic!("one order");
        };
        assert_eq!(sent.epoch, epoch);
        ids.push(sent.order_id);
    }
    assert_eq!(ids[0], ids[1]);
    assert_eq!(ids[1], ids[2]);
    assert_eq!(ids[3], ids[4]);
    assert!(ids[3] > ids[0], "a later activation, a higher id: {ids:?}");
    assert!(ids[0] > 0);
}

/// Catches a status read as the answer to an order it is not: the status names an id and no
/// epoch, so after a store restore (ids counted again) an old acknowledgement of the same
/// number must not stand for the new activation. A host that had acked id 5 of the old store,
/// still at the old release, is sent activation 5 of the new epoch: its unchanged status is
/// no report for it. Once the host has acted (the new release installed) the same id counts;
/// so does a host that acked it before a controller restart gave the order again. An id the
/// process sent no order under, and a different id, never count.
///
/// Mutant seen red: the status read as the current epoch's order whatever it names.
#[test]
fn an_old_acknowledgement_is_not_the_answer_to_a_new_order_of_the_same_number() {
    use kbf_proto::update::update_status::State;
    use kbf_server::update::FactsSource as _;
    let farm = farm();
    let store = store();
    let rows = Rows::new(Arc::clone(&store));
    rows.offer(&Release::new(A), farm.wall_now())
        .expect("a run");
    let (mut transport, mut source) = ports(Arc::clone(&farm), rows, members(&store));
    let (stream, _inbound) = register(&farm, "mac-1", "macos");
    let id = WorkerId::new("mac-1");
    let epoch = farm.lease_epoch();
    let status = |acked: u64, installed: &str, state: State| UpdateStatus {
        running_commit: "fedcba987654".to_owned(),
        installed_commit: installed.to_owned(),
        state: state as i32,
        acked_order_id: acked,
        ..UpdateStatus::default()
    };
    let reported = |source: &mut kbf_server::update::FarmFacts| {
        let hosts = source.look(FarmTime::from_millis(0)).expect("a look").hosts;
        hosts[0].helper
    };
    // Acknowledged order 5 of the old store; nothing sent yet.
    farm.update_status(&id, stream, status(5, OLD, State::Confirmed));
    assert_eq!(
        reported(&mut source).map(|r| r.order),
        Some(OrderId::new(0, 5))
    );
    // The new store's activation 5 goes out; the status has not changed.
    let activation = OrderId::new(epoch, 5);
    transport
        .activate(&id, activation, &Release::new(A))
        .expect("sent");
    assert_eq!(
        reported(&mut source).map(|r| r.order),
        Some(OrderId::new(0, 5))
    );
    // The host acts: the release is installed under the same number. It counts.
    farm.update_status(&id, stream, status(5, A, State::ActivatedUnconfirmed));
    assert_eq!(reported(&mut source).map(|r| r.order), Some(activation));
    // Another number is no answer to it.
    farm.update_status(&id, stream, status(6, A, State::ActivatedUnconfirmed));
    assert_eq!(
        reported(&mut source).map(|r| r.order),
        Some(OrderId::new(0, 6))
    );
    // The same order given again after the host acked it counts as well (a controller
    // that restarted gives its orders again): the baseline is the id itself, the release is in.
    let (mut again, mut source) = ports(
        Arc::clone(&farm),
        Rows::new(Arc::clone(&store)),
        members(&store),
    );
    farm.update_status(&id, stream, status(5, A, State::Confirmed));
    again
        .activate(&id, activation, &Release::new(A))
        .expect("sent");
    assert_eq!(reported(&mut source).map(|r| r.order), Some(activation));
}

/// Catches a farm that cannot be read answered as an empty one (the controller would then see
/// no host and think nothing is missing): a state store that fails the read of the desired
/// release makes the look an error that says so.
#[test]
fn a_look_that_cannot_read_the_run_is_an_error() {
    use kbf_server::update::FactsSource as _;
    let farm = farm();
    let (hooked, store) = hooked_store();
    let (_, mut source) = ports(
        Arc::clone(&farm),
        Rows::new(Arc::clone(&store)),
        members(&store),
    );
    hooked.fail_gets.store(1, Ordering::SeqCst);
    let failed = source
        .look(FarmTime::from_millis(0))
        .expect_err("the store failed");
    assert!(!failed.0.is_empty());
    assert!(
        source.look(FarmTime::from_millis(0)).is_ok(),
        "and then it reads"
    );
}

/// Catches an activation that is given again after a failed send and overwrites what the first
/// one recorded. The host had acked 4; activation 5 is sent; the host acts on it and the helper
/// reverts it (it reports REVERTED, acked 5, the old release still installed); the controller,
/// which saw the first send fail, sends activation 5 again. The record of the first sending
/// stands, so the status still answers the order: the run is held as `Reverted`, not left to
/// stall for lack of a report. The same for a refusal.
///
/// Mutant seen red: a repeat of an order records afresh (the baseline becomes 5, the old
/// release is installed, and the report is lost).
#[test]
fn a_repeated_activation_keeps_the_record_of_its_first_sending() {
    use kbf_proto::update::update_status::State;
    use kbf_server::update::{FactsSource as _, HelperStatus};
    for (state, want) in [
        (State::Reverted, HelperStatus::Reverted),
        (State::Refused, HelperStatus::Refused),
    ] {
        let farm = farm();
        let store = store();
        let rows = Rows::new(Arc::clone(&store));
        rows.offer(&Release::new(A), farm.wall_now())
            .expect("a run");
        let (mut transport, mut source) = ports(Arc::clone(&farm), rows, members(&store));
        let (stream, _inbound) = register(&farm, "mac-1", "macos");
        let id = WorkerId::new("mac-1");
        let status = |acked: u64, state: State| UpdateStatus {
            running_commit: "fedcba987654".to_owned(),
            installed_commit: OLD.to_owned(),
            state: state as i32,
            acked_order_id: acked,
            ..UpdateStatus::default()
        };
        farm.update_status(&id, stream, status(4, State::Idle));
        let activation = OrderId::new(farm.lease_epoch(), 5);
        transport
            .activate(&id, activation, &Release::new(A))
            .expect("sent");
        farm.update_status(&id, stream, status(5, state));
        transport
            .activate(&id, activation, &Release::new(A))
            .expect("sent again");
        let hosts = source.look(FarmTime::from_millis(0)).expect("a look").hosts;
        let report = hosts[0].helper.expect("a report");
        assert_eq!(
            (report.order, report.status),
            (activation, want),
            "{state:?}"
        );
    }
}
