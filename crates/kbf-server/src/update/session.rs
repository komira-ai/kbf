//! The controller's two ports over the farm and its worker sessions: [`SessionTransport`]
//! gives the orders ([`Transport`]) and [`FarmFacts`] reads the farm ([`FactsSource`]).
//! [`ports`] builds the pair; they share what they must agree on, which order ids went to
//! which host.
//!
//! **Orders.** An activation or confirmation goes out as an `UpdateOrder` on the host's
//! newest stream, with the epoch of the order id, which must be the epoch `Welcome` named
//! ([`Farm::lease_epoch`]): an order of another epoch is refused here, not sent, because the
//! node would refuse it. A cordon is the scheduler's own ([`Farm::place`]); an alert is a
//! line under the target `kbf_server::attention`. A planned-down mark needs the members API
//! of U3 (#914), which this branch does not have: both orders are refused, and no host that
//! has a storage half is made eligible (below).
//!
//! **A handed-over farm gives no order.** Once the farm is fenced ([`Handed::handed`]: a
//! quiesce or a stand-down) a cordon, an uncordon, an activation and a confirmation are
//! refused here, not given: [`Farm::place`] still writes the cordon row of a fenced farm and
//! [`Farm::send_order`] still reaches the node, and the successor is the one that orders the
//! hosts now. The check is made just before the farm is asked, for a look that was under way
//! when the farm was fenced; the store's generation fence remains what refuses a controller
//! whose handle was superseded.
//!
//! **A confirmation has an id of its own.** The node and its helper take an id once and drop
//! the same id again as a replay, so a `CONFIRM` under the activation's id would never be
//! acted on. The transport draws one from the store's counter ([`Rows::mint_order`]) the
//! first time it is asked to confirm an activation, and gives the same one again for the same
//! activation (the controller repeats the order); the facts map a status that names it back
//! to the activation, which is what the node step compares ([`HelperReport`]). A status
//! carries an id and no epoch, so it counts only for an order this process sent, and only if
//! the host had not acked that id before or has the order's release installed (a restored
//! store counts ids again). After a
//! restart of the controller the confirmation is given again under a new id and the old one
//! is no longer mapped; a `CONFIRMED` that names it counts once the new one is acted on.
//!
//! **Facts.** A host is a registered node whose `os` is `macos` (a Mac of class
//! `macos-<arch>`) or `linux`. A Linux host may have a storage half, which the farm cannot
//! tell without the join of U3, so it is listed as a storage-and-daemon host that is linked to
//! no member: the picker never starts it. What the node reports comes from its newest
//! `UpdateStatus` of its current stream (a stream that replaced it has none until it sends
//! one): the release it runs is the desired release when the 12 digits of its running commit
//! begin the desired commit, else its own commit (none, if that is not 12 lowercase hex
//! digits: the report keeps what a host sent); the helper's report is its state and the
//! order it acted on. `Work` is read from the scheduler: a lease on the node, else an empty
//! queue.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_front::Handed;
use kbf_map::MemberId;
use kbf_proto::update::UpdateOrder;
use kbf_proto::update::update_order::Verb;
use kbf_proto::update::update_status::State;
use kbf_types::{FarmTime, WorkerId};

use super::command::{Alert, Severity};
use super::facts::{
    DaemonLoad, HelperReport, HelperStatus, HostFacts, HostKind, HostRecord, OrderId, Platform,
    PlatformClass, Release, Work,
};
use super::ports::{FactsSource, FarmView, SourceError, Transport, TransportError};
use super::rows::Rows;
use crate::farm::{Farm, NodeAction, NodeLook};

/// Which confirmation order id went to which host, for which activation.
#[derive(Debug, Default)]
struct Issued {
    sent: BTreeMap<WorkerId, Vec<Sent>>,
}

/// An order this process sent to a host.
#[derive(Clone, Debug)]
struct Sent {
    /// The id on the wire: the activation's own, or a confirmation's.
    order: OrderId,
    /// The activation it is about.
    activation: OrderId,
    /// The newest order id the host had acted on when the order was first sent. A status that
    /// still names this id says nothing about the order: the id is an older one that happens
    /// to be the same number (a restored store counts ids again), unless the host's installed
    /// release is the order's (a controller that restarted gives its orders again).
    baseline: u64,
    /// The release the order names.
    commit: String,
}

impl Issued {
    /// Records `sent` for `host` unless that order was recorded already: a repeat of an order
    /// keeps the baseline of its first sending.
    ///
    /// A newer activation supersedes the older ones of the host: its orders are answered no
    /// more, so they are dropped, and a host holds the orders of one activation (its own and
    /// its confirmation).
    fn note(&mut self, host: &WorkerId, sent: Sent) {
        let all = self.sent.entry(host.clone()).or_default();
        if all.iter().any(|s| s.order == sent.order) {
            return;
        }
        all.retain(|s| s.activation >= sent.activation);
        all.push(sent);
    }

    /// Forgets every host `present` does not name.
    fn keep_only(&mut self, present: &[WorkerId]) {
        self.sent.retain(|host, _| present.contains(host));
    }
}

type Shared = Arc<Mutex<Issued>>;

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Issued> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The transport and the facts source over `farm`, with the rows of its state store.
#[must_use]
pub fn ports(farm: Arc<Farm>, rows: Rows) -> (SessionTransport, FarmFacts) {
    let issued = Shared::default();
    (
        SessionTransport {
            farm: Arc::clone(&farm),
            rows: rows.clone(),
            issued: Arc::clone(&issued),
        },
        FarmFacts { farm, rows, issued },
    )
}

/// The orders, over the sessions.
#[derive(Debug)]
pub struct SessionTransport {
    farm: Arc<Farm>,
    rows: Rows,
    issued: Shared,
}

fn failed(why: impl std::fmt::Display) -> TransportError {
    TransportError::new(why.to_string())
}

impl SessionTransport {
    /// Refuses an order whose epoch is not the one `Welcome` names.
    fn check_epoch(&self, order: OrderId) -> Result<(), TransportError> {
        let welcome = self.farm.lease_epoch();
        if order.epoch == welcome {
            Ok(())
        } else {
            Err(failed(format!(
                "order {}.{} is of epoch {}; the sessions' epoch is {welcome}",
                order.epoch, order.seq, order.epoch
            )))
        }
    }

    /// How long the host may take to confirm after the flip: the step's wait for the
    /// activation and for the confirmation, in seconds. A Mac arms its revert timer with it
    /// (clamped by the helper); a Linux helper ignores it.
    fn window_s(&self) -> Result<u32, TransportError> {
        let policy = self.rows.policy().map_err(failed)?;
        let window = policy.activate_limit.saturating_add(policy.health_limit);
        Ok(u32::try_from(window.as_secs()).unwrap_or(u32::MAX))
    }

    /// The newest order id the host's status says it acted on; 0 before a status.
    fn acked_now(&self, host: &WorkerId) -> u64 {
        self.farm
            .update_look()
            .into_iter()
            .find(|n| n.id == *host)
            .and_then(|n| n.update)
            .map_or(0, |u| u.acked_order_id)
    }

    fn send(
        &self,
        host: &WorkerId,
        verb: Verb,
        order_id: u64,
        epoch: u64,
        release: &Release,
        window_s: u32,
    ) -> Result<(), TransportError> {
        let order = UpdateOrder {
            order_id,
            epoch,
            verb: verb as i32,
            commit: release.as_str().to_owned(),
            window_s,
        };
        self.not_handed()?;
        self.farm.send_order(host, order).map_err(failed)
    }

    /// Places `host` as the scheduler does for an operator, unless the farm is handed over.
    fn place(&self, host: &WorkerId, action: NodeAction) -> Result<(), TransportError> {
        self.not_handed()?;
        self.farm.place(host, action).map(drop).map_err(failed)
    }

    /// Refuses an order once the farm is handed over: the successor orders the hosts.
    fn not_handed(&self) -> Result<(), TransportError> {
        if self.farm.handed() {
            Err(failed(
                "this farm is handed over; its successor orders the hosts",
            ))
        } else {
            Ok(())
        }
    }
}

impl Transport for SessionTransport {
    fn cordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.place(host, NodeAction::Cordon)
    }

    fn uncordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.place(host, NodeAction::Uncordon)
    }

    fn mark_down(&mut self, host: &WorkerId, _member: MemberId) -> Result<(), TransportError> {
        Err(failed(format!(
            "cannot mark {host} down: the planned-down mark (U3, #914) is not in this build"
        )))
    }

    fn undown(&mut self, host: &WorkerId, _member: MemberId) -> Result<(), TransportError> {
        Err(failed(format!(
            "cannot undo the mark of {host}: the planned-down mark (U3, #914) is not in this build"
        )))
    }

    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.check_epoch(order)?;
        let window = self.window_s()?;
        let baseline = self.acked_now(host);
        lock(&self.issued).note(
            host,
            Sent {
                order,
                activation: order,
                baseline,
                commit: release.as_str().to_owned(),
            },
        );
        self.send(
            host,
            Verb::Activate,
            order.seq,
            order.epoch,
            release,
            window,
        )
    }

    fn confirm(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.check_epoch(order)?;
        let known = lock(&self.issued)
            .sent
            .get(host)
            .into_iter()
            .flatten()
            .find(|s| s.activation == order && s.order != order)
            .map(|s| s.order.seq);
        let id = match known {
            Some(id) => id,
            None => {
                let drawn = self.rows.mint_order(order.epoch).map_err(failed)?;
                lock(&self.issued).note(
                    host,
                    Sent {
                        order: drawn,
                        activation: order,
                        baseline: self.acked_now(host),
                        commit: release.as_str().to_owned(),
                    },
                );
                drawn.seq
            }
        };
        self.send(host, Verb::Confirm, id, order.epoch, release, 0)
    }

    fn alert(&mut self, host: &WorkerId, alert: &Alert) -> Result<(), TransportError> {
        match alert.severity() {
            Severity::Page => {
                tracing::error!(target: "kbf_server::attention", "update: node {host}: {alert:?}");
            }
            Severity::Warn => {
                tracing::warn!(target: "kbf_server::attention", "update: node {host}: {alert:?}");
            }
        }
        Ok(())
    }
}

/// The facts, from the farm.
#[derive(Debug)]
pub struct FarmFacts {
    farm: Arc<Farm>,
    rows: Rows,
    issued: Shared,
}

/// The release a host runs, given the one desired: its own commit, which the desired one
/// stands for when it begins with it. A running commit that is not 12 lowercase hex digits
/// (`unknown`, or any text a host chose: the report keeps its bytes as data) names no
/// release.
fn running(commit: &str, target: Option<&Release>) -> Option<Release> {
    let hex = commit
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if commit.len() != 12 || !hex {
        return None;
    }
    match target {
        Some(target) if target.as_str().starts_with(commit) => Some(target.clone()),
        _ => Some(Release::new(commit)),
    }
}

fn helper_status(state: State) -> Option<HelperStatus> {
    match state {
        State::ActivatedUnconfirmed => Some(HelperStatus::ActivatedUnconfirmed),
        State::Confirmed => Some(HelperStatus::Confirmed),
        State::Reverted => Some(HelperStatus::Reverted),
        State::Refused => Some(HelperStatus::Refused),
        State::Unspecified | State::Idle | State::Fetching | State::Staged | State::Manual => None,
    }
}

impl FarmFacts {
    fn host(
        &self,
        look: NodeLook,
        epoch: u64,
        target: Option<&Release>,
        queue_empty: bool,
    ) -> Option<HostFacts> {
        let (platform, kind) = match look.os.as_str() {
            "macos" => (
                Platform::Mac(PlatformClass::new(format!("macos-{}", look.arch))),
                HostKind::DaemonOnly,
            ),
            // Perhaps a storage half; the join to the member is not available (see the
            // module docs), so the host is unlinked and never started.
            "linux" => (Platform::Linux, HostKind::StorageAndDaemon),
            _ => return None,
        };
        let helper = look.update.as_ref().and_then(|u| {
            let status = helper_status(u.kind)?;
            // The status carries an id and no epoch. It is read as the answer to an order this
            // process sent, under the epoch it sent it, and only if the host had not acked
            // that id already when it was sent; any other id is of an epoch not known (0),
            // which no activation order has.
            let acked = u.acked_order_id;
            let order = lock(&self.issued)
                .sent
                .get(&look.id)
                .into_iter()
                .flatten()
                .find(|s| {
                    s.order == OrderId::new(epoch, acked)
                        && (s.baseline != acked || u.installed_commit == s.commit)
                })
                .map_or(OrderId::new(0, acked), |s| s.activation);
            Some(HelperReport { order, status })
        });
        let work = if look.leases > 0 {
            Work::Leased
        } else if queue_empty {
            Work::QueueEmpty
        } else {
            Work::Waiting
        };
        Some(HostFacts {
            id: look.id,
            kind,
            platform,
            member: None,
            release: look
                .update
                .as_ref()
                .and_then(|u| running(&u.running_commit, target)),
            connected: look.connected,
            cordoned: Some(look.cordoned),
            load: Some(DaemonLoad {
                leases: look.leases,
                undelivered: look.undelivered,
                whole_machine_hold: look.whole_machine_hold,
            }),
            open_writes: None,
            routed_writes: None,
            helper,
            work: Some(work),
            record: HostRecord::unstarted(),
        })
    }
}

impl FactsSource for FarmFacts {
    fn look(&mut self, _now: FarmTime) -> Result<FarmView, SourceError> {
        let target = self
            .rows
            .desired()
            .map_err(|e| SourceError(e.to_string()))?
            .value
            .map(|d| d.commit);
        let epoch = self.farm.lease_epoch();
        let queue_empty = self.farm.queued_operations() == 0;
        let looks = self.farm.update_look();
        let present: Vec<WorkerId> = looks.iter().map(|l| l.id.clone()).collect();
        lock(&self.issued).keep_only(&present);
        let hosts = looks
            .into_iter()
            .filter_map(|look| self.host(look, epoch, target.as_ref(), queue_empty))
            .collect();
        Ok(FarmView {
            hosts,
            members: Vec::new(),
            // An empty queue asks for no class. A queue with work in it is not read by
            // class here, so it is unknown, and the last Mac of a class is never cordoned.
            queued_classes: queue_empty.then(BTreeSet::new),
        })
    }
}

#[cfg(test)]
mod tests {
    use kbf_caps::NodeCaps;
    use kbf_front::{Cache, MemoryMetaLog};
    use kbf_objstore::MemoryStore;
    use kbf_proto::worker::ServerMessage;
    use kbf_state::fake::{Fake, Flaw};
    use kbf_state::{ManualClock, Store};
    use kbf_types::Resources;
    use tokio::sync::mpsc;

    use super::*;
    use crate::farm::Reported;

    const A: &str = "0123456789abcdef0123456789abcdef01234567";

    fn farm() -> Arc<Farm> {
        Arc::new(Farm::new(
            Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        ))
    }

    fn rows() -> Rows {
        let clock = Arc::new(ManualClock::default());
        Rows::new(Arc::new(Fake::with_flaw(clock, Flaw::None)) as Arc<dyn Store>)
    }

    /// Catches a record that grows with every activation of a host (a long-lived frontend
    /// keeps a line per order for ever), one that forgets the order it still waits on, and
    /// one that keeps a host that is no longer in the farm's look. A thousand activations of
    /// one host leave the orders of the last (its own, and its confirmation); an older
    /// activation's orders are not answered after a newer is sent; a host the look does not
    /// name is forgotten.
    #[test]
    fn the_record_of_sent_orders_stays_bounded() {
        let farm = farm();
        let (outbound, _inbound) =
            mpsc::unbounded_channel::<Result<ServerMessage, tonic::Status>>();
        let caps = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")]).expect("caps");
        farm.register(
            &WorkerId::new("mac"),
            kbf_sched::DaemonInstance::new("mac"),
            kbf_sched::DaemonInstance::default(),
            Reported::serving(Resources::new(1_000, 1 << 30), caps),
            outbound,
            ServerMessage::default(),
        );
        let rows = rows();
        let (mut transport, mut source) = ports(Arc::clone(&farm), rows.clone());
        let epoch = farm.lease_epoch();
        let release = Release::new(A);
        let host = WorkerId::new("mac");
        for _ in 0..1_000 {
            // Every id comes from the one counter, as the controller's do.
            let order = rows.mint_order(epoch).expect("an id");
            transport.activate(&host, order, &release).expect("sent");
            transport.confirm(&host, order, &release).expect("sent");
        }
        let held = |source: &FarmFacts, host: &WorkerId| {
            lock(&source.issued).sent.get(host).map_or(0, Vec::len)
        };
        assert_eq!(
            held(&source, &host),
            2,
            "the last activation and its confirmation"
        );
        // An older activation, sent late, is not recorded over the newer.
        transport
            .activate(&host, OrderId::new(epoch, 5), &release)
            .expect("sent");
        assert_eq!(held(&source, &host), 3);
        // A host that is not in the farm: the activation does not go, and the record goes.
        let ghost = WorkerId::new("ghost");
        transport
            .activate(&ghost, OrderId::new(epoch, 7), &release)
            .expect_err("unknown");
        assert_eq!(held(&source, &ghost), 1);
        source.look(FarmTime::from_millis(0)).expect("a look");
        assert_eq!(held(&source, &ghost), 0);
        assert_eq!(held(&source, &host), 3);
    }

    /// Catches ports that cannot drive the controller in this process (the integration
    /// tests run it in another binary): over a farm with one idle Mac and a desired release,
    /// the first look starts the host and the second moves it on.
    #[test]
    fn a_controller_over_the_ports_moves_an_idle_mac() {
        use crate::update::{Controller, HostPhase};
        let farm = farm();
        let (outbound, _inbound) =
            mpsc::unbounded_channel::<Result<ServerMessage, tonic::Status>>();
        let caps = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")]).expect("caps");
        let stream = farm.register(
            &WorkerId::new("mac"),
            kbf_sched::DaemonInstance::new("mac"),
            kbf_sched::DaemonInstance::default(),
            Reported::serving(Resources::new(1_000, 1 << 30), caps),
            outbound,
            ServerMessage::default(),
        );
        let status = kbf_proto::update::UpdateStatus {
            running_commit: "fedcba987654".to_owned(),
            ..kbf_proto::update::UpdateStatus::default()
        };
        farm.update_status(&WorkerId::new("mac"), stream, status);
        let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        let rows = Rows::new(Arc::clone(&store));
        rows.offer(&Release::new(A), farm.wall_now())
            .expect("a run");
        let (transport, source) = ports(Arc::clone(&farm), rows);
        let mut controller = Controller::new(
            store,
            farm.lease_epoch(),
            Box::new(transport),
            Box::new(source),
        );
        let first = controller.tick(farm.wall_now()).expect("a look");
        assert_eq!(first.moved.len(), 1, "{first:?}");
        let second = controller.tick(farm.wall_now()).expect("a look");
        assert_eq!(
            second.moved.first().map(|m| m.to.clone()),
            Some(HostPhase::Cordoned)
        );
    }
}
