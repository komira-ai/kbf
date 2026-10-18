//! The update controller on a farm that is quiesced or handed over, and with a store handle that
//! a newer writer has superseded (`docs/design/frontend-proxy.md` section 9, rule R4: the node
//! controller runs only on the generation holder). The store's generation fence is what refuses
//! a superseded controller's writes. These tests are about the time before it applies: the farm
//! is fenced (a quiesce) and the controller's store handle is still the writer, so only the
//! controller and its transport can keep it from moving host rows, cordoning and ordering.
//!
//! Every test runs a real `Farm` over an in-memory state store, with one Mac whose stream is the
//! channel the server writes to, and reads what the controller left in the store.

#[path = "support/hooked.rs"]
mod hooked;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Handed, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::update::{UpdateOrder, UpdateStatus};
use kbf_proto::worker::{ServerMessage, server_message};
use kbf_server::farm::{Farm, Reported};
use kbf_server::update::{Policy, Release, Rows, Transport, ports, spawn_controller};
use kbf_server::{EpochPolicy, StateStore};
use kbf_state::{Fenced, Store, Versioned};
use kbf_types::{Resources, WorkerId};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

const A: &str = "0123456789abcdef0123456789abcdef01234567";

/// The controller's look interval: short, so a quiet spell holds many looks.
const EVERY: Duration = Duration::from_millis(5);

/// How long a test lets a controller go on when it expects it to do nothing: about eighty
/// looks.
const QUIET: Duration = Duration::from_millis(400);

/// How long a test waits for something it expects to happen.
const LONG: Duration = Duration::from_secs(10);

type Inbound = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

/// Log lines written by the test's subscriber on this thread. The runtime of a `#[tokio::test]`
/// runs the controller's task on that thread, so its lines are the ones captured.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Lines {
    fn subscribe(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        // A callsite another test's thread reached first may have cached "never".
        tracing_core::callsite::rebuild_interest_cache();
        guard
    }

    /// The lines that contain `part`.
    fn with(&self, part: &str) -> Vec<String> {
        let bytes = self.0.lock().expect("lock").clone();
        String::from_utf8(bytes)
            .expect("UTF-8 logs")
            .lines()
            .filter(|line| line.contains(part))
            .map(str::to_owned)
            .collect()
    }
}

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A farm over a state store it holds generation 1 of, with `mac-1` connected and idle, a
/// release desired and `auto_nodes` set: a controller that is free to would cordon the Mac and
/// then activate the release on it.
struct Rig {
    farm: Arc<Farm>,
    /// The whole store: the rows, and the generation a newer writer takes. Its reads can run a
    /// hook, to fence the farm at a chosen moment of a call.
    whole: Arc<hooked::Hooked>,
    /// The handle of generation 1, which the farm and the controller write through.
    writer: Arc<dyn Store>,
    /// What the server sends to the Mac.
    mac: Inbound,
}

fn mac() -> WorkerId {
    WorkerId::new("mac-1")
}

fn rig() -> Rig {
    let whole = hooked::Hooked::new();
    let writer: Arc<dyn Store> =
        Arc::new(Fenced::take(Arc::clone(&whole) as Arc<dyn Store>, None).expect("generation 1"));
    let farm = Arc::new(
        Farm::new(
            Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_state(Some(StateStore {
            store: Arc::clone(&writer),
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store"),
    );
    let (outbound, mac_inbound) = mpsc::unbounded_channel();
    let caps = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")]).expect("caps");
    let stream = farm.register(
        &mac(),
        kbf_sched::DaemonInstance::new("mac-1"),
        kbf_sched::DaemonInstance::default(),
        Reported::serving(Resources::new(8_000, 16 << 30), caps),
        outbound,
        ServerMessage::default(),
    );
    farm.update_status(
        &mac(),
        stream,
        UpdateStatus {
            running_commit: "fedcba987654".to_owned(),
            ..UpdateStatus::default()
        },
    );
    let rows = Rows::new(Arc::clone(&writer));
    rows.offer(&Release::new(A), farm.wall_now())
        .expect("a run");
    rows.set_policy(&Policy {
        auto_nodes: true,
        ..Policy::default()
    })
    .expect("policy");
    Rig {
        farm,
        whole,
        writer,
        mac: mac_inbound,
    }
}

impl Rig {
    /// Every row of the store.
    fn rows(&self) -> Vec<(String, Versioned)> {
        self.whole.scan_prefix("").expect("rows")
    }

    /// Whether the farm kept a cordon row for the Mac.
    fn cordon_row(&self) -> bool {
        !self.whole.scan_prefix("cordon/").expect("rows").is_empty()
    }

    /// How many transactions the store has applied.
    fn txns(&self) -> u64 {
        self.whole.commit_stats().txns
    }

    /// The controller of this rig, every `EVERY`.
    fn spawn(&self) -> AbortHandle {
        spawn_controller(
            Arc::clone(&self.farm),
            Some(Arc::clone(&self.writer)),
            EVERY,
        )
        .expect("a state store")
    }

    /// The orders the server has sent to the Mac since the last call.
    fn orders(&mut self) -> Vec<UpdateOrder> {
        let mut orders = Vec::new();
        while let Ok(sent) = self.mac.try_recv() {
            if let Some(server_message::Message::UpdateOrder(order)) =
                sent.expect("no error").message
            {
                orders.push(order);
            }
        }
        orders
    }
}

/// Polls `look` until it answers, for at most `LONG`.
async fn within<T>(what: &str, mut look: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + LONG;
    loop {
        if let Some(found) = look() {
            return found;
        }
        assert!(Instant::now() < deadline, "no {what} within {LONG:?}");
        tokio::time::sleep(EVERY).await;
    }
}

/// Catches a control that proves nothing: the set-up of the tests below, left to itself, does
/// cordon the Mac and send it an activation order, and the task goes on running after. If it
/// did not, "nothing happened" on a fenced farm would be what an idle rig does anyway.
#[tokio::test]
async fn a_farm_that_is_not_handed_over_cordons_the_mac_and_activates_it() {
    let mut rig = rig();
    let task = rig.spawn();
    let mut sent = Vec::new();
    let order = within("activation order", || {
        sent.extend(rig.orders());
        sent.first().cloned()
    })
    .await;
    assert_eq!(order.commit, A);
    assert!(rig.cordon_row(), "the host was ordered with no cordon");
    assert!(!task.is_finished());
    task.abort();
}

/// Catches a controller that goes on looking at a fenced farm. The farm is quiesced and the
/// controller's store handle is still the writer, so only the controller's own check stops it
/// from moving the host's row, cordoning the host and activating it. Over eighty looks the
/// store takes not one transaction, the Mac receives no order, the task is still running (it is
/// waiting, not ended: a farm that is resumed has a controller again), and the wait is said
/// once, not at every look.
///
/// Mutants seen red: the check on the farm removed from the loop (the host row is written at
/// the first look and the cordon follows); the log flag removed (a line at every look).
#[tokio::test]
async fn a_handed_over_farm_makes_no_look() {
    let log = Lines::default();
    let _logging = log.subscribe();
    let mut rig = rig();
    rig.farm.fence();
    let (rows, txns) = (rig.rows(), rig.txns());
    let task = rig.spawn();
    tokio::time::sleep(QUIET).await;
    assert!(!task.is_finished(), "a handed-over farm ended the task");
    assert_eq!(rig.txns(), txns, "the controller wrote to the store");
    assert_eq!(rig.rows(), rows);
    assert!(!rig.cordon_row());
    assert!(rig.orders().is_empty(), "an order reached the Mac");
    assert_eq!(log.with("this farm is handed over").len(), 1);
    task.abort();
}

/// Catches an order that goes out in the time between a look's start and the farm's being
/// fenced (the look is under way, the fence lands, the order is given), and one that goes out
/// for one of the two ways it is given. On a fenced farm `Farm::place` still writes the cordon
/// row and `Farm::send_order` still reaches the node, so each order is refused by the
/// transport: cordon, uncordon, activation and confirmation, each with an error that names the
/// reason, with no transaction (a confirmation draws its id from the store's counter, which is
/// a transaction), no cordon row and no message to the Mac. The same calls on the farm before
/// the fence succeed. The activation and the confirmation refused are of a second activation,
/// never confirmed: a confirmation that was sent already is found in the record and draws
/// nothing, which would hide a draw made on a fenced farm.
///
/// Mutants seen red: the check removed from the placement path (the cordon and the uncordon
/// are given); the check removed from the top of `confirm` only (the confirmation draws an
/// id: the store took a transaction); the checks removed from the top of `activate` and
/// `confirm` and from `send` (the activation and the confirmation are sent).
#[tokio::test]
async fn a_handed_over_farm_cordons_nobody_and_sends_no_order() {
    let mut rig = rig();
    let update_rows = Rows::new(Arc::clone(&rig.writer));
    let (mut transport, _facts) = ports(Arc::clone(&rig.farm), update_rows.clone());
    let release = Release::new(A);
    // From the store's counter, as the controller's are: the confirmation draws the next.
    let epoch = rig.farm.lease_epoch();
    let sent = update_rows.mint_order(epoch).expect("an id");

    transport.cordon(&mac()).expect("cordoned");
    assert!(rig.cordon_row());
    transport.uncordon(&mac()).expect("uncordoned");
    assert!(!rig.cordon_row());
    transport
        .activate(&mac(), sent, &release)
        .expect("activation sent");
    transport
        .confirm(&mac(), sent, &release)
        .expect("confirmation sent");
    assert_eq!(rig.orders().len(), 2);

    let activation = update_rows.mint_order(epoch).expect("an id");
    rig.farm.fence();
    let (rows, txns) = (rig.rows(), rig.txns());
    let refused = [
        ("cordon", transport.cordon(&mac())),
        ("uncordon", transport.uncordon(&mac())),
        (
            "activation",
            transport.activate(&mac(), activation, &release),
        ),
        (
            "confirmation",
            transport.confirm(&mac(), activation, &release),
        ),
    ];
    let given: Vec<&str> = refused
        .iter()
        .filter(|(_, answer)| answer.is_ok())
        .map(|(order, _)| *order)
        .collect();
    assert!(given.is_empty(), "a handed-over farm gave: {given:?}");
    for (order, answer) in refused {
        let error = answer.expect_err("refused");
        assert!(
            error.to_string().contains("handed over"),
            "{order}: {error}"
        );
    }
    assert_eq!(rig.rows(), rows);
    assert_eq!(rig.txns(), txns);
    assert!(rig.orders().is_empty(), "an order reached the Mac");
}

/// Catches a controller that goes on after a newer writer took its store. The farm is not
/// fenced here, so nothing but the store's answer tells the controller: its first write is
/// refused as `Fenced`, the task ends, nothing it did lands, the Mac is ordered nothing, and
/// the end is one line (not the generic "update controller:" warning as well, and not one per
/// look).
///
/// Mutants seen red: the end removed (the task goes on, the line comes at every look).
#[tokio::test]
async fn a_controller_whose_store_handle_is_superseded_ends() {
    let log = Lines::default();
    let _logging = log.subscribe();
    let mut rig = rig();
    let _newer =
        Fenced::take(Arc::clone(&rig.whole) as Arc<dyn Store>, Some(1)).expect("generation 2");
    let (rows, txns) = (rig.rows(), rig.txns());
    let task = rig.spawn();
    within("end of the task", || task.is_finished().then_some(())).await;
    assert_eq!(rig.txns(), txns, "a superseded controller wrote");
    assert_eq!(rig.rows(), rows);
    assert!(rig.orders().is_empty(), "an order reached the Mac");
    assert_eq!(log.with("update controller").len(), 1);
    assert_eq!(log.with("a newer writer holds the state store").len(), 1);
}

/// Catches an activation that goes out although the farm was fenced after the transport first
/// looked at it: the fence lands while the activation reads the policy (for the window it
/// gives the host), which is after the check at the top of the call and before the farm is
/// asked to send. The check just before the send refuses it with a reason, and the Mac
/// receives nothing. The hook runs after the policy read, so the fence is the farm's own, not
/// one placed before the call.
///
/// Mutants seen red: the check removed from `send` only (the activation is sent).
#[tokio::test]
async fn a_fence_that_lands_while_an_activation_is_prepared_stops_it() {
    let mut rig = rig();
    let update_rows = Rows::new(Arc::clone(&rig.writer));
    let (mut transport, _facts) = ports(Arc::clone(&rig.farm), update_rows.clone());
    let activation = update_rows
        .mint_order(rig.farm.lease_epoch())
        .expect("an id");
    let farm = Arc::clone(&rig.farm);
    rig.whole.after_get("update/policy", move || farm.fence());
    let error = transport
        .activate(&mac(), activation, &Release::new(A))
        .expect_err("refused");
    assert!(error.to_string().contains("handed over"), "{error}");
    assert!(rig.farm.handed());
    assert!(rig.orders().is_empty(), "an order reached the Mac");
}
