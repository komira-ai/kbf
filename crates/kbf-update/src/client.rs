//! The update client: a [`Handle`] the daemon's session feeds with the frontend's offers
//! and orders, and a [`Worker`] that watches the installer's status file.
//!
//! What the client decides, and how:
//!
//! - **Offers** are level-triggered and only remembered: one is acted on only if its
//!   generation is above every generation seen, so the frontend may send the same offer
//!   after every Welcome. An offer that fails [`Offer::check`] is refused with the reason.
//!   The newest few are kept by commit, because an activation order carries only a commit
//!   and the record carries the offer's components for it. Nothing is fetched here.
//! - **Orders** are accepted only from the epoch of the current session, above every
//!   order id accepted before, and an activation only for a commit whose offer this node
//!   holds and with no lease live. Confirm and revert are for a commit the installer
//!   reports as held. An accepted order is written to the order spool as a record and
//!   nothing else is done with it. An accepted activation closes the update gate.
//! - **Status** is derived from what the client and the installer's status file say, and
//!   published on a watch channel after every change.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;

use crate::floors::{Floors, FloorsError, FloorsFile};
use crate::helper::{self, Helper, HelperState, Report};
use crate::record::OrderRecord;
use crate::spool::Spool;
use crate::types::{Offer, Order, State, Status, Verb};

/// How many offers are remembered, oldest generation first out.
const KEEP_OFFERS: usize = 4;

/// How a client is set up.
#[derive(Clone)]
pub struct Config {
    /// The installer's directory: its `status.json` is read from here.
    pub dir: PathBuf,
    /// The order spool, owned by the service user. It also holds this client's own small
    /// state file, because the service user can write nowhere else.
    pub orders_dir: PathBuf,
    /// The platform of this node, as offers name it.
    pub platform: String,
    /// The commit this process was built from.
    pub running_commit: String,
    /// How often the installer's status file is read.
    pub poll_every: Duration,
}

impl Config {
    /// The configuration of the node `platform` running `running_commit`, with the
    /// installer's directory `dir` and its order spool `dir/orders`.
    #[must_use]
    pub fn new(dir: PathBuf, platform: String, running_commit: String) -> Self {
        Self {
            orders_dir: dir.join("orders"),
            dir,
            platform,
            running_commit,
            poll_every: Duration::from_secs(2),
        }
    }
}

/// Why a client cannot start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("update directory {path}: {source}")]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    State(#[from] FloorsError),
}

/// What the client did with an order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Written to the order spool; the status acknowledges it.
    Accepted,
    /// The same order again: nothing written, the status acknowledges it again.
    Replayed,
    /// Not written, and why.
    Refused(String),
}

impl Decision {
    /// Why the order was refused; `None` when it was not.
    #[must_use]
    pub fn refusal(&self) -> Option<&str> {
        match self {
            Self::Refused(why) => Some(why),
            Self::Accepted | Self::Replayed => None,
        }
    }
}

/// The update gate: closed from an accepted activation until the helper settles it.
#[derive(Debug)]
struct Gate {
    order_id: u64,
    commit: String,
}

/// What the client knows, under one lock.
struct Core {
    /// The newest offer generation acted on, refused ones included.
    seen: u64,
    floors: Floors,
    /// The commit of the newest accepted offer.
    offered_commit: Option<String>,
    /// The offers this node can order an activation for, oldest generation first.
    offers: Vec<Offer>,
    refusal: Option<String>,
    acked_order: u64,
    helper: Helper,
    gate: Option<Gate>,
}

impl Core {
    /// Remembers `offer` if it is newer than any seen. Whether it was taken (and so the
    /// generation floor moved). The same generation again is kept (a restart loses the
    /// memory of it) but changes nothing else.
    fn offer(&mut self, offer: Offer, platform: &str) -> bool {
        if offer.generation < self.seen {
            return false;
        }
        let repeat = offer.generation == self.seen;
        if !repeat {
            self.seen = offer.generation;
        }
        if let Err(why) = offer.check(platform) {
            if !repeat {
                tracing::warn!(generation = offer.generation, %why, "offer refused");
                self.refusal = Some(why.to_string());
            }
            return false;
        }
        if !repeat {
            self.refusal = None;
            self.offered_commit = Some(offer.commit.clone());
        }
        self.offers.retain(|held| held.commit != offer.commit);
        self.offers.push(offer);
        self.offers.sort_by_key(|held| held.generation);
        let extra = self.offers.len().saturating_sub(KEEP_OFFERS);
        self.offers.drain(..extra);
        !repeat
    }

    /// Opens the gate again when the helper has settled the order that closed it
    /// without restarting this process.
    fn settle_gate(&mut self) {
        let Some(gate) = &self.gate else { return };
        if let Helper::Reported(report) = &self.helper
            && report.acked_order_id >= gate.order_id
            && matches!(report.state, HelperState::Reverted | HelperState::Refused)
        {
            tracing::info!(order = gate.order_id, "the gate opens");
            self.gate = None;
        }
    }

    fn report(&self) -> Option<&Report> {
        match &self.helper {
            Helper::Reported(report) => Some(report),
            Helper::Missing | Helper::Unreadable(_) => None,
        }
    }

    /// The state and the reason for it.
    fn state(&self) -> (State, String) {
        if let Helper::Unreadable(why) = &self.helper {
            return (State::Refused, why.clone());
        }
        if let Some(why) = &self.refusal {
            return (State::Refused, why.clone());
        }
        let Some(report) = self.report() else {
            return (State::Manual, String::new());
        };
        let state = match report.state {
            HelperState::Idle => State::Idle,
            HelperState::Fetching => State::Fetching,
            HelperState::Staged if report.staged == self.offered_commit => State::Staged,
            HelperState::Staged => State::Idle,
            HelperState::ActivatedUnconfirmed => State::ActivatedUnconfirmed,
            HelperState::Confirmed => State::Confirmed,
            HelperState::Reverted => State::Reverted,
            HelperState::Refused => return (State::Refused, report.refusal.clone()),
        };
        (state, String::new())
    }

    fn status(&self, running_commit: &str) -> Status {
        let (state, refusal) = self.state();
        let report = self.report();
        Status {
            running_commit: running_commit.to_owned(),
            installed_commit: report.and_then(|r| r.installed.clone()),
            staged_commit: report.and_then(|r| r.staged.clone()),
            state,
            refusal,
            acked_order_id: self.acked_order,
            offered_generation: self.seen,
            helper_present: self.helper != Helper::Missing,
            staged: report.and_then(|r| r.staged_facts),
            gated: self.gate.is_some(),
        }
    }
}

struct Shared {
    config: Config,
    spool: Spool,
    floors: FloorsFile,
    core: Mutex<Core>,
    status: watch::Sender<Status>,
}

impl Shared {
    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, core: &Core) {
        let now = core.status(&self.config.running_commit);
        self.status.send_if_modified(|status| {
            let changed = *status != now;
            if changed {
                *status = now;
            }
            changed
        });
    }

    /// Reads the installer's status file again.
    fn refresh(&self) {
        let mut core = self.core();
        core.helper = helper::read(&self.config.dir.join("status.json"));
        core.settle_gate();
        self.publish(&core);
    }

    fn offer(&self, offer: Offer) {
        let mut core = self.core();
        let generation = offer.generation;
        if core.offer(offer, &self.config.platform) {
            tracing::info!(generation, "offer remembered");
            core.floors.generation = core.floors.generation.max(generation);
            if let Err(e) = self.floors.save(&core.floors) {
                tracing::warn!(error = %e, "the newest generation could not be recorded");
            }
        }
        self.publish(&core);
    }

    fn order(&self, order: &Order, epoch: u64, live: &dyn Fn() -> usize) -> Decision {
        let mut core = self.core();
        core.helper = helper::read(&self.config.dir.join("status.json"));
        let decision = self.decide(&mut core, order, epoch, live);
        match &decision {
            Decision::Refused(why) => {
                tracing::warn!(order = order.order_id, %why, "order refused");
                core.refusal = Some(why.clone());
            }
            Decision::Accepted | Decision::Replayed => core.refusal = None,
        }
        self.publish(&core);
        decision
    }

    fn decide(
        &self,
        core: &mut Core,
        order: &Order,
        epoch: u64,
        live: &dyn Fn() -> usize,
    ) -> Decision {
        let id = order.order_id;
        if order.epoch != epoch {
            return Decision::Refused(format!(
                "order {id} is of epoch {}; this session's is {epoch}",
                order.epoch
            ));
        }
        // The same order again is answered, not written twice. (If its file is missing,
        // the first try died between recording the id and writing it: write it now.)
        if id > 0
            && id == core.floors.order_id
            && epoch == core.floors.order_epoch
            && self.spool.has(epoch, id)
        {
            core.acked_order = id;
            return Decision::Replayed;
        }
        // Epoch first, then id: a restored store raises the epoch and counts ids again, so
        // a lower id of a newer epoch is newer, and any id of an older epoch is not.
        if epoch < core.floors.order_epoch {
            return Decision::Refused(format!(
                "order {id} is of epoch {epoch}, older than the newest accepted, {}",
                core.floors.order_epoch
            ));
        }
        if id == 0 || (epoch == core.floors.order_epoch && id < core.floors.order_id) {
            return Decision::Refused(format!(
                "order id {id} is not above the newest accepted, {}",
                core.floors.order_id
            ));
        }
        let Some(verb) = order.verb else {
            return Decision::Refused(format!("order {id} names no verb"));
        };
        if verb == Verb::FrontendUpdate {
            return Decision::Refused(format!(
                "order {id} is a frontend update, which a node does not take from the session"
            ));
        }
        let Some(report) = core.report() else {
            return Decision::Refused(
                "no update installer reports on this host, so it cannot take an order".to_owned(),
            );
        };
        let activate = verb == Verb::Activate;
        // An activation names a release the installer fetches itself; the other two verbs
        // are about a release the host already holds.
        if !activate && !report.holds(&order.commit) {
            return Decision::Refused(format!(
                "commit {} is neither staged nor installed on this host",
                order.commit
            ));
        }
        let offer = core.offers.iter().find(|o| o.commit == order.commit);
        if activate && offer.is_none() {
            return Decision::Refused(format!(
                "no offer for commit {} is held; not activating",
                order.commit
            ));
        }
        let record = match OrderRecord::for_order(order, verb, offer.filter(|_| activate)) {
            Ok(record) => record,
            Err(e) => return Decision::Refused(format!("order {id} cannot be written: {e}")),
        };
        let leases = if activate { live() } else { 0 };
        if leases > 0 {
            return Decision::Refused(format!("{leases} leases are live; not activating"));
        }
        let floors = Floors {
            order_id: id,
            order_epoch: epoch,
            ..core.floors
        };
        if let Err(e) = self.floors.save(&floors) {
            return Decision::Refused(format!("cannot record order {id}: {e}"));
        }
        core.floors = floors;
        // Immediately before the order is written: the first look may be a while ago.
        let leases = if activate { live() } else { 0 };
        if leases > 0 {
            return Decision::Refused(format!("{leases} leases are live; not activating"));
        }
        if let Err(e) = self.spool.write(&record) {
            return Decision::Refused(format!("cannot write order {id}: {e}"));
        }
        core.acked_order = id;
        match verb {
            Verb::Activate => {
                core.gate = Some(Gate {
                    order_id: id,
                    commit: order.commit.clone(),
                });
            }
            Verb::Revert => core.gate = None,
            // Refused above, before anything is written.
            Verb::Confirm | Verb::FrontendUpdate => {}
        }
        Decision::Accepted
    }
}

/// The part of the client the daemon's session talks to. Cheap to clone.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}

impl Handle {
    /// Takes an offer the frontend sent and remembers it. Returns at once.
    pub fn offer(&self, offer: Offer) {
        self.shared.offer(offer);
    }

    /// Decides an order the frontend sent. `epoch` is the lease epoch of the current
    /// session's Welcome (0 when it named none); `live` counts the leases that are live
    /// now, and is asked again immediately before an activation is written.
    #[must_use]
    pub fn order(&self, order: &Order, epoch: u64, live: &dyn Fn() -> usize) -> Decision {
        self.shared.order(order, epoch, live)
    }

    /// The status, now and after each change.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<Status> {
        self.shared.status.subscribe()
    }

    /// Reads the helper's status file now, rather than at the worker's next look.
    pub fn refresh(&self) {
        self.shared.refresh();
    }

    /// While the update gate is closed, the reason a lease cannot start.
    #[must_use]
    pub fn gate(&self) -> Option<String> {
        let core = self.shared.core();
        let gate = core.gate.as_ref()?;
        Some(format!("update to {} ordered", &gate.commit[..12]))
    }
}

/// The background half: re-reads the installer's status.
pub struct Worker {
    shared: Arc<Shared>,
}

impl Worker {
    /// Runs until it is dropped.
    pub async fn run(self) {
        let mut tick = tokio::time::interval(self.shared.config.poll_every);
        loop {
            tick.tick().await;
            self.shared.refresh();
        }
    }
}

/// Starts a client.
///
/// # Errors
/// The order spool or the client's state file cannot be used. The client does not
/// start on a state it cannot read.
pub fn start(config: Config) -> Result<(Handle, Worker), StartError> {
    let io = |source| StartError::Directory {
        path: config.orders_dir.clone(),
        source,
    };
    let spool = Spool::open(&config.orders_dir).map_err(io)?;
    let (floors_file, floors) = FloorsFile::open(&config.orders_dir)?;
    let core = Core {
        seen: floors.generation,
        floors,
        offered_commit: None,
        offers: Vec::new(),
        refusal: None,
        // An order recorded and written before a restart is still acknowledged.
        acked_order: if spool.has(floors.order_epoch, floors.order_id) {
            floors.order_id
        } else {
            0
        },
        helper: helper::read(&config.dir.join("status.json")),
        gate: None,
    };
    let (status, _) = watch::channel(core.status(&config.running_commit));
    let shared = Arc::new(Shared {
        config,
        spool,
        floors: floors_file,
        core: Mutex::new(core),
        status,
    });
    Ok((
        Handle {
            shared: Arc::clone(&shared),
        },
        Worker { shared },
    ))
}

#[cfg(test)]
mod harness;
#[cfg(test)]
mod order_tests;
#[cfg(test)]
mod tests;
