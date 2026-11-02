//! The scheduler's state in the state store (`docs/design/architecture-v2.md`, sections
//! 4.2 and 4.4): every change is written through [`kbf_state::Store`] before it is
//! acknowledged, and a scheduler that starts over a store takes up what the store holds.
//!
//! Rows:
//!
//! - `op/<id>`: an unfinished operation's request, written at submit, deleted when it
//!   finishes or is refused;
//! - `queue/<id>`: present while the operation waits for a lease. A grant requires it
//!   and deletes it, so one operation never holds two live leases;
//! - `lease/<lease>`: the fencing token of a committed grant, present while the
//!   operation holds the lease. A result is accepted only while it is there, which is the
//!   compare-and-set on the operation's lease;
//! - `session/<worker>`: what the scheduler knows of a daemon's newest session (its
//!   instance, capacity and capabilities). It expires unless renewed: one write per
//!   session per [`SESSION_RENEWAL`], however many leases the daemon holds (a lease row
//!   is written at the grant and at the end, never to renew it);
//! - `owner/<worker>`: the frontend that owns a daemon's node, and a counter that each
//!   `Hello` of the daemon to a frontend raises by one (sixteen bytes, big-endian). A write
//!   of a lease of the node is conditional on it, so a frontend the daemon has left
//!   cannot grant, give up or finish a lease of the node (see the `owner` module);
//! - `lease-by-op/<operation>`: the id of the lease the operation holds, put in the
//!   transaction of the grant and deleted with the lease, so a request that joins a running
//!   operation finds its lease without reading every `lease/` row (see the `leases` module);
//! - `loss/<operation>`: how many of the operation's leases were lost, while it is
//!   unfinished and has lost any ([`kbf_sched::LOSS_REQUEUES`] refuses it);
//! - `fate/<operation>`: what the operation's faults changed: the raised booking and the
//!   memory runs of its kills for memory, the farm runs it used, the nodes placement
//!   avoids for it and the leases given up as not started ([`kbf_sched::Fate`]), written
//!   in the transaction of every requeue that follows a fault (whole, so the newest row
//!   is the whole state) and deleted with the operation's other rows. A successor (another
//!   frontend that takes the operation up, or a restart after a crash) books the next rung
//!   and spends the budgets that are left instead of starting over. A store with no row for
//!   an operation books and counts as it did before the row existed; a row written before
//!   the budgets were (version 1) carries none of them;
//! - `floor/<instance>/<action>`: a memory floor, the least memory a fresh submission of
//!   the action books ([`kbf_sched::Floor`]): the ask and a stamp that orders the raises,
//!   written in the transaction of the requeue that raises it, deleted in the one that
//!   forgets it (the scheduler keeps [`kbf_sched::MEMORY_FLOORS`]), and not deleted with
//!   an operation. A successor loads them all, oldest stamp first, and raises past the
//!   largest stamp, so the order of eviction survives a reload. They have no expiry;
//!   in memory they have none either;
//! - `keyfloor/<key>`: a key floor, the least memory a request carrying the size key
//!   books at a placement round ([`kbf_sched::KeyFloor`]): the ask and a stamp, written
//!   in the transaction of the requeue that raises it beside the `floor/` row, deleted
//!   in the one that forgets it. A store that was never `--learned-sizes=on` has none,
//!   and an older binary does not read the prefix;
//! - `opkey/<operation>`: the size key of an operation ([`kbf_sched::Scheduler::set_size_key`]),
//!   written in the submission's transaction beside `op/` and `queue/` and deleted with
//!   them, only for an operation given one (`--learned-sizes=on`). It is in a row of its
//!   own, not in the request, so no row of an older format changes and a binary that does
//!   not know the prefix reads the store as it always did;
//! - `opwall/<operation>`: the longest the operation may run, the `timeout` of its action
//!   ([`kbf_sched::Scheduler::set_max_wall`], which the whole-node drain order and deadlines
//!   read), written in the submission's transaction beside `op/` and `queue/` and deleted
//!   with them, only for an operation given one. Like `opkey/` it is in a row of its own, so
//!   no row of an older format changes, a binary that does not know the prefix reads the
//!   store as it always did, and an operation with no row has no wall time. It is read
//!   whatever the options: the scheduler ignores it unless the whole-node option is on;
//! - `subject/<operation>`: what the operation's `action usage:` line says it is (the
//!   shape key, configuration hash, target and the client's mnemonic and target hint,
//!   [`crate::action_usage::Subject`]), written in the submission's transaction beside
//!   `op/` and `queue/` and deleted with them, so a successor logs the same line the
//!   writer would have. A store with no row for an operation logs the empty shape;
//! - `done/<operation>`: how a finished or refused operation ended, kept for the finished
//!   retention (at least [`DONE_FLOOR`]) so a process that did not finish it can still
//!   answer the callers of it: a tag byte and the `ExecuteResponse` ([`kbf_front::Finished`]).
//!   It is put in the same transaction that deletes the operation's `op/`, `queue/`,
//!   `loss/`, `fate/` and `subject/` rows (and its `lease/` row, for a result), so a
//!   finished operation is always in the store as its unfinished rows or as its result,
//!   never as neither. The row is as large as the response, and the store sets no bound
//!   on a row, so a very large response makes a large transaction, as it made a large
//!   row before;
//! - `doneof/<operation>`: the instance name and action of a finished or refused operation,
//!   put in the same transaction as its `done/` row and kept as long, so that any frontend can
//!   answer a client that asks for the operation by its name (`operations/<id>`), authorized
//!   against the instance, after the `op/` row that held them is gone (the `named` module);
//! - `cordon/<worker>`: how an operator placed a node (cordoned, or draining until a
//!   deadline on the store's clock), kept until it is returned to placement, so a
//!   successor process serves the node as the operator left it, and the other frontends
//!   of the store apply it at their next intake poll;
//! - `cordon-owner/<worker>`: who owns that cordon (`operator`, or `updater:<unit>`), read
//!   for the node detail and written by nothing yet (`farm::cordon_owner`). A row of its
//!   own, outside the `cordon/` scan, so a frontend that does not know it never reads it;
//! - `est/<instance>/<platform>/<basis>/<shape>/<cfg>`: one size summary of the learned-sizes
//!   shadow ([`crate::learned`]): a version byte and the estimator's own checksummed row.
//!   Never in the transaction of a requeue or a finish: a summary an own-limit kill changed
//!   is written right after it, best effort, and the rest is written behind, in batches;
//!   a sweep deletes the stale. Read only when the shadow is on;
//! - `meta/epoch`, `meta/next_op`, `meta/next_seq`: the store incarnation, and the first
//!   operation id and lease sequence no frontend has reserved. A frontend reserves a block
//!   of [`ID_BLOCK`] ids of either with one compare-and-set write of `max(stored, its own
//!   next) + ID_BLOCK` (read again and tried again when another frontend wrote first) and
//!   numbers inside the block, so the stored value never goes back and no two frontends,
//!   and no restarted one, take an id twice. The `op/` and `lease/` rows of a finished
//!   operation are deleted, so their absence guards no id. The value is eight bytes,
//!   big-endian, as it was when each id was written as it was taken, so no new row or
//!   format is needed: a store from that time reads as one whose blocks are spent. The
//!   epoch row is created with an absent-only write, and a frontend that loses that race
//!   reads the other's;
//! - `meta/generation`: the writer generation of a store an older build that takes
//!   generations opened, or the marker [`kbf_state::SHARED_WRITERS`] that a start puts there
//!   when the frontends share the store and none takes a generation. A write of the
//!   scheduler is then checked by the rows it read, not by a generation. Operation names do
//!   not carry it: a farm over a store names its operations `operations/<id>`, and the ids
//!   are the store's.
//!
//! Several frontends may share one store. Each change of the rows above to an operation
//! (a grant, a lease given up, a result, a refusal) is one transaction that is conditional
//! on the versions of the rows it read, so a frontend that loses a race to another writes
//! nothing and the other's work stands: an operation another frontend finished is not
//! queued again, one another frontend runs is not refused, and a result that arrives twice
//! is taken once. The write that loses is dropped as stale, and the callers waiting on its
//! operation at that frontend are not answered by it. The `transitions` module says how
//! each one is conditional. A write of a lease of a daemon's node is conditional on the
//! node's `owner/` row too: the frontend the daemon last said `Hello` to is the only one
//! that may write the node's leases, and one the daemon has left fails with
//! [`StateError::StaleOwner`] for that node alone (the `owner` module). Whether an
//! operation is unservable is judged from the `session/` rows, which hold every
//! frontend's daemons, and not from the daemons of one frontend.
//!
//! A frontend learns what the others did by looking at the rows every [`INTAKE_POLL`] (the
//! `intake` module): it takes in the operations they queued, forgets the ones they took
//! (the callers it kept are answered from `done/`), applies their `cordon/` rows and raises
//! its floors to theirs. A start takes up only the operations that wait for a lease: a
//! leased one is taken up when its node says `Hello` to this frontend, and the `sweep`
//! module requeues the lease of a node no frontend hears, so the work of a frontend that
//! died is not stranded. A request that joins an operation leased on another frontend's
//! node takes up nothing: it follows the operation through the store until its `done/` row
//! is there ([`Held::Elsewhere`]); only a lease on a node this frontend owns is taken up by
//! a join.
//!
//! A client that asks for an operation by its name (`operations/<id>`) is answered by any
//! frontend: by the caller it holds for the operation if its scheduler holds it, and from the
//! `op/`, `queue/`, `done/` and `doneof/` rows if not (the `named` module).
//!
//! Values are the `kbf-log` encoding of the scheduler's own inputs (a `Submit`, the
//! `Committed` lease, a `WorkerUp`), so the `kbf-log` codec the replicated log already runs
//! decides what a row may hold.
//!
//! A `lease/` or `op/` row that holds a command this binary does not know was written by
//! a later binary: a scan skips it, and the rows it can read load. A start ([`Durable::load`])
//! logs the key of each row it skips; the scans a request makes skip them silently, so one
//! leftover row does not fill the log at request rate. A row that is malformed, or that
//! holds a known command of the wrong kind, still refuses the scan, and so does a single
//! row a write depends on, whatever it holds.
//!
//! A skipped `lease/` row leaves its operation with no lease: it loads queued, the
//! scheduler can grant it again, and the action may run twice. Nothing names the skipped
//! row's lease again, so the row is never deleted.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
#[cfg(test)]
use std::time::UNIX_EPOCH;
use std::time::{Duration, SystemTime};

use kbf_estimator::Day;
use kbf_front::{Finished, Stage};
#[cfg(test)]
use kbf_log::{FarmBatch, FarmInput, Format, LogCommand};
use kbf_sched::fence::LEASE_GRACE;
use kbf_sched::{
    DaemonInstance, Fate, Floor, FloorChange, KeyFloor, Recovered, RecoveredLease, Request,
    Requeue, RequeueReason,
};
use kbf_state::{
    Cond, Conflict, GENERATION, SHARED_WRITERS, Store, StoreError, Txn, Version, Versioned,
};
use kbf_types::{
    ActionKey, ControlRecord, Digest, Effect, FarmTime, LeaseGrant, LeaseId, OperationId, WaiterId,
    WorkerId,
};

use tonic::Status;

use super::engine::{Adapter, Engine};
use super::{Farm, Reported, State};
// What the tests of this module and its children name through `use super::*`.
#[cfg(test)]
use super::{finished_of, refused_as};
use crate::action_usage::Subject;
use crate::learned::Row;
use crate::machine::FarmMachine;
use crate::state_store::StateStore;
#[cfg(test)]
use kbf_front::Described;

use self::codec::{KbfLog, RowCodec, WorkerUp};
#[cfg(test)]
use self::codec::{decode, encode};
use self::ext::{Ext, KbfRows};
use self::inflight::Submitted;
use self::leases::lease_by_op_key;
use self::owner::{Owner, Ownership};

pub use self::blocks::ID_BLOCK;
pub use self::intake::INTAKE_POLL;
pub use self::owner::{ACK_WINDOW, OWNER_CACHE_TTL};
pub use self::sweep::ORPHAN_WAIT;

/// How often a daemon session's row in the store is renewed while the daemon heartbeats:
/// a third of the lease grace, so the row outlives two missed renewals. One write per
/// session per interval, whatever the number of leases it holds.
pub const SESSION_RENEWAL: Duration = Duration::from_secs(LEASE_GRACE.as_secs() / 3);

const EPOCH: &str = "meta/epoch";
const OPERATION: &str = "op/";
const QUEUE: &str = "queue/";
const LOSS: &str = "loss/";
const DONE: &str = "done/";
const LEASE: &str = "lease/";
const ALIVE: &str = "meta/alive";
const NEXT_OP: &str = "meta/next_op";
const NEXT_SEQ: &str = "meta/next_seq";
const CORDON: &str = "cordon/";
const SESSION: &str = "session/";
const FATE: &str = "fate/";
const FLOOR: &str = "floor/";
const KEY_FLOOR: &str = "keyfloor/";
const OPKEY: &str = "opkey/";
const OPWALL: &str = "opwall/";
pub(crate) const ESTIMATE: &str = "est/";
/// Rows of one transaction of the size sweep, and the most transactions of one call.
pub(super) const SWEEP_ROWS: usize = 256;
pub(super) const SWEEP_TXNS: usize = 4;
pub(crate) const SUBJECT: &str = "subject/";

/// The least time a `done/` row is kept, whatever the finished retention: a process that
/// follows an operation it did not finish needs the row for as long as a stream it kept
/// open can take to notice it.
pub const DONE_FLOOR: Duration = Duration::from_secs(600);

/// What a start does with the store's epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochPolicy {
    /// Keep it: the scheduler resumes the store's incarnation, and a daemon that
    /// reconnects keeps the runs of its leases.
    Keep,
    /// Take the next one: the start is a new incarnation, and a daemon that sees it
    /// kills the runs of the leases it held before.
    Bump,
}

/// An operator's raise of the stored epoch ([`Farm::bump_epoch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpochBump {
    /// The epoch the store held before.
    pub previous: u64,
    /// The epoch it holds now.
    pub epoch: u64,
}

/// Why the state store could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The backend failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A row changed under the scheduler.
    #[error("a row changed under the scheduler: {0}")]
    Conflict(#[from] Conflict),
    /// The row a write depends on is not there: the write is stale (a result of a lease
    /// the operation no longer holds, a grant of an operation that is not queued).
    #[error("the state store has no {0}: the write is stale")]
    Missing(String),
    /// The write is for a daemon's node that another frontend owns now (the daemon said
    /// `Hello` there since this frontend did): the write is stale, and only this node's
    /// writes are.
    #[error("the node {0} belongs to another frontend: the write is stale")]
    StaleOwner(WorkerId),
    /// A row cannot be read.
    #[error("the state store row {key} cannot be read: {why}")]
    Corrupt {
        /// The row.
        key: String,
        /// What is wrong with it.
        why: String,
    },
    /// A value cannot be encoded.
    #[error("a state store value cannot be encoded: {0}")]
    Encode(#[from] kbf_log::EncodeError),
    /// This build has no backend for the store's scheme.
    #[error("this build has no {0} state store backend")]
    NoBackend(&'static str),
    /// The store could not be opened: its file is held by another process or is not a
    /// state store, or its directory cannot be made.
    #[error("the state store cannot be opened: {0}")]
    Open(String),
}

/// The errors of the core's rows ([`js_store`]) are the farm's own, variant for variant. The
/// core has no encode error of the log's kind: its codec's text is a row that cannot be
/// written, which is [`StateError::Corrupt`] here.
impl From<js_store::StateError> for StateError {
    fn from(error: js_store::StateError) -> Self {
        match error {
            js_store::StateError::Store(cause) => Self::Store(cause),
            js_store::StateError::Conflict(conflict) => Self::Conflict(conflict),
            js_store::StateError::Missing(row) => Self::Missing(row),
            js_store::StateError::StaleOwner(worker) => Self::StaleOwner(worker),
            js_store::StateError::Corrupt { key, why } => Self::Corrupt { key, why },
            js_store::StateError::Codec(why) => Self::Corrupt {
                key: "(encode)".to_owned(),
                why,
            },
        }
    }
}

/// What a store holds of the scheduler, as a start reads it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stored {
    /// The first operation id no frontend has reserved.
    pub next_operation: u64,
    /// The first lease sequence no frontend has reserved.
    pub next_seq: u64,
    /// Every unfinished operation, in id order.
    pub operations: Vec<StoredOperation>,
    /// Every memory floor, oldest stamp first (two floors with one stamp in key order, the
    /// later with the next stamp).
    pub floors: Vec<Floor>,
    /// Every key floor, in the same order.
    pub key_floors: Vec<KeyFloor>,
    /// The size summary each key floor decays with, for those whose row names one.
    pub key_names: Vec<(u64, kbf_estimator::Key)>,
    /// Every node an operator placed, in node-id order.
    pub cordons: Vec<(WorkerId, StoredCordon)>,
}

/// How an operator placed a node, as its `cordon/` row keeps it: a tag byte, and for a
/// drain the deadline as eight bytes, big-endian, milliseconds on the store's clock (a
/// frontend has a clock of its own for farm time, and the store's is the one all share; for
/// a store whose clock starts at the Unix epoch, the wall clock's own reading).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StoredCordon {
    /// Placement skips the node.
    Cordoned,
    /// Placement skips the node and waits for its leases until the deadline.
    Draining { deadline_unix_ms: u64 },
}

impl StoredCordon {
    fn to_row(self) -> Vec<u8> {
        match self {
            Self::Cordoned => vec![0],
            Self::Draining { deadline_unix_ms } => {
                [&[1][..], &deadline_unix_ms.to_be_bytes()].concat()
            }
        }
    }

    fn from_row(key: &str, row: &[u8]) -> Result<Self, StateError> {
        match row {
            [0] => Ok(Self::Cordoned),
            [1, deadline @ ..] => {
                let deadline = <[u8; 8]>::try_from(deadline)
                    .map_err(|_| corrupt(key, "a drain deadline is not 8 bytes"))?;
                Ok(Self::Draining {
                    deadline_unix_ms: u64::from_be_bytes(deadline),
                })
            }
            _ => Err(corrupt(key, "not a cordon")),
        }
    }
}

/// Where an operation stands in the state store, as a process that did not run it sees it.
#[derive(Debug, PartialEq)]
pub(super) enum Progress {
    /// It ended, and this is how.
    Ended(Finished),
    /// It waits for a lease.
    Queued,
    /// It holds a lease.
    Running,
    /// The store has no trace of it: not unfinished and not ended. Its writer ends an
    /// operation in one transaction (the operation's rows removed, its `done/` row
    /// put), so a finish never looks like this; a result row past its retention does.
    Gone,
}

impl Progress {
    pub(super) const fn is_gone(&self) -> bool {
        matches!(self, Self::Gone)
    }
}

/// An unfinished operation of a store.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct StoredOperation {
    pub operation: OperationId,
    pub request: Request,
    /// The action `request` runs: its key as the server names it.
    pub action: ActionKey,
    /// The committed lease it holds.
    pub lease: Option<LeaseGrant>,
    /// How many of its leases were lost.
    pub losses: u32,
    /// What its kills for memory changed, if it was killed.
    pub fate: Option<Fate>,
    /// What its `action usage:` line says it is, if the writer recorded it.
    pub subject: Option<Subject>,
    /// Its size key, if the writer named one (`opkey/` row).
    pub size_key: Option<u64>,
    /// The longest it may run, if the writer named one (`opwall/` row).
    pub max_wall: Option<Duration>,
}

impl StoredOperation {
    /// This operation as the scheduler takes it up, held for `waiter`.
    pub(super) fn recovered(self, waiter: WaiterId) -> Recovered {
        Recovered {
            operation: self.operation,
            request: self.request,
            waiter,
            lease: self.lease.map(|grant| RecoveredLease {
                lease: grant.lease,
                worker: grant.worker,
                // The rows of this store keep no cost: the scheduler charges the lease its
                // request.
                cost: None,
            }),
            losses: self.losses,
            fate: self.fate,
        }
    }
}

/// Where a request that was queued or that joined an operation is held ([`Engine::queue`]).
pub(super) enum Held {
    /// By this frontend's scheduler: the request is a caller of an operation it holds.
    Here,
    /// By another frontend: the request joined this operation, which holds a lease on a
    /// node that frontend owns. This one takes up nothing of it, not even the lease (that
    /// would end in this scheduler giving up a lease it does not own, and deleting the
    /// owner's `lease/` row): the request follows the operation through the store until its
    /// result is there.
    Elsewhere(OperationId),
}

mod blocks;
mod codec;
// `estimate`, `named` and `subject` hold the formats of the build-only rows, which the
// adapter over `js-store` (`crate::farm_js`) writes too.
pub(crate) mod estimate;
mod ext;
mod fate;
mod floor;
mod inflight;
pub(super) mod intake;
mod leases;
pub(crate) mod named;
mod opkey;
mod opwall;
mod owner;
mod reads;
pub(crate) mod subject;
mod sweep;
mod transitions;

/// The operations the `effects` name, comma separated, for a log line.
/// Each operation is named once, in the order the effects first name it.
fn operations_of(effects: &VecDeque<Effect>) -> String {
    let mut operations: Vec<OperationId> = Vec::new();
    for effect in effects {
        let operation = match effect {
            Effect::Start(start) => start.operation,
            Effect::Answer(answer) => answer.operation,
            Effect::Refuse(refusal) => refusal.operation,
            Effect::Waiting(waiting) => waiting.operation,
            Effect::Commit(_) => continue,
        };
        if !operations.contains(&operation) {
            operations.push(operation);
        }
    }
    operations
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The lease an effect starts, if it is a `Start`.
fn started(effect: &Effect) -> Option<LeaseId> {
    match effect {
        Effect::Start(start) => Some(start.lease),
        _ => None,
    }
}

/// How a write to the state store ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attempt {
    /// The store took it.
    Taken,
    /// The store could not be reached: the same write may succeed later.
    Transient,
    /// A row the write depends on is gone or changed: another writer got there first, and
    /// the write is stale. A write the store applied but did not acknowledge is not this:
    /// each write is written to find its own work done and take it as taken
    /// ([`Durable::granted`], [`Durable::finished`], [`Durable::refused`] and
    /// [`Durable::requeued`]), so a retry of it is `Taken`.
    Stale,
    /// The write is for a daemon's node that another frontend owns now: it is dropped, as
    /// a stale write is, and trying again cannot help. The node's other writes, held or to
    /// come, meet the same refusal; the store as a whole is not superseded.
    Disowned,
    /// The write cannot be encoded, or a newer writer holds the store: trying again
    /// cannot help.
    Dropped,
}

/// How an operation ended, as the `done/` row written in the transaction that removes
/// the operation's rows.
#[derive(Debug)]
pub(super) struct Ending {
    pub(super) finished: Finished,
    /// How long the row is kept, at least [`DONE_FLOOR`].
    pub(super) retention: Duration,
}

impl Ending {
    /// How long the result is kept: the finished retention, and at least [`DONE_FLOOR`], so
    /// that a client which reconnects to another frontend after the finish still finds it.
    pub(super) fn ttl(&self) -> Duration {
        self.retention.max(DONE_FLOOR)
    }

    /// [`Ending::settle_with`] with the rows of the build server.
    #[cfg(test)]
    fn settle(&self, txn: Txn, operation: OperationId) -> Txn {
        self.settle_with(&KbfRows, txn, operation, false)
    }

    /// `txn`, then the removal of the operation's `op/`, `queue/`, `loss/`, `fate/` and
    /// `opkey/` rows (and the rows `ext` removes, between the `fate/` and `opkey/` deletes)
    /// and the put of its result, so one transaction carries all of it. The `opkey/` delete
    /// is made whatever `--learned-sizes` says: an operation
    /// an `on` run queued has the row, and a later `off` run cleans it up as it ends. The
    /// `opwall/` row is deleted only if `walled` says the operation has one, so an operation
    /// without a wall time ends in the transaction it always did.
    /// The result goes over whatever is there: the transaction also deletes the lease row
    /// at the version read, which no other writer can have deleted first and written a
    /// result after.
    fn settle_with(&self, ext: &dyn Ext, txn: Txn, operation: OperationId, walled: bool) -> Txn {
        let txn = txn
            .delete(op_key(operation), Cond::Any)
            .delete(queue_key(operation), Cond::Any);
        self.settle_rest(ext, txn, operation, walled, Cond::Any)
    }

    /// `txn`, which already deletes the operation's `op/` and `queue/` rows at the versions
    /// it read, then the removal of the rest of its rows (its `lease-by-op/` row among
    /// them, whoever left it) and the put of its result where `done` allows:
    /// [`Cond::Absent`] for a result no other writer has put.
    fn settle_rest(
        &self,
        ext: &dyn Ext,
        txn: Txn,
        operation: OperationId,
        walled: bool,
        done: Cond,
    ) -> Txn {
        let ttl = self.ttl();
        let txn = txn
            .delete(loss_key(operation), Cond::Any)
            .delete(fate_key(operation), Cond::Any);
        let txn = ext
            .settled(txn, operation)
            .delete(opkey_key(operation), Cond::Any);
        let txn = if walled {
            txn.delete(opwall_key(operation), Cond::Any)
        } else {
            txn
        };
        txn.delete(lease_by_op_key(operation), Cond::Any).put_ttl(
            done_key(operation),
            self.finished.to_row(),
            ttl,
            done,
        )
    }
}

/// The scheduler's handle on its state store.
pub(super) struct Durable {
    store: Arc<dyn Store>,
    /// What the values of the `op/`, `lease/` and `session/` rows are as bytes.
    codec: Arc<dyn RowCodec>,
    /// The rows only a build server has, beside the scheduler's.
    ext: Arc<dyn Ext>,
    /// A write failed because the store could not be reached, and none has been taken
    /// since ([`Engine::watch_store`] tells the scheduler).
    pub(super) unreachable: AtomicBool,
    /// A write failed because another writer holds a newer generation of the store: this
    /// scheduler was superseded and takes no more work. Never cleared; unlike an outage,
    /// the store coming back does not end it.
    pub(super) fenced: AtomicBool,
    /// The operation ids this handle reserved last ([`Durable::reserve_operation`]):
    /// numbered inside it, and no other frontend takes one of them.
    ops: Mutex<Range<u64>>,
    /// This frontend's id in the `owner/` rows it writes: the term it started under, which
    /// differs from every other frontend's but for a chance of about one in 2^16 within
    /// a millisecond. Ownership checks compare the counter too, so a shared id costs only
    /// the detection of a move between the two frontends.
    frontend: u64,
    /// The nodes this frontend claimed, and what it holds of each ([`Ownership`]).
    owners: Mutex<BTreeMap<WorkerId, Ownership>>,
    /// The owner each node's newest claim found, for the claims whose arrival did not
    /// complete (what the claim took over could not be read): the retry finds this
    /// frontend's own row, and takes the owner named here as the one it replaced.
    handovers: Mutex<BTreeMap<WorkerId, Option<Owner>>>,
    /// The value of the newest session row this handle wrote for each node, so a renewal
    /// that finds the row gone (it expired, or its write was refused) puts it again.
    sessions: Mutex<BTreeMap<WorkerId, Vec<u8>>>,
    /// Whether the `opkey/` and `keyfloor/` rows are read (`--learned-sizes=on`). A store
    /// that never had the flag on holds none, and with it off a row an earlier `on` run
    /// left is neither loaded nor allowed to fail a start. The finish transaction deletes
    /// an operation's `opkey/` row whatever the flag says, so rows an `on` run left are
    /// cleaned up as its operations end.
    pub(super) read_size_keys: AtomicBool,
    /// Test builds only: refusals the store gives in place of a write, each to the next
    /// attempt that names its `what` ([`Durable::refuse_next`]).
    #[cfg(test)]
    refusals: Mutex<Vec<(&'static str, StateError)>>,
}

impl std::fmt::Debug for Durable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Durable")
    }
}

fn op_key(operation: OperationId) -> String {
    format!("{OPERATION}{:016x}", operation.0)
}

fn queue_key(operation: OperationId) -> String {
    format!("{QUEUE}{:016x}", operation.0)
}

fn loss_key(operation: OperationId) -> String {
    format!("{LOSS}{:016x}", operation.0)
}

fn fate_key(operation: OperationId) -> String {
    format!("{FATE}{:016x}", operation.0)
}

fn opkey_key(operation: OperationId) -> String {
    format!("{OPKEY}{:016x}", operation.0)
}

fn opwall_key(operation: OperationId) -> String {
    format!("{OPWALL}{:016x}", operation.0)
}

fn subject_key(operation: OperationId) -> String {
    format!("{SUBJECT}{:016x}", operation.0)
}

fn done_key(operation: OperationId) -> String {
    format!("{DONE}{:016x}", operation.0)
}

fn cordon_key(worker: &WorkerId) -> String {
    format!("{CORDON}{worker}")
}

fn lease_key(lease: LeaseId) -> String {
    format!("{LEASE}{:016x}.{:016x}", lease.term, lease.seq)
}

fn corrupt(key: &str, why: &str) -> StateError {
    StateError::Corrupt {
        key: key.to_owned(),
        why: why.to_owned(),
    }
}

fn parse_operation(key: &str, prefix: &str) -> Result<OperationId, StateError> {
    let id = u64::from_str_radix(&key[prefix.len()..], 16)
        .map_err(|_| corrupt(key, "not an operation id"))?;
    Ok(OperationId(id))
}

/// The action `request`, the one in the `op/` row `row`, runs: the key every request of
/// the server's front is made with.
fn action_of_request(row: &str, request: &Request) -> Result<ActionKey, StateError> {
    ActionKey::try_from(&request.key).map_err(|why| corrupt(row, &why.to_string()))
}

fn number(key: &str, row: &Versioned) -> Result<u64, StateError> {
    let bytes = <[u8; 8]>::try_from(row.value.as_slice())
        .map_err(|_| corrupt(key, "not an 8-byte number"))?;
    Ok(u64::from_be_bytes(bytes))
}

impl Durable {
    /// Opens the scheduler's rows in `store` and settles its epoch: a store that has
    /// none gets a first one that no other store's epoch is likely to equal (a daemon
    /// must not mistake a new store for the one it knew), `policy` decides for one that
    /// has. Returns the handle and the epoch. `first` is the process term, which also names
    /// the handle's frontend in the `owner/` rows it writes: two frontends of a store start
    /// under one only by a chance of about one in 2^16 within a millisecond.
    pub(super) fn open(
        store: Arc<dyn Store>,
        policy: EpochPolicy,
        first: u64,
    ) -> Result<(Self, u64), StateError> {
        Self::open_with(store, policy, first, Arc::new(KbfLog), Arc::new(KbfRows))
    }

    /// [`Durable::open`], with the encoding of the row values and the rows of a build
    /// server given: the keys and the layout of the rows are this handle's, the bytes of
    /// the values `codec`'s, and `ext` adds its own rows to the transactions.
    fn open_with(
        store: Arc<dyn Store>,
        policy: EpochPolicy,
        first: u64,
        codec: Arc<dyn RowCodec>,
        ext: Arc<dyn Ext>,
    ) -> Result<(Self, u64), StateError> {
        let durable = Self {
            store,
            codec,
            ext,
            unreachable: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
            ops: Mutex::new(0..0),
            frontend: first,
            owners: Mutex::new(BTreeMap::new()),
            handovers: Mutex::new(BTreeMap::new()),
            sessions: Mutex::new(BTreeMap::new()),
            read_size_keys: AtomicBool::new(true),
            #[cfg(test)]
            refusals: Mutex::new(Vec::new()),
        };
        let epoch = durable.settle_epoch(policy, first)?;
        Ok((durable, epoch))
    }

    /// The rows under `prefix` that a scan can use: a row that holds a command this
    /// binary does not know is left out, and every other row stays, for the scan to read
    /// or refuse. It logs nothing: a scan a request makes would repeat it per request.
    fn scan_known(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StateError> {
        let mut rows = self.store.scan_prefix(prefix)?;
        rows.retain(|(key, row)| !self.is_unknown(key, &row.value));
        Ok(rows)
    }

    /// [`Durable::scan_known`], logging the key of each row it leaves out: for a start.
    fn scan_known_logged(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StateError> {
        let mut rows = self.store.scan_prefix(prefix)?;
        rows.retain(|(key, row)| {
            let unknown = self.is_unknown(key, &row.value);
            if unknown {
                tracing::warn!(%key, "a row holds a command this binary does not know: skipped");
            }
            !unknown
        });
        Ok(rows)
    }

    /// The row at `key`, which the write that asks depends on.
    fn row(&self, key: &str) -> Result<Versioned, StateError> {
        let row = self.store.get(key)?;
        row.ok_or_else(|| StateError::Missing(key.to_owned()))
    }

    fn commit(&self, txn: Txn) -> Result<(), StateError> {
        self.store.txn(txn)??;
        Ok(())
    }

    /// What the store holds: the unfinished operations, each with its lease, and the
    /// first ids to hand out.
    pub(super) fn load(&self) -> Result<Stored, StateError> {
        let mut leases = std::collections::BTreeMap::new();
        for (key, row) in self.scan_known_logged(LEASE)? {
            let grant = self.lease_row(&key, &row)?;
            leases.insert(grant.operation, grant);
        }
        let mut losses = std::collections::BTreeMap::new();
        for (key, row) in self.store.scan_prefix(LOSS)? {
            let operation = parse_operation(&key, LOSS)?;
            let count = u32::try_from(number(&key, &row)?)
                .map_err(|_| corrupt(&key, "not a loss count"))?;
            losses.insert(operation, count);
        }
        let mut fates = std::collections::BTreeMap::new();
        for (key, row) in self.store.scan_prefix(FATE)? {
            let operation = parse_operation(&key, FATE)?;
            let fate = fate::decode(&row.value).map_err(|why| corrupt(&key, why))?;
            fates.insert(operation, fate);
        }
        let mut subjects = self.ext.subjects(&*self.store)?;
        let read_size_keys = self.read_size_keys.load(Ordering::SeqCst);
        let mut opkeys = std::collections::BTreeMap::new();
        for (key, row) in self
            .store
            .scan_prefix(OPKEY)?
            .into_iter()
            .filter(|_| read_size_keys)
        {
            let operation = parse_operation(&key, OPKEY)?;
            let size_key = opkey::decode(&row.value).map_err(|why| corrupt(&key, why))?;
            opkeys.insert(operation, size_key);
        }
        let mut walls = std::collections::BTreeMap::new();
        for (key, row) in self.store.scan_prefix(OPWALL)? {
            let operation = parse_operation(&key, OPWALL)?;
            let wall = opwall::decode(&row.value).map_err(|why| corrupt(&key, why))?;
            walls.insert(operation, wall);
        }
        let mut floors = Vec::new();
        for (key, row) in self.store.scan_prefix(FLOOR)? {
            let floor = floor::decode(&row.value).map_err(|why| corrupt(&key, why))?;
            if key != floor::row_key(&floor.key) {
                return Err(corrupt(&key, "not the key of the floor it holds"));
            }
            floors.push(floor);
        }
        // Frontends sharing a store stamp their raises from counters of their own, so two
        // floors may carry one stamp: they are ordered by key, the later one a stamp on.
        floors.sort_by_key(|floor| floor.stamp);
        let mut next = 0;
        for floor in &mut floors {
            floor.stamp = floor.stamp.max(next);
            next = floor.stamp.saturating_add(1);
        }
        let mut key_floors = Vec::new();
        let mut key_names = Vec::new();
        for (key, row) in self
            .store
            .scan_prefix(KEY_FLOOR)?
            .into_iter()
            .filter(|_| read_size_keys)
        {
            let (floor, name) = floor::decode_key(&row.value).map_err(|why| corrupt(&key, why))?;
            match name.as_deref().map(|name| self.ext.summary_key(name)) {
                Some(Some(summary)) => key_names.push((floor.key, summary)),
                Some(None) => {
                    tracing::warn!(row = %key, "a key floor names a size summary that does not parse");
                }
                None => {}
            }
            if key != floor::key_row(floor.key) {
                return Err(corrupt(&key, "not the key of the key floor it holds"));
            }
            key_floors.push(floor);
        }
        key_floors.sort_by_key(|floor| floor.stamp);
        let mut next = 0;
        for floor in &mut key_floors {
            floor.stamp = floor.stamp.max(next);
            next = floor.stamp.saturating_add(1);
        }
        let mut operations = Vec::new();
        for (key, row) in self.scan_known_logged(OPERATION)? {
            let request = self.request_row(&key, &row)?;
            let action = action_of_request(&key, &request)?;
            let operation = parse_operation(&key, OPERATION)?;
            let lease = leases.remove(&operation);
            operations.push(StoredOperation {
                operation,
                request,
                action,
                lease,
                losses: losses.remove(&operation).unwrap_or(0),
                fate: fates.remove(&operation),
                subject: subjects.remove(&operation),
                size_key: opkeys.remove(&operation),
                max_wall: walls.remove(&operation),
            });
        }
        Ok(Stored {
            next_operation: self.counter(NEXT_OP)?,
            next_seq: self.counter(NEXT_SEQ)?,
            operations,
            floors,
            key_floors,
            key_names,
            cordons: self.cordons()?,
        })
    }

    /// The size summaries the store holds. A row that cannot be read is skipped (and
    /// counted in the log): its action starts cold.
    pub(super) fn estimates(&self) -> Result<Vec<Row>, StateError> {
        self.ext.estimates(&*self.store)
    }

    /// Writes `rows`, size summaries, in one transaction.
    pub(super) fn put_estimates(&self, rows: &[Row]) -> Result<(), StateError> {
        self.commit(self.ext.put_estimates(Txn::new(), rows))
    }

    /// Deletes the size summaries idle past [`kbf_estimator::STALE_DAYS`] on `today`,
    /// and strips the samples from those that hold a floor, as the estimator itself
    /// does in memory. At most [`SWEEP_TXNS`] transactions of [`SWEEP_ROWS`] rows are
    /// committed per call. Returns how many rows it changed and whether more are left
    /// for another call. A transaction the store refuses is logged and the rest go on. The scan
    /// reads every row (at most the estimator's key bound) whatever the cap.
    pub(super) fn sweep_estimates(&self, today: Day) -> Result<(usize, bool), StateError> {
        let changes = self.ext.stale_estimates(&*self.store, today)?;
        let left = changes.len() > SWEEP_TXNS * SWEEP_ROWS;
        let mut changed = 0;
        for chunk in changes.chunks(SWEEP_ROWS).take(SWEEP_TXNS) {
            let mut txn = Txn::new();
            for (name, value, version) in chunk {
                txn = match value {
                    None => txn.delete(name.clone(), Cond::Version(*version)),
                    Some(value) => txn.put(name.clone(), value.clone(), Cond::Version(*version)),
                };
            }
            match self.commit(txn) {
                Ok(()) => changed += chunk.len(),
                Err(error) => {
                    tracing::warn!(%error, "a batch of size summaries was not swept");
                    if self.superseded_by(&error) {
                        break;
                    }
                }
            }
        }
        // A sweep that committed nothing does not ask for another tick: a store that
        // refuses every batch is not scanned again until the next hour.
        Ok((changed, left && changed > 0))
    }

    /// The nodes an operator placed.
    fn cordons(&self) -> Result<Vec<(WorkerId, StoredCordon)>, StateError> {
        let rows = self.store.scan_prefix(CORDON)?;
        rows.into_iter()
            .map(|(key, row)| {
                let cordon = StoredCordon::from_row(&key, &row.value)?;
                Ok((WorkerId::new(&key[CORDON.len()..]), cordon))
            })
            .collect()
    }

    /// An operator placed `worker` as `cordon`, or returned it to placement (`None`).
    pub(super) fn cordoned(
        &self,
        worker: &WorkerId,
        cordon: Option<StoredCordon>,
    ) -> Result<(), StateError> {
        let key = cordon_key(worker);
        let txn = match cordon {
            Some(cordon) => Txn::new().put(key, cordon.to_row(), Cond::Any),
            None => Txn::new().delete(key, Cond::Any),
        };
        self.commit(txn)
    }

    /// Where `operation` stands. The reads are separate, so a finish may land between
    /// them: `done/` is read again when the operation's rows are gone, and a finish (one
    /// transaction) leaves its result by then.
    pub(super) fn progress(&self, operation: OperationId) -> Result<Progress, StateError> {
        if let Some(finished) = self.ended(operation)? {
            return Ok(Progress::Ended(finished));
        }
        let unfinished = self.store.get(&op_key(operation))?.is_some();
        let queued = self.store.get(&queue_key(operation))?.is_some();
        Ok(match (unfinished, queued) {
            (false, _) => self
                .ended(operation)?
                .map_or(Progress::Gone, Progress::Ended),
            (true, true) => Progress::Queued,
            (true, false) => Progress::Running,
        })
    }

    fn counter(&self, key: &str) -> Result<u64, StateError> {
        self.store.get(key)?.map_or(Ok(0), |row| number(key, &row))
    }

    /// An operation submitted: its request, what its `action usage:` line says it is,
    /// and its place in the queue, in one transaction.
    #[cfg(test)]
    pub(super) fn submitted(
        &self,
        operation: OperationId,
        request: &Request,
        subject: &Subject,
    ) -> Result<(), StateError> {
        self.submitted_keyed(operation, request, subject, None, None)
    }

    /// [`Durable::submitted`], with the operation's size key and longest wall time, if it
    /// has them, in an `opkey/` and an `opwall/` row of the same transaction.
    fn submitted_keyed(
        &self,
        operation: OperationId,
        request: &Request,
        subject: &Subject,
        size_key: Option<u64>,
        max_wall: Option<Duration>,
    ) -> Result<(), StateError> {
        self.commit(self.submission(operation, request, subject, size_key, max_wall)?)
    }

    /// [`Durable::submitted`], with the action's in-flight row in the same transaction:
    /// `pin` is its key and what the row must be (absent, or the leftover version).
    fn submitted_pinned(
        &self,
        operation: OperationId,
        request: &Request,
        subject: &Subject,
        size_key: Option<u64>,
        max_wall: Option<Duration>,
        (pin, cond): (&str, Cond),
    ) -> Result<(), StateError> {
        let txn = self.submission(operation, request, subject, size_key, max_wall)?;
        self.commit(txn.put(pin, operation.0.to_be_bytes(), cond))
    }

    fn submission(
        &self,
        operation: OperationId,
        request: &Request,
        subject: &Subject,
        size_key: Option<u64>,
        max_wall: Option<Duration>,
    ) -> Result<Txn, StateError> {
        let value = self.codec.submit(request)?;
        let mut txn = Txn::new();
        if let Some(key) = size_key {
            txn = txn.put(opkey_key(operation), opkey::encode(key), Cond::Absent);
        }
        if let Some(wall) = max_wall {
            txn = txn.put(opwall_key(operation), opwall::encode(wall), Cond::Absent);
        }
        let txn = txn.put(op_key(operation), value, Cond::Absent);
        let txn = self.ext.submitted(txn, operation, subject);
        Ok(txn.put(queue_key(operation), Vec::new(), Cond::Absent))
    }

    /// The value of the `keyfloor/` row of `floor`: it names the size summary `summary`
    /// when it is known and fits, else the name the row already holds (a write that does
    /// not know the summary leaves the name where it was).
    fn key_floor_value(
        &self,
        floor: &KeyFloor,
        summary: Option<&kbf_estimator::Key>,
    ) -> Result<Vec<u8>, StateError> {
        let named = summary.map(|summary| self.ext.summary_name(summary));
        let name = match named.filter(|n| n.len() <= usize::from(u16::MAX)) {
            Some(name) => Some(name),
            None => self
                .store
                .get(&floor::key_row(floor.key))?
                .and_then(|row| floor::decode_key(&row.value).ok())
                .and_then(|(_, name)| name),
        };
        Ok(floor::encode_key(floor, name.as_deref()))
    }

    /// Writes the key floor `floor` as it now stands, or deletes its row when it was
    /// forgotten (an `ask` of 0): outside any requeue, after the estimator's decay lowered
    /// it. Best effort for the caller; the row keeps its stamp and names the size summary
    /// `summary` that its floor decays with.
    pub(super) fn put_key_floor(
        &self,
        floor: &KeyFloor,
        summary: Option<&kbf_estimator::Key>,
    ) -> Result<(), StateError> {
        let row = floor::key_row(floor.key);
        let txn = if floor.ask == 0 {
            Txn::new().delete(row, Cond::Any)
        } else {
            Txn::new().put(row, self.key_floor_value(floor, summary)?, Cond::Any)
        };
        self.commit(txn)
    }

    /// Writes what the scheduler's effects of a committed record settle, in order.
    /// Nothing the effects carry (a `Start`, an answer) is carried out unless this
    /// returns `Ok`. `ending` is how the operation an `Answer` or a `Refuse` finishes
    /// ended: its row is written with the operation's rows removed, in one transaction
    /// (an effect that ends an operation and has none is `Missing`, and writes nothing).
    /// The farm passes one effect at a time (the held queue's front); `ending` is the
    /// result of the one effect among `effects` that ends an operation, so a slice with
    /// two endings is not a use of it.
    pub(super) fn persist(
        &self,
        effects: &[Effect],
        ending: Option<&Ending>,
    ) -> Result<(), StateError> {
        let ending = || {
            ending.ok_or_else(|| StateError::Missing("result of the ending operation".to_owned()))
        };
        for effect in effects {
            match effect {
                Effect::Start(start) => self.granted(start)?,
                Effect::Answer(answer) => {
                    self.finished(answer.operation, answer.lease, ending()?)?;
                }
                Effect::Refuse(refusal) => self.refused(refusal.operation, ending()?)?,
                Effect::Commit(_) | Effect::Waiting(_) => {}
            }
        }
        Ok(())
    }

    /// A daemon registered: its session row, as of the registration.
    pub(super) fn session(
        &self,
        worker: &WorkerId,
        instance: &DaemonInstance,
        predecessor_stopped: bool,
        reported: &Reported,
    ) -> Result<(), StateError> {
        let up = WorkerUp {
            worker: worker.clone(),
            instance: instance.clone(),
            predecessor_stopped,
            capacity: reported.capacity,
            caps: reported.caps.clone(),
        };
        let value = self.codec.session(&up)?;
        let key = format!("{SESSION}{worker}");
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.insert(worker.clone(), value.clone());
        drop(sessions);
        self.commit(Txn::new().put_ttl(key, value, LEASE_GRACE, Cond::Any))
    }

    /// Extends `worker`'s session row by the lease grace, from the store's clock. A
    /// daemon with no row (its registration was not written, or the row expired) gets the
    /// registration this handle wrote last put again, where there is still no row; a
    /// daemon this handle never wrote a registration for is left for its next
    /// registration to write.
    pub(super) fn renew(&self, worker: &WorkerId) -> Result<(), StateError> {
        let key = format!("{SESSION}{worker}");
        let Some(row) = self.store.get(&key)? else {
            let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(value) = sessions.get(worker).cloned() else {
                return Ok(());
            };
            drop(sessions);
            return self.commit(Txn::new().put_ttl(key, value, LEASE_GRACE, Cond::Absent));
        };
        let renewed = Txn::new().put_ttl(key, row.value, LEASE_GRACE, Cond::Version(row.version));
        self.commit(renewed)
    }

    /// The writer generation the store holds, if it holds one: a store with no generation
    /// row, and one that holds the marker of shared frontends (a Postgres store opened by
    /// [`crate::StateStoreUri::open`]), hold none.
    fn generation(&self) -> Result<Option<u64>, StateError> {
        let Some(row) = self.store.get(GENERATION)? else {
            return Ok(None);
        };
        if row.value == SHARED_WRITERS {
            return Ok(None);
        }
        number(GENERATION, &row).map(Some)
    }

    /// How `operation` ended, if its `done/` row is still there.
    fn ended(&self, operation: OperationId) -> Result<Option<Finished>, StateError> {
        let key = done_key(operation);
        let Some(row) = self.store.get(&key)? else {
            return Ok(None);
        };
        Finished::from_row(&row.value)
            .map(Some)
            .map_err(|why| corrupt(&key, &why))
    }

    /// Records what a refused write says about the store. Only a store that cannot be
    /// reached is an outage; a write that lost a compare-and-set, or found its row gone,
    /// is the store working; a write the store fenced means another writer holds a newer
    /// generation, which no return of the store undoes. Not generic, unlike
    /// [`Engine::durably`], so every caller's runs count toward the same function.
    fn note_failure(&self, error: &StateError) {
        if let StateError::Store(cause) = error {
            let flag = if matches!(cause, StoreError::Fenced) {
                &self.fenced
            } else {
                &self.unreachable
            };
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// Whether `error` says a newer writer holds the store; if so, this handle is fenced
    /// from now on, as after any other write the store refused for it.
    pub(super) fn superseded_by(&self, error: &StateError) -> bool {
        let fenced = matches!(error, StateError::Store(StoreError::Fenced));
        if fenced {
            self.fenced.store(true, Ordering::SeqCst);
        }
        fenced
    }

    /// Test builds only: the next attempt that names `what` ([`Engine::attempt`]) is
    /// refused with `error` and its write is not made, as if the store had refused it.
    #[cfg(test)]
    pub(super) fn refuse_next(&self, what: &'static str, error: StateError) {
        let mut refusals = self.refusals.lock().unwrap_or_else(PoisonError::into_inner);
        refusals.push((what, error));
    }

    /// Test builds only: the refusal [`Durable::refuse_next`] holds for `what`, taken.
    #[cfg(test)]
    fn refusal_for(&self, what: &str) -> Option<StateError> {
        let mut refusals = self.refusals.lock().unwrap_or_else(PoisonError::into_inner);
        let at = refusals.iter().position(|(named, _)| *named == what)?;
        Some(refusals.remove(at).1)
    }

    /// Whether a write was refused because a newer writer holds the store: nothing this
    /// handle writes can succeed any more.
    pub(super) fn superseded(&self) -> bool {
        self.fenced.load(Ordering::SeqCst)
    }

    /// A write that tells whether the store takes writes again.
    pub(super) fn ping(&self) -> Result<(), StateError> {
        self.commit(Txn::new().put(ALIVE, Vec::new(), Cond::Any))
    }
}

impl Farm {
    /// This farm, with `state` as the truth the scheduler keeps its state in, if given:
    /// every operation, lease grant, result and refusal is written to it before it is
    /// acknowledged, and the unfinished operations and leases it already holds are taken
    /// up again (see [`kbf_sched::Scheduler::recover`]; the callers they had are gone,
    /// and a client that asks for the action again joins the operation). With `None`
    /// the farm is as [`Farm::new`] made it: its state is in memory.
    ///
    /// The term and the lease epoch `Welcome` names are the store's epoch, which
    /// [`StateStore::epoch`] decides: kept, a daemon that reconnects keeps the runs of
    /// its leases; bumped, it kills them. Call it before the farm serves anything.
    ///
    /// The farm numbers its operations and leases in blocks it reserves in the store
    /// ([`ID_BLOCK`] ids at a time); the first blocks are reserved here, past every id any
    /// frontend of the store reserved before.
    ///
    /// # Errors
    /// The store cannot be read or written, or holds a row that is not one the scheduler
    /// wrote.
    pub fn with_state(mut self, state: Option<StateStore>) -> Result<Self, StateError> {
        if let super::driver::Driver::Js(js) = &mut self.driver {
            let epoch = js.keep_in(state, &self.settings)?;
            self.term = epoch.unwrap_or(self.term);
            self.lease_epoch = epoch.unwrap_or(self.lease_epoch);
            return Ok(self);
        }
        let epoch = self.driver.state_mut().keep_in(state)?;
        self.term = epoch.unwrap_or(self.term);
        self.lease_epoch = epoch.unwrap_or(self.lease_epoch);
        Ok(self)
    }
}

impl Farm {
    /// How `operation` ended, as the state store keeps it: `None` if the farm has no
    /// state store, or the operation has no `done/` row (it has not finished, or its row
    /// outlived the finished retention and the floor). The row is the answer a process
    /// that did not finish the operation gives the callers it kept waiting.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not a result.
    pub fn ended(&self, operation: OperationId) -> Result<Option<Finished>, StateError> {
        js_arm!(self, lifecycle::ended(operation));
        let state = self.lock();
        state
            .engine
            .durable
            .as_ref()
            .map_or(Ok(None), |d| d.ended(operation))
    }

    /// Raises the epoch the state store holds by one, and returns the epoch before and the
    /// one stored now; `None` if the farm has no state store. This farm keeps the epoch it
    /// started under (its leases and its `Welcome` name it): the stored one is read by a
    /// frontend when it starts, so the daemons drop the leases of the older epoch when
    /// they reach a frontend started after the bump.
    ///
    /// # Errors
    /// The store cannot be read or written, or holds no epoch.
    pub fn bump_epoch(&self) -> Result<Option<EpochBump>, StateError> {
        js_arm!(self, lifecycle::bump_epoch());
        let state = self.lock();
        state
            .engine
            .durable
            .as_ref()
            .map(Durable::bump_epoch)
            .transpose()
    }
}

impl Engine {
    /// Runs `write` on the state store, if the farm has one. Whether it took: a
    /// refusal is logged here, and the caller does not acknowledge what depends on it.
    pub(super) fn durably(
        &self,
        what: &str,
        write: impl FnOnce(&Durable) -> Result<(), StateError>,
    ) -> bool {
        self.attempt(what, write) == Attempt::Taken
    }

    /// [`Engine::durably`], telling a refusal that a later try can overcome (the store
    /// could not be reached) from one it cannot (the write is stale, or a newer writer
    /// holds the store).
    fn attempt(
        &self,
        what: &str,
        write: impl FnOnce(&Durable) -> Result<(), StateError>,
    ) -> Attempt {
        self.attempt_logging(what, false, write)
    }

    /// [`Engine::attempt`]; with `race_is_expected`, a refusal as stale is not logged here:
    /// the caller knows it is the loss of a race with another frontend and says so itself.
    fn attempt_logging(
        &self,
        what: &str,
        race_is_expected: bool,
        write: impl FnOnce(&Durable) -> Result<(), StateError>,
    ) -> Attempt {
        let Some(durable) = &self.durable else {
            return Attempt::Taken;
        };
        #[cfg(test)]
        let written = durable
            .refusal_for(what)
            .map_or_else(|| write(durable), Err);
        #[cfg(not(test))]
        let written = write(durable);
        match written {
            Ok(()) => Attempt::Taken,
            Err(error) => {
                let race = matches!(error, StateError::Missing(_) | StateError::Conflict(_));
                if !(race && race_is_expected) {
                    tracing::error!(%error, "the state store did not take {what}");
                }
                durable.note_failure(&error);
                match error {
                    StateError::Store(cause) if !matches!(cause, StoreError::Fenced) => {
                        Attempt::Transient
                    }
                    StateError::Missing(_) | StateError::Conflict(_) => Attempt::Stale,
                    StateError::StaleOwner(_) => Attempt::Disowned,
                    _ => Attempt::Dropped,
                }
            }
        }
    }

    /// A newer writer holds the state store ([`Durable::fenced`]): nothing more is
    /// written to it.
    fn superseded(&self) -> bool {
        self.durable
            .as_ref()
            .is_some_and(|d| d.fenced.load(Ordering::SeqCst))
    }

    /// Writes the held-back effects of committed records to the state store, oldest
    /// first, and returns the ones it took, for the caller to carry out. Stops at the
    /// first row the store refuses because it cannot be reached: it and everything after
    /// it wait for the next call, so no later row is written ahead of an earlier one (a
    /// result must not be written before the grant it follows, nor a grant after the
    /// result of the lease it names). A row the store refuses as stale drops its record's
    /// remaining effects, as a refused record always did; so does one for a node another
    /// frontend owns now ([`StateError::StaleOwner`]), and so does each later effect for
    /// that node, one by one, as it meets the same refusal; the effects of other nodes are
    /// written as before. Once a newer writer fences the store, everything held is dropped
    /// and nothing is written.
    pub(super) fn release_unwritten(&mut self, a: &mut dyn Adapter) -> Vec<Effect> {
        let mut ready = Vec::new();
        loop {
            if self.superseded() {
                let held: usize = self.unwritten.iter().map(VecDeque::len).sum();
                if held > 0 {
                    self.write_drops += held as u64;
                    tracing::error!(held, "a newer writer holds the store: held effects dropped");
                }
                // Only the held answers' details go: an answer already written and in
                // `ready` is settled with its own after this returns.
                for batch in std::mem::take(&mut self.unwritten) {
                    self.forget_details(a, batch);
                }
                return ready;
            }
            let Some(front) = self.unwritten.front() else {
                return ready;
            };
            let Some(effect) = front.front().cloned() else {
                self.unwritten.pop_front();
                continue;
            };
            let ending = self.ending(&*a, &effect);
            // A try that the store applied and did not acknowledge is tried again here,
            // and finds its own work in the rows (the write says so: it is `Taken`); a
            // `Stale` is another writer's work in the way, and the effect is dropped.
            let attempt = self.attempt_logging("a committed record", true, |d| {
                d.persist(std::slice::from_ref(&effect), ending.as_ref())
            });
            match attempt {
                Attempt::Taken => {
                    self.unwritten.front_mut().and_then(VecDeque::pop_front);
                    ready.push(effect);
                }
                Attempt::Transient => {
                    let held: usize = self.unwritten.iter().map(VecDeque::len).sum();
                    tracing::warn!(held, "store holds back records; retried later");
                    return ready;
                }
                Attempt::Stale | Attempt::Disowned | Attempt::Dropped => {
                    let dropped = self.unwritten.pop_front().unwrap_or_default();
                    if attempt == Attempt::Stale {
                        // Two frontends took the same queue row and the store kept the
                        // other's write: nothing is lost, this frontend follows the store.
                        self.lost_races += 1;
                        tracing::warn!(
                            operations = %operations_of(&dropped),
                            "another frontend won the race for the operation: this frontend's write is dropped and its placement undone"
                        );
                    } else {
                        self.write_drops += 1;
                        tracing::error!("a committed record's remaining effects were dropped");
                    }
                    // Another frontend's work stands in its place: this one gives up what
                    // it did for the operation and follows the store. A finish dropped for
                    // another reason (the node moved to another frontend, and its daemon
                    // keeps the result for that frontend) is followed too, but its grants
                    // stay: nothing in the store says they lost.
                    if attempt == Attempt::Stale {
                        self.lost_race(a, &dropped);
                    } else {
                        self.follow_finishes(&dropped);
                    }
                    self.forget_details(a, dropped);
                }
            }
        }
    }

    /// Whether the finish of `operation` (its answer or refusal) is held back, unwritten.
    /// Bounded by what is held, which is capped.
    pub(super) fn finish_held(&self, operation: OperationId) -> bool {
        self.unwritten.iter().flatten().any(|effect| match effect {
            Effect::Answer(answer) => answer.operation == operation,
            Effect::Refuse(refusal) => refusal.operation == operation,
            Effect::Start(_) | Effect::Commit(_) | Effect::Waiting(_) => false,
        })
    }

    /// Whether the scheduler holds `operation`, or finished it and holds the finish back,
    /// unwritten. The store's rows of such an operation are no news: taking them in would
    /// run the operation again.
    pub(super) fn known_operation(&self, operation: OperationId) -> bool {
        self.sched.state(operation).is_some() || self.finish_held(operation)
    }

    /// Forgets the details parked for the answers among `dropped` effects, which will
    /// never be written. The details of answers the store has taken stay: they are
    /// settled after this feed's writes, whatever a later record does.
    fn forget_details(&mut self, a: &mut dyn Adapter, dropped: VecDeque<Effect>) {
        for effect in dropped {
            if let Effect::Answer(answer) = effect {
                a.forget_parcel(answer.lease);
                // Never written, so never acknowledged: the daemon keeps the result.
                self.owed.remove(&answer.lease);
            }
        }
    }

    /// Forgets the detail of every report whose answer is no longer held.
    pub(super) fn forget_parked(&mut self, a: &mut dyn Adapter) {
        let held = &self.unwritten;
        let waits = |lease: LeaseId| {
            let answers = |e: &Effect| matches!(e, Effect::Answer(ans) if ans.lease == lease);
            held.iter().flatten().any(answers)
        };
        a.retain_parcels(&waits);
    }

    /// How the operation of `effect` ended, if the effect ends one: the row the store
    /// takes in the transaction that finishes it. An answer's is what its callers are
    /// told, which the adapter decides from the report's detail if one is kept.
    fn ending(&self, a: &dyn Adapter, effect: &Effect) -> Option<Ending> {
        let finished = a.ending(effect)?;
        Some(Ending {
            finished,
            retention: self.finished_retention,
        })
    }

    /// Tells the scheduler when the state store went away and when it came back (see
    /// [`kbf_sched::Scheduler::store_unavailable`]). A write that could not reach it
    /// raises the flag; while it is raised each feed tries one write, and the first that
    /// takes ends the outage. Called at the start of every feed, so no lease is judged
    /// expired by a tick that comes before the farm knows the store is back.
    pub(super) fn watch_store(&mut self, now: FarmTime) {
        let Some(durable) = &self.durable else {
            return;
        };
        if durable.fenced.load(Ordering::SeqCst) {
            // Superseded: the store is reachable but this scheduler may not write it, so
            // it stays unavailable (no grant, no lease given up) and never pings back.
            if !self.store_down {
                tracing::error!("the state store has a newer writer: this scheduler takes no work");
                self.store_down = true;
                self.sched.store_unavailable();
            }
            return;
        }
        if durable.unreachable.load(Ordering::SeqCst) && !self.store_down {
            tracing::warn!("the state store is unavailable: no lease is given up or granted");
            self.store_down = true;
            self.sched.store_unavailable();
        }
        if self.store_down && durable.ping().is_ok() {
            tracing::info!("the state store is back: expiry waits for the lease grace");
            durable.unreachable.store(false, Ordering::SeqCst);
            self.store_down = false;
            self.sched.store_available(now);
        }
    }

    /// Writes the leases the scheduler has just given up, after the ones the state store
    /// has not taken, oldest first, and keeps the ones it still refuses for the next
    /// feed. A grant needs the operation's queue row, which only its requeue writes, so
    /// a requeue the store missed is made up before the grant that follows it, or that
    /// grant is refused and the operation never runs.
    pub(super) fn settle_requeues(&mut self, a: &mut dyn Adapter, given_up: &[Requeue]) {
        // What the kills among them taught the estimator, written once the requeues are.
        let mut kills = Vec::new();
        for requeue in given_up {
            // A grant the store has not taken for a lease the scheduler has given up
            // is never written: it would put the lease back in the store, held by no
            // one, over the queue row this requeue writes.
            for batch in &mut self.unwritten {
                batch.retain(|e| started(e) != Some(requeue.lease));
            }
            let losses = if requeue.reason.is_loss() {
                self.sched.losses(requeue.operation)
            } else {
                None
            };
            // Taken now, not when the store takes the write: a later kill must not
            // reach an earlier requeue's row.
            let fate = self.sched.fate(requeue.operation);
            // Only a kill past the action's own limit raises a floor, and only a raise
            // forgets one.
            let floors = if matches!(requeue.reason, RequeueReason::OutOfMemory { .. }) {
                self.sched.take_floor_change(requeue.operation)
            } else {
                FloorChange::default()
            };
            let learned = a.learned_requeue(&self.view(), requeue);
            if let Some(lowered) = learned.lowered {
                self.lower_key_floors(a, lowered);
            }
            kills.extend(learned.rows);
            // A raise makes any lowered row still waiting for the store stale: the
            // requeue writes the floor as it stands.
            if let Some(raised) = &floors.key_raised {
                self.unwritten_floors.retain(|f| f.key != raised.key);
            }
            self.unsettled.push((requeue.clone(), losses, fate, floors));
        }
        let pending = std::mem::take(&mut self.unsettled);
        // A give-up for a node another frontend owns now is not written again: the node's
        // lease is that frontend's to give up, and the check would refuse it every time.
        let mut disowned = 0;
        let refused: Vec<_> = pending
            .into_iter()
            .filter(|(requeue, losses, fate, floors)| {
                let summary = floors
                    .key_raised
                    .as_ref()
                    .and_then(|f| a.summary_key(f.key));
                let floors = self.current_key_floors(floors);
                let attempt = self.attempt("a requeue", |d| {
                    d.requeued_named(requeue, *losses, fate.as_ref(), &floors, summary.as_ref())
                });
                disowned += u64::from(attempt == Attempt::Disowned);
                !matches!(attempt, Attempt::Taken | Attempt::Disowned)
            })
            .collect();
        self.unsettled = refused;
        self.write_drops += disowned;
        // The queue row a give-up writes is this frontend's own: the intake poll finds the
        // operation queued as it left it, not queued again by another frontend.
        for requeue in given_up {
            self.intake.rewrote(requeue.operation);
        }
        self.write_estimates(a, &kills);
    }

    /// Whether the refusal that `record` proposes is taken back because a daemon connected
    /// to another frontend of the state store could run the operation. The scheduler judged
    /// the operation unservable from its own daemons; the session rows of the store hold
    /// every frontend's, so they decide whether the operation is refused. An operation
    /// held waits again for the unservable wait, and the rows are read again then. A store
    /// whose session rows cannot be read holds it too: a refusal is never final on a guess.
    pub(super) fn held_for_other_frontends(&mut self, record: &ControlRecord) -> bool {
        let ControlRecord::Refusal(refusal) = record else {
            return false;
        };
        let Some(durable) = &self.durable else {
            return false;
        };
        self.sched.hold_refusal(refusal.operation, |request| {
            durable.session_could_run(request).unwrap_or_else(|error| {
                tracing::warn!(%error, "the daemon sessions cannot be read: the refusal waits");
                true
            })
        })
    }

    /// Renews `worker`'s session row if its last write is a renewal interval old: one
    /// write per daemon session, never one per lease. Called for a heartbeat taken.
    pub(super) fn renew_session(&mut self, now: FarmTime, worker: &WorkerId) {
        // A fenced farm writes nothing: the successor's rows are the truth.
        if self.durable.is_none() || self.fenced {
            return;
        }
        let due = self
            .renewed
            .get(worker)
            .is_none_or(|at| now >= at.saturating_add(SESSION_RENEWAL));
        if due && self.durably("a session renewal", |d| d.renew(worker)) {
            self.renewed.insert(worker.clone(), now);
            self.session_ok.insert(worker.clone(), now);
        }
    }

    /// `op`, an operation the state store holds, as the scheduler takes it up: held for
    /// a caller of its own that no client waits on, which the next request for the
    /// action joins.
    fn take_stored(
        &mut self,
        a: &mut dyn Adapter,
        mut op: StoredOperation,
        now: SystemTime,
    ) -> Recovered {
        let waiter = a.adopt(&mut op, now);
        if op.lease.is_some() {
            // A recovered operation that holds a lease has been started: if it is queued
            // again, its streams read `WaitingKept`.
            self.ever_started.insert(op.operation);
        }
        op.recovered(waiter)
    }
}

impl State {
    /// [`Farm::with_state`] on the farm's state: opens `state`, reads what it holds and
    /// takes it up. Returns the store's epoch, or `None` if there is no store. Not
    /// generic, unlike `Farm::with_state`, so every test binary's runs count toward the
    /// same function's coverage.
    fn keep_in(&mut self, state: Option<StateStore>) -> Result<Option<u64>, StateError> {
        let Some(StateStore { store, epoch, .. }) = state else {
            return Ok(None);
        };
        let (durable, epoch) = Durable::open(store, epoch, self.engine.ambient.term())?;
        durable
            .read_size_keys
            .store(self.side.learned.keys(), Ordering::SeqCst);
        let stored = durable.load()?;
        // The farm hands out `operations/<id>` names, whose ids are the store's. The machine's
        // term is the one of the old form, `operations/{term}-{waiter}`, which the farm still
        // answers for a caller it holds: it carries the writer generation when the store has
        // one, so a kept epoch does not let a process answer a name another one wrote (issue
        // #154). A store with none (a store with no generation row, or one holding the shared
        // marker) gives the epoch as the term. The farm gives out no name of the old form
        // over a store, so no client holds one for two frontends of a store to answer.
        self.side.machine = FarmMachine::new(durable.generation()?.unwrap_or(epoch));
        let now = self.engine.ambient.wall();
        // The summaries come first: a queued operation is predicted from them as it is read
        // back (`--placement-predictions`), by a lookup in memory. Off reads none and leaves
        // them as they are.
        if !self.side.learned.off() {
            self.side.learned.restore(&durable.estimates()?);
        }
        let mut recovered = Vec::new();
        let mut keyed = Vec::new();
        let mut walled = Vec::new();
        let mut predicted = Vec::new();
        // A start takes up the operations that wait for a lease, and none that holds one:
        // the lease belongs to the frontend its node is connected to, which may be another
        // than this. A leased operation is taken up when its node says `Hello` here
        // ([`Engine::claim_worker`]); if the node never does, the orphan sweep requeues it.
        for op in stored
            .operations
            .into_iter()
            .filter(|op| op.lease.is_none())
        {
            if let Some(size_key) = op.size_key.filter(|_| self.side.learned.keys()) {
                keyed.push((op.operation, size_key));
                self.side.register_stored(&op, size_key);
            }
            if let Some(wall) = op.max_wall {
                walled.push((op.operation, wall));
            }
            if let Some(prediction) = self.side.predict_stored(&self.engine.sched, &op) {
                predicted.push((op.operation, prediction));
            }
            recovered.push(self.engine.take_stored(&mut self.side, op, now));
        }
        // The first blocks of ids are reserved here, past everything any frontend of the
        // store reserved, and the scheduler numbers inside them from now on. Every start
        // reserves, so a start never takes an id an earlier one took, whether or not the
        // rows of the operations that used them are still there.
        let first_operation = durable.reserve_operation(OperationId(stored.next_operation))?;
        let seqs = durable.reserve_seqs(stored.next_seq)?;
        self.engine.sched.restore_floors(stored.floors);
        self.engine.sched.restore_key_floors(stored.key_floors);
        let stored_names = stored.key_names;
        self.engine
            .sched
            .recover(epoch, first_operation, seqs.start, recovered);
        self.engine.sched.use_seq_block(seqs);
        // The longest each operation may run comes from the store's rows, given before the
        // first placement round: the whole-node drain order and deadlines read it.
        for (operation, wall) in walled {
            self.engine.sched.set_max_wall(operation, wall);
        }
        // So is the predicted cost of a queued operation: it is not in the store, and the
        // first placement round after the start reads it.
        for (operation, prediction) in predicted {
            self.engine.sched.set_cost(operation, prediction);
        }
        // The size keys come from the store's own rows, not from the requests: only `on`
        // reads them. A recovered lease keeps the request the store holds; a key floor
        // raises only the operations queued after it.
        for (operation, key) in keyed {
            self.engine.sched.set_size_key(operation, key);
        }
        // A key whose operations have all finished still has its floor and its decay.
        if self.side.learned.keys() {
            for (size_key, summary) in &stored_names {
                self.side.learned.register(summary, *size_key);
            }
        }
        self.engine
            .restore_cordons(stored.cordons, durable.store_ms());
        self.engine.durable = Some(durable);
        Ok(Some(epoch))
    }
}

impl Engine {
    /// Writes `request`, about to be queued, to the state store, unless it will join a
    /// twin already there, and says where it is held ([`Held`]). Not generic, unlike
    /// `Farm::submit`, so every test binary's runs count toward the same function's
    /// coverage.
    ///
    /// A twin the store holds that this scheduler does not is taken up as an operation of
    /// its own, queued or leased on a node this frontend owns (`now` is farm time, for the
    /// look at the owner). One leased on a node another frontend owns is not taken up: the
    /// lease is that frontend's, and this scheduler would give it up when the node did not
    /// register here. The request follows the operation through the store instead.
    ///
    /// # Errors
    /// UNAVAILABLE if the store did not take it: the request is not queued, and the
    /// client, who is told nothing is queued, asks again.
    pub(super) fn queue(
        &mut self,
        a: &mut dyn Adapter,
        request: &Request,
        subject: &Subject,
        size_key: Option<u64>,
        max_wall: Option<Duration>,
        now: FarmTime,
    ) -> Result<Held, Status> {
        if self.sched.twin(request).is_some() {
            return Ok(Held::Here);
        }
        let wanted = self.sched.next_operation();
        let mut submitted = None;
        let taken = self.durably("a submission", |d| {
            // The id comes from the block this frontend reserved: the operation's row
            // is put under an id no other frontend takes.
            let id = d.reserve_operation(wanted)?;
            submitted = Some(d.submit_keyed(id, request, subject, size_key, max_wall)?);
            Ok(())
        });
        if !taken {
            if self
                .durable
                .as_ref()
                .is_some_and(|d| d.fenced.load(Ordering::SeqCst))
            {
                return Err(Status::failed_precondition(
                    "this scheduler was superseded by a newer writer of the state store",
                ));
            }
            return Err(Status::unavailable(
                "the scheduler's state store did not take the request",
            ));
        }
        match submitted {
            Some(Submitted::Queued(used)) => self.sched.skip_to(used),
            Some(Submitted::Joined(op)) => {
                // The store names an operation the scheduler holds and has no twin
                // for: it finished it, and the finish (which removes the rows) is
                // held back. The request cannot join a finished operation, and
                // taking the rows up again would run the action twice. It asks again:
                // once the finish is written, the rows are gone and it queues afresh
                // (or the cache answers it).
                //
                // The scheduler retires a finished operation after its retention
                // whether or not the finish was written, so the held finish itself is
                // asked too: while it waits, the operation's rows are not free.
                if self.known_operation(op.operation) {
                    return Err(Status::unavailable(
                        "the action's previous run is finishing; the request is not queued",
                    ));
                }
                if self.leased_elsewhere(&op, now) {
                    return Ok(Held::Elsewhere(op.operation));
                }
                self.take_in_stored(a, *op);
            }
            None => {}
        }
        Ok(Held::Here)
    }

    /// Whether `op` holds a lease on a node this frontend does not own at `now`: its daemon
    /// is connected to another frontend, or to none (the daemon that ran it has not said
    /// `Hello` anywhere since the lease was written, and the orphan sweep requeues the lease
    /// if it never does). A store whose owner row cannot be read is the same: nothing is
    /// taken up on a guess.
    fn leased_elsewhere(&self, op: &StoredOperation, now: FarmTime) -> bool {
        let lease = op.lease.as_ref();
        lease.is_some_and(|grant| !self.owns_worker(now, &grant.worker))
    }

    /// Takes `op`, which the state store holds and the scheduler neither holds nor has
    /// finished (the caller checked), into the scheduler, held for a caller of its own that
    /// no client waits on, with the size key its row names if the learned sizes read it and
    /// the longest wall time its row names.
    pub(super) fn take_in_stored(&mut self, a: &mut dyn Adapter, op: StoredOperation) {
        let operation = op.operation;
        let stored_key = op.size_key.filter(|_| a.size_keys());
        let stored_wall = op.max_wall;
        let recovered = self.take_stored(a, op, self.ambient.wall());
        // The operation is neither in the scheduler nor finished, so `take_in` has nothing
        // to refuse.
        let taken = self.sched.take_in(recovered);
        if let Some(key) = stored_key {
            self.sched.set_size_key(operation, key);
        }
        if let Some(wall) = stored_wall {
            self.sched.set_max_wall(operation, wall);
        }
        debug_assert!(taken, "the stored operation was already in the scheduler");
    }

    /// Writes the session row of a daemon that registered. Not generic, unlike
    /// `Farm::register`.
    pub(super) fn record_session(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        instance: &DaemonInstance,
        predecessor_stopped: bool,
        reported: &Reported,
    ) {
        let written = self.durably("a session", |d| {
            d.session(worker, instance, predecessor_stopped, reported)
        });
        // A registration the store refused is written at the next heartbeat, which
        // renews: a daemon whose row is not there is not acknowledged.
        if written {
            self.renewed.insert(worker.clone(), now);
            self.session_ok.insert(worker.clone(), now);
        }
    }
}

#[cfg(test)]
mod blocks_tests;
#[cfg(test)]
mod codec_tests;
#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod intake_tests;
#[cfg(test)]
mod leases_tests;
#[cfg(test)]
mod named_tests;
#[cfg(test)]
mod owner_tests;
#[cfg(test)]
mod place_tests;
#[cfg(test)]
mod reads_tests;
#[cfg(test)]
mod refusal_tests;
#[cfg(test)]
mod results_tests;
#[cfg(test)]
mod submit_tests;
#[cfg(test)]
mod sweep_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod timing_tests;
#[cfg(test)]
mod transition_tests;
#[cfg(test)]
mod unknown_tests;
#[cfg(test)]
mod wall_intake_tests;
