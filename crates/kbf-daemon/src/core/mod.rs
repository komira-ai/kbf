//! The lease core of the daemon: the state that decides which leases run, report and
//! fence, and nothing that names the storage map, the data plane's counters, a driver's
//! report or the node's memory. [`crate::Daemon`] owns one and drives it from its session
//! loop; the loop's wakes and the messages on the stream are the daemon's.
//!
//! What it holds: the leases and their runs, the contact with the server (the fence's
//! clock), the window in which a Start may run, the Results the server has not
//! acknowledged, the lease epoch of the newest Welcome, and what each lease was granted
//! under. Everything it does to an observer it does through [`Event`].

mod event;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use kbf_proto::google::rpc::Code;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{self, DaemonMessage, daemon_message};
use kbf_types::LeaseId;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::acks;
use crate::clock::{Clock, Moment};
use crate::contact::Contact;
use crate::lease::{Done, Leases, failure, lease_id};
use crate::lease_minutes::LeaseMinutes;
use crate::window::StartWindow;

pub use event::Event;

/// The longest a heartbeat waits for the runtime's reading: past it the beat goes out
/// without a load, so a hung cgroup file cannot stop the stream's heartbeats.
const NODE_LOAD_DEADLINE: Duration = Duration::from_secs(2);

/// What the daemon remembers of a lease's Start until it forgets the lease.
pub(crate) struct Granted {
    /// The lease epoch in force when the Start arrived.
    pub(crate) epoch: Option<u64>,
    /// The action the Start named, which the lease's Result echoes.
    pub(crate) action: Option<Digest>,
}

/// The generic session state of a daemon: see the module documentation.
pub struct LeaseCore {
    pub(crate) leases: Leases,
    pub(crate) contact: Contact,
    /// Send times of this stream's Hello and heartbeats, which Starts name.
    pub(crate) window: StartWindow,
    /// Results the server has not acknowledged yet, by lease.
    pub(crate) unacked: BTreeMap<LeaseId, worker::Result>,
    /// The lease epoch the newest Welcome named; `None` before the first, or when it
    /// named none (0).
    pub(crate) epoch: Option<u64>,
    /// Each lease acted on and not yet forgotten: the epoch of its Start and the action
    /// it named.
    pub(crate) granted: BTreeMap<LeaseId, Granted>,
    /// The drain's state as the loops last took it: set once the first signal came.
    pub(crate) draining: bool,
    /// How many Results are kept unacknowledged before one is sent once and dropped.
    max_unacked: usize,
    /// The suspend-counting clock that contact and the Start window read.
    pub(crate) clock: Arc<dyn Clock>,
    /// The kinds of failed node-load reading already logged, so a read that fails on
    /// every heartbeat is logged once.
    load_errors: BTreeSet<&'static str>,
    /// The closed minutes and start clocks that heartbeats carry.
    pub(crate) minutes: LeaseMinutes,
    events: Option<mpsc::UnboundedSender<Event>>,
}

impl LeaseCore {
    pub(crate) fn new(
        leases: Leases,
        fence_after: Duration,
        max_unacked: usize,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            leases,
            contact: Contact::new(fence_after),
            window: StartWindow::default(),
            unacked: BTreeMap::new(),
            epoch: None,
            granted: BTreeMap::new(),
            draining: false,
            max_unacked,
            clock,
            load_errors: BTreeSet::new(),
            minutes: LeaseMinutes::new(),
            events: None,
        }
    }

    /// The node's load for a Heartbeat, from the runtime that takes readings, with the
    /// start of each running lease and the minutes that closed since the last batch was
    /// acknowledged ([`LeaseMinutes`]). A reading that fails, or does not come within
    /// [`NODE_LOAD_DEADLINE`], leaves the load unset and takes nothing from the collector;
    /// each kind of failure is logged the first time it happens.
    pub(crate) async fn node_load(&mut self, seq: u64) -> Option<worker::NodeLoad> {
        // Dropping the read at the deadline does not stop a read running on the blocking pool,
        // and cannot pile such reads up: the container runtime's gate stays held by that
        // blocking call until it returns, so the next beat waits (on a lock, not on a thread)
        // instead of starting a second read.
        let read = timeout(NODE_LOAD_DEADLINE, self.leases.runtimes.node_load()).await;
        match read {
            Ok(Ok(Some(reading))) => {
                let attached = self.minutes.beat(seq, &reading);
                let mut load = reading.load;
                load.lease_clocks = attached.clocks;
                load.covered = attached.covered;
                load.minutes = attached.minutes;
                Some(load)
            }
            Ok(Ok(None)) => None,
            Ok(Err(error)) => {
                if self.load_errors.insert(error.kind) {
                    tracing::warn!(%error, "node load not read: heartbeats go without it");
                }
                None
            }
            Err(_) => {
                if self.load_errors.insert("deadline") {
                    tracing::warn!(
                        deadline = ?NODE_LOAD_DEADLINE,
                        "node load not read in time: heartbeats go without it"
                    );
                }
                None
            }
        }
    }

    /// A stream began: its contact, its Start window and its lease usage start over.
    /// `usage_window` is the sampling window of the stream's heartbeats.
    pub(crate) fn new_stream(
        &mut self,
        interval: Duration,
        usage_window: Duration,
        hello_sent: Moment,
    ) {
        self.contact.new_stream(interval);
        self.window.new_stream(hello_sent);
        self.minutes.new_stream(usage_window);
    }

    /// Tells the runtimes what a Welcome said of placement by measured load (`Welcome.placement_load`,
    /// false when absent).
    pub(crate) async fn welcomed_placement_load(&self, on: bool) {
        self.leases.runtimes.welcomed_placement_load(on).await;
    }

    /// Sends every [`Event`] to `events`.
    pub(crate) fn observe(&mut self, events: mpsc::UnboundedSender<Event>) {
        self.events = Some(events);
    }

    pub(crate) fn emit(&self, event: Event) {
        if let Some(events) = &self.events {
            // An observer that went away does not stop the daemon.
            let _ = events.send(event);
        }
    }

    /// Whether the daemon is draining and has nothing left to wait for: no lease runs
    /// and every Result is acknowledged.
    pub(crate) fn drained(&self) -> bool {
        self.draining && self.leases.running().is_empty() && self.unacked.is_empty()
    }

    /// Takes the acknowledgement of heartbeat `seq`.
    pub(crate) fn on_heartbeat_ack(&mut self, seq: u64) {
        self.window.acknowledged(seq);
        self.minutes.acked(seq);
        if self.contact.acknowledged(seq).restored {
            tracing::info!("contact restored");
            self.emit(Event::ContactRestored);
        }
    }

    /// Takes the server's decision on a Result: the daemon forgets it.
    pub(crate) fn on_result_ack(&mut self, ack: &worker::ResultAck) {
        if let Some(id) = ack.lease_id.map(lease_id) {
            self.unacked.remove(&id);
            self.granted.remove(&id);
            let reason = acks::reason(ack);
            acks::log(id, ack.accepted, reason);
            self.emit(Event::ResultAcknowledged {
                lease: id,
                accepted: ack.accepted,
                reason,
            });
        }
    }

    /// Kills the lease a Cancel names, or forgets it when it does not run here.
    pub(crate) fn on_cancel(&mut self, cancel: &worker::Cancel) {
        if let Some(id) = cancel.lease_id.map(lease_id) {
            if self.leases.cancel(id) {
                self.emit(Event::Cancelled(id));
            } else {
                self.leases.forget(id);
            }
        }
    }

    /// Takes the lease epoch a Welcome named. When it names one, every lease granted
    /// under another is dropped: its run is killed and its Result forgotten, unsent
    /// (issue #137). A Welcome that names none drops nothing, and a lease granted while
    /// none was named is kept.
    pub(crate) fn new_epoch(&mut self, epoch: Option<u64>) {
        self.epoch = epoch;
        let stale: Vec<LeaseId> = self
            .granted
            .keys()
            .copied()
            .filter(|id| self.superseded(*id))
            .collect();
        if stale.is_empty() {
            return;
        }
        let running = self.leases.running();
        for id in &stale {
            self.unacked.remove(id);
            if running.contains(id) {
                // Its Result is dropped when the run ends (`report`).
                self.leases.cancel(*id);
            } else {
                self.granted.remove(id);
            }
        }
        tracing::warn!(?stale, ?epoch, "leases of an earlier epoch dropped");
        self.emit(Event::Superseded(stale));
    }

    /// Whether lease `id` was granted under a lease epoch other than the newest
    /// Welcome's, when that Welcome named one.
    fn superseded(&self, id: LeaseId) -> bool {
        superseded(self.granted.get(&id).and_then(|g| g.epoch), self.epoch)
    }

    /// Runs the lease a Start names, unless its Result is still unacknowledged, it
    /// arrived after its window, or contact is lost (then it is refused with a Result).
    /// A lease it acts on is remembered ([`Self::remember`]).
    pub(crate) fn on_start(&mut self, start: worker::Start, tx: &UnboundedSender<DaemonMessage>) {
        let done = start.lease_id.map(lease_id);
        if let Some(id) = done.filter(|id| self.unacked.contains_key(id)) {
            // Its Result stands until acknowledged; running it again could
            // produce a second one.
            tracing::warn!(lease = %id, "a Start for a lease already reported ignored");
            return;
        }
        let valid_for = Duration::from_millis(start.valid_for_ms);
        if !self
            .window
            .allows(start.heartbeat_seq, valid_for, self.clock.now())
        {
            // The scheduler may have given the lease up and granted it again; it
            // gives it up here too, as a Start that never arrived.
            if let Some(id) = done {
                tracing::warn!(lease = %id, "a Start that arrived after its window not run");
                self.emit(Event::StartExpired(id));
            }
            return;
        }
        self.remember(done, &start);
        let refused = if self.contact.lost(self.clock.now()) {
            start.lease_id.map(|id| {
                failure(
                    lease_id(id),
                    Code::Unavailable,
                    "contact with the server is lost; not starting",
                )
            })
        } else if self.draining {
            start.lease_id.map(|id| {
                failure(
                    lease_id(id),
                    Code::Unavailable,
                    "the node is draining; not starting",
                )
            })
        } else {
            self.leases.start(start)
        };
        if let Some(result) = refused {
            self.report(Some(tx), result);
        }
    }

    /// Remembers lease `id` of `start`, if the Start names one and it is not remembered
    /// already: the lease epoch in force and the action the Start named.
    fn remember(&mut self, id: Option<LeaseId>, start: &worker::Start) {
        if let Some(id) = id {
            let granted = Granted {
                epoch: self.epoch,
                action: start.action_digest.clone(),
            };
            self.granted.entry(id).or_insert(granted);
        }
    }

    /// Declares a heartbeat gap, and fences, when their deadlines have passed. A fence
    /// returns at once: each fenced lease reports through `done` once its run stops.
    pub(crate) fn recheck(&mut self) {
        let now = self.clock.now();
        if let Some(silent_for) = self.contact.check_gap(now) {
            tracing::warn!(?silent_for, "heartbeat gap: no acknowledgement");
            self.emit(Event::HeartbeatGap { silent_for });
        }
        if !self.contact.lost(now) {
            return;
        }
        let fenced = self.leases.fence();
        if !fenced.is_empty() {
            self.emit(Event::Fenced(fenced));
        }
    }

    /// Reports a finished run.
    pub(crate) fn finished(
        &mut self,
        tx: Option<&UnboundedSender<DaemonMessage>>,
        (id, outcome, usage, phases): Done,
    ) {
        let result = self.leases.finished(id, outcome, usage, &phases);
        self.minutes.lease_ended(id);
        self.emit(Event::LeaseEnded(phases));
        self.report(tx, result);
    }

    /// How long to sleep before [`Self::recheck`] may have something to do: until its
    /// next deadline, but never longer than `recheck_every`, since the sleep runs on a
    /// clock that stops during suspend and the deadline on one that does not.
    pub(crate) fn until_recheck(&self, recheck_every: Duration) -> Duration {
        let fence = if !self.leases.unfenced() {
            None
        } else {
            self.contact.fence_deadline()
        };
        [fence, self.contact.gap_deadline()]
            .into_iter()
            .flatten()
            .min()
            .map_or(Duration::from_secs(3600), |deadline| {
                deadline
                    .saturating_duration_since(self.clock.now())
                    .min(recheck_every)
            })
    }

    /// Keeps `result` until its lease's ResultAck, and sends it on `tx` if a stream is
    /// up. Without one it goes out after the next Welcome. It echoes the action its
    /// lease's Start named. The Result of a lease of an earlier epoch is dropped.
    pub(crate) fn report(
        &mut self,
        tx: Option<&UnboundedSender<DaemonMessage>>,
        mut result: worker::Result,
    ) {
        // Every Result here names its lease: the lease manager builds them all.
        let id = result.lease_id.map_or(LeaseId::new(0, 0), lease_id);
        if self.superseded(id) {
            tracing::warn!(lease = %id, "the Result of a lease of an earlier epoch dropped");
            self.granted.remove(&id);
            return;
        }
        result.action_digest = self.granted.get(&id).and_then(|g| g.action.clone());
        if let Some(tx) = tx {
            send(tx, daemon_message::Message::Result(result.clone()));
        }
        if self.unacked.len() >= self.max_unacked {
            tracing::error!(lease = %id, kept = self.unacked.len(), "too many unacknowledged results: this one is sent once and not kept");
            self.granted.remove(&id);
            return;
        }
        self.unacked.insert(id, result);
    }

    /// What a Heartbeat lists as running: the leases running now, and those whose
    /// Result the server has not acknowledged (issue #26).
    pub(crate) fn listed(&self) -> BTreeSet<LeaseId> {
        self.leases
            .running()
            .into_iter()
            .chain(self.unacked.keys().copied())
            .collect()
    }
}

/// Queues a message on the stream. A message for a stream that is gone is dropped: a
/// Result stays in the daemon's unacknowledged set and is resent on the next stream.
pub(crate) fn send(tx: &UnboundedSender<DaemonMessage>, message: daemon_message::Message) {
    let _ = tx.unbounded_send(DaemonMessage {
        message: Some(message),
    });
}

/// Whether a lease granted under the lease epoch `granted` is dropped once the newest
/// `Welcome` named `newest` (issue #137): only when both name one and they differ. A
/// Welcome that names none drops nothing, and a lease granted while none was named is
/// kept. The daemon's rule, public so a simulated daemon follows the same one.
#[must_use]
pub fn superseded(granted: Option<u64>, newest: Option<u64>) -> bool {
    newest.is_some() && granted.is_some() && granted != newest
}

#[cfg(test)]
#[path = "lease_minutes_tests.rs"]
mod lease_minutes_tests;
