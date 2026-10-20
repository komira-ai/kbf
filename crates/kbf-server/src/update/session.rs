//! The controller's two ports over the farm and its worker sessions: [`SessionTransport`]
//! gives the orders ([`Transport`]) and [`FarmFacts`] reads the farm ([`FactsSource`]).
//! [`ports`] builds the pair; they share what they must agree on, which order ids went to
//! which host.
//!
//! **Orders.** An activation or confirmation goes out as an `UpdateOrder` on the host's
//! newest stream, with the epoch of the order id, which must be the epoch `Welcome` named
//! ([`Farm::lease_epoch`]): an order of another epoch is refused here, not sent, because the
//! node would refuse it. A cordon is the scheduler's own ([`Farm::place`]); an alert is a
//! line under the target `kbf_server::attention`. A planned-down mark is
//! [`Members::plan_down`] for as long as the restart deadline of the policy, and its removal
//! [`Members::undo_plan`], which promotes nothing that is not planned down (a member that
//! registered again is left as it is). Neither is given by a farm that is handed over.
//!
//! **A handed-over farm gives no order.** Once the farm is fenced ([`Handed::handed`]: a
//! quiesce or a stand-down) a cordon, an uncordon, an activation and a confirmation are
//! refused here, not given: [`Farm::place`] still writes the cordon row of a fenced farm and
//! [`Farm::send_order`] still reaches the node, and the successor is the one that orders the
//! hosts now. The check is made first in an activation and a confirmation, so that a refused
//! one draws no id from the store's counter and leaves no note in the record of sent orders,
//! and again just before the farm is asked, for a farm fenced while the order was prepared.
//! It is made for a look that was under way when the farm was fenced; the store's generation
//! fence remains what refuses a controller whose handle was superseded.
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
//! `macos-<arch>`) or `linux`. A Linux host is joined to its storage member by the address it
//! advertised when it enrolled (`enroll/node/<id>`, the registry's row for the node id of its
//! `Hello`): the member whose registered address is that text, byte for byte. One member
//! matching makes the host a storage-and-daemon host linked to it. A node the registry
//! knows with no advertised address has no storage half and is a daemon-only host. A node
//! the registry does not know, an address no member registered and an address two members
//! registered are storage-and-daemon hosts linked to no member: the picker never starts
//! them, and [`FarmFacts::unlinked`] says why. A linked host's write counts are its
//! member's newest fresh report: the open writes it made, and no routed write when the
//! report was made under a map at least as new as the current one (the router keeps no
//! count of its own; a node that reported under an older map may still be sent a write).
//! The members' own facts, the boot counter included, are those of [`Members::look`]; the
//! reason a marked member is not idle yet is [`FarmFacts::not_idle`]. What the node reports comes from its newest
//! `UpdateStatus` of its current stream (a stream that replaced it has none until it sends
//! one): the release it runs is the desired release when the 12 digits of its running commit
//! begin the desired commit, else its own commit (none, if that is not 12 lowercase hex
//! digits: the report keeps what a host sent); the helper's report is its state and the
//! order it acted on. `Work` is read from the scheduler: a lease on the node, else an empty
//! queue.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_front::Handed;
use kbf_map::{MemberId, MemberState};
use kbf_proto::update::UpdateOrder;
use kbf_proto::update::update_order::Verb;
use kbf_proto::update::update_status::State;
use kbf_types::{FarmTime, WorkerId};

use super::command::{Alert, Severity};
use super::facts::MemberFacts;
use super::facts::{
    DaemonLoad, HelperReport, HelperStatus, HostFacts, HostKind, HostRecord, OrderId, Platform,
    PlatformClass, Release, Work,
};
use super::ports::{FactsSource, FarmView, SourceError, Transport, TransportError};
use super::rows::Rows;
use crate::farm::{Farm, NodeAction, NodeLook};
use crate::members::{MemberLook, Members, MembersLook, MembershipError};
use crate::storage_health::{Class, NotIdle};

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
pub fn ports(farm: Arc<Farm>, rows: Rows, members: Arc<Members>) -> (SessionTransport, FarmFacts) {
    let issued = Shared::default();
    (
        SessionTransport {
            farm: Arc::clone(&farm),
            rows: rows.clone(),
            members: Arc::clone(&members),
            issued: Arc::clone(&issued),
        },
        FarmFacts {
            farm,
            rows,
            members,
            issued,
            unlinked: BTreeMap::new(),
            not_idle: BTreeMap::new(),
        },
    )
}

/// The orders, over the sessions.
pub struct SessionTransport {
    farm: Arc<Farm>,
    rows: Rows,
    members: Arc<Members>,
    issued: Shared,
}

impl std::fmt::Debug for SessionTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTransport").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for FarmFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FarmFacts")
            .field("unlinked", &self.unlinked)
            .field("not_idle", &self.not_idle)
            .finish_non_exhaustive()
    }
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
            // The generation of the desired release is sent by a later change (0 is a
            // frontend that predates the field).
            ..UpdateOrder::default()
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

    /// The member is planned down for the policy's restart deadline: long enough for the
    /// waits of steps 4 and 5 and the activation, short enough that the dwell's end never
    /// decides first. A plan that runs out while the old process still beats returns the
    /// member to `Active` by its next heartbeat.
    fn mark_down(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.not_handed()?;
        let plan = self.rows.policy().map_err(failed)?.restart_deadline();
        self.members
            .plan_down(member, plan)
            .map(drop)
            .map_err(|e| failed(format!("cannot mark {host} (member {member}) down: {e}")))
    }

    /// A member that is not planned down is left as it is, and one with no live row has
    /// nothing to undo: it stopped, and registering again deletes its mark.
    fn undown(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.not_handed()?;
        match self.members.undo_plan(member) {
            Ok(_) | Err(MembershipError::NotRegistered(_)) => Ok(()),
            Err(e) => Err(failed(format!(
                "cannot undo the mark of {host} (member {member}): {e}"
            ))),
        }
    }

    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.not_handed()?;
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
        self.not_handed()?;
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
pub struct FarmFacts {
    farm: Arc<Farm>,
    rows: Rows,
    members: Arc<Members>,
    issued: Shared,
    /// Why each Linux host that has no member is not linked, as of the last look.
    unlinked: BTreeMap<WorkerId, String>,
    /// Why each marked member's host is not idle yet, as of the last look.
    not_idle: BTreeMap<WorkerId, NotIdle>,
}

/// How a Linux host stands to the storage members.
enum Link<'a> {
    /// The registry knows the node and it advertises no address: no storage half.
    NoStorage,
    /// Its advertised address is the one `member` registered.
    Member(&'a MemberLook),
    /// No member is its own; the text says why.
    Unlinked(String),
}

/// The member whose registered address is the address `host` advertised when it enrolled
/// (`adverts`: node id to address), both read through [`kbf_enroll_registry::canonical_addr`],
/// the one spelling enrollment stores and a member registers. No prefix, no port guessing: an
/// address that is not a member's links nothing.
fn link<'a>(
    host: &WorkerId,
    adverts: &BTreeMap<String, String>,
    members: &'a MembersLook,
) -> Link<'a> {
    let Some(advertised) = adverts.get(host.as_str()) else {
        return Link::Unlinked(format!(
            "{host} is not in the enrollment registry, so its storage address is not known"
        ));
    };
    if advertised.is_empty() {
        return Link::NoStorage;
    }
    let advertised = &kbf_enroll_registry::canonical_addr(advertised);
    let mut matching = members
        .members
        .iter()
        .filter(|m| kbf_enroll_registry::canonical_addr(&m.addr) == *advertised);
    match (matching.next(), matching.next()) {
        (Some(only), None) => Link::Member(only),
        (None, _) => Link::Unlinked(format!(
            "{host} advertises {advertised}, which no storage member registered"
        )),
        (Some(_), Some(_)) => Link::Unlinked(format!(
            "{host} advertises {advertised}, which more than one storage member registered"
        )),
    }
}

/// `now` less `ago`.
fn before(now: FarmTime, ago: std::time::Duration) -> FarmTime {
    let ms = u64::try_from(ago.as_millis()).unwrap_or(u64::MAX);
    FarmTime::from_millis(now.as_millis().saturating_sub(ms))
}

fn member_facts(look: &MembersLook, now: FarmTime) -> Vec<MemberFacts> {
    look.members
        .iter()
        .map(|m| MemberFacts {
            id: m.id,
            state: m.state,
            planned_down: m.planned_down,
            heard_at: m.heard_ago.map(|ago| before(now, ago)),
            repair_debt: m.repair_debt,
            down_since: m.marked_ago.map(|ago| before(now, ago)),
            boot: (m.boot > 0).then_some(m.boot),
        })
        .collect()
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
        link: Option<&Link<'_>>,
        generation: u64,
    ) -> Option<HostFacts> {
        let (platform, kind) = match look.os.as_str() {
            "macos" => (
                Platform::Mac(PlatformClass::new(format!("macos-{}", look.arch))),
                HostKind::DaemonOnly,
            ),
            "linux" => (
                Platform::Linux,
                match link {
                    Some(Link::NoStorage) => HostKind::DaemonOnly,
                    _ => HostKind::StorageAndDaemon,
                },
            ),
            _ => return None,
        };
        let member = match link {
            Some(Link::Member(member)) => Some(*member),
            _ => None,
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
            member: member.map(|m| m.id),
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
            open_writes: member.and_then(|m| m.open_writes),
            // The router keeps no count of the writes it sent a member. A report made under
            // a map at least as new as the current one is of a node every router has
            // stopped sending to if it is down in that map, and the same rule as
            // `Members::node_idle` gives (its `at_generation`). An older or missing report
            // is unknown, which a gate does not read as zero.
            routed_writes: member
                .filter(|m| m.reported_generation.is_some_and(|g| g >= generation))
                .map(|_| 0),
            helper,
            work: Some(work),
            record: HostRecord::unstarted(),
        })
    }
}

impl FarmFacts {
    /// Why `host`, a Linux host, is linked to no storage member as of the last look; `None`
    /// if it is linked or has no storage half.
    #[must_use]
    pub fn unlinked(&self, host: &WorkerId) -> Option<&str> {
        self.unlinked.get(host).map(String::as_str)
    }

    /// Why the storage member of `host`, which is planned down, is not idle yet as of the
    /// last look ([`Members::node_idle`]); `None` if it is idle, or not planned down.
    #[must_use]
    pub fn not_idle(&self, host: &WorkerId) -> Option<&NotIdle> {
        self.not_idle.get(host)
    }

    /// Keeps `now` as the hosts that are unlinked, and says each once, when it first is or its
    /// reason changes.
    fn say_unlinked(&mut self, now: BTreeMap<WorkerId, String>) {
        for (host, why) in &now {
            if self.unlinked.get(host) != Some(why) {
                tracing::warn!(
                    target: "kbf_server::attention",
                    "update: {why}; the host is not started"
                );
            }
        }
        self.unlinked = now;
    }

    /// For each linked member that is planned down, asks `Members::node_idle` and keeps the
    /// first reason it is not; a wait is said at info and a hold at warn, once per reason.
    /// The controller's own gate reads the same facts (`writes_quiet`, the member's state);
    /// this is how a person finds out which it is waiting for.
    fn say_idle(&mut self, hosts: &[HostFacts], members: &MembersLook) {
        let mut now = BTreeMap::new();
        for host in hosts {
            let Some(id) = host.member else { continue };
            let marked = members
                .members
                .iter()
                .any(|m| m.id == id && m.planned_down && m.state == MemberState::Down);
            if !marked {
                continue;
            }
            if let Err(reason) = self.members.node_idle(id, members.generation) {
                if self.not_idle.get(&host.id) != Some(&reason) {
                    match reason.class() {
                        Class::Wait => tracing::info!(
                            "update: {} is marked down and not idle yet: {reason}",
                            host.id
                        ),
                        Class::Held => tracing::warn!(
                            target: "kbf_server::attention",
                            "update: {} is marked down and must not be stopped: {reason}",
                            host.id
                        ),
                    }
                }
                now.insert(host.id.clone(), reason);
            }
        }
        self.not_idle = now;
    }
}

impl FactsSource for FarmFacts {
    fn look(&mut self, now: FarmTime) -> Result<FarmView, SourceError> {
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
        let members = self
            .members
            .look()
            .map_err(|e| SourceError(format!("the members could not be read: {e}")))?;
        let adverts: BTreeMap<String, String> =
            kbf_enroll_registry::list_nodes(&**self.rows.store())
                .map_err(|e| {
                    SourceError(format!("the enrollment registry could not be read: {e}"))
                })?
                .into_iter()
                .map(|node| (node.id.to_string(), node.advertise))
                .collect();
        let mut unlinked = BTreeMap::new();
        let mut hosts = Vec::new();
        for look in looks {
            let linked = (look.os == "linux").then(|| link(&look.id, &adverts, &members));
            if let Some(Link::Unlinked(why)) = &linked {
                unlinked.insert(look.id.clone(), why.clone());
            }
            hosts.extend(self.host(
                look,
                epoch,
                target.as_ref(),
                queue_empty,
                linked.as_ref(),
                members.generation,
            ));
        }
        self.say_unlinked(unlinked);
        self.say_idle(&hosts, &members);
        Ok(FarmView {
            hosts,
            members: member_facts(&members, now),
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

    fn members(rows: &Rows) -> Arc<Members> {
        Arc::new(Members::new(
            Arc::clone(rows.store()),
            crate::members::MembershipConfig::default(),
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
        let (mut transport, mut source) = ports(Arc::clone(&farm), rows.clone(), members(&rows));
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

    /// Catches an activation or a confirmation that, on a handed-over farm, is refused only
    /// at the send: by then a confirmation has drawn an id from the store's counter and both
    /// have left a note for an order that never went out. The farm is fenced before the
    /// first confirmation of the activation (a confirmation already sent would be found in
    /// the record and draw nothing). Mutants seen red: the check removed from the top of
    /// `activate` (a note is left); from the top of `confirm` (a note is left and an id is
    /// drawn; the integration test `a_handed_over_farm_cordons_nobody_and_sends_no_order`
    /// sees the draw as a store transaction).
    #[test]
    fn a_handed_over_farm_records_no_order_and_draws_no_id() {
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
        let (mut transport, source) = ports(Arc::clone(&farm), rows.clone(), members(&rows));
        let epoch = farm.lease_epoch();
        let (host, release) = (WorkerId::new("mac"), Release::new(A));
        let order = rows.mint_order(epoch).expect("an id");
        farm.fence();
        for answer in [
            transport.activate(&host, order, &release),
            transport.confirm(&host, order, &release),
        ] {
            let error = answer.expect_err("refused");
            assert!(error.to_string().contains("handed over"), "{error}");
        }
        assert_eq!(lock(&source.issued).sent.get(&host).map(Vec::len), None);
        assert_eq!(
            rows.mint_order(epoch).expect("an id").seq,
            order.seq + 1,
            "a handed-over farm drew an id"
        );
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
        let members = members(&rows);
        let (transport, source) = ports(Arc::clone(&farm), rows, members);
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
