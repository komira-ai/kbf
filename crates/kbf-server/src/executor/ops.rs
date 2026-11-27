//! The operations a process holds: the open ones by id and by action, their callers' stage, the
//! cancel of each, and the finished ones a `WaitExecution` may still ask for by name.
//!
//! A caller of an open operation is a receiver of its stage: each Execute and WaitExecution
//! stream holds one for as long as it is open, and a GetOperation or CancelOperation for the
//! length of the call. Every receiver is made here ([`Ops::open`], [`Ops::join`],
//! [`Ops::ticket`]), and each one made after the first counts a join ([`Callers::joins`]), which
//! is what wakes the watch that cancels an operation no caller is left on
//! ([`crate::executor::callers`]).

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use kbf_front::{Finished, Stage, Ticket};
use kbf_types::{ActionKey, OperationId};
use tokio::sync::watch;
use tokio::time::Instant;

use super::stored_name;

/// An operation being run here.
struct Open {
    key: ActionKey,
    /// What its callers see.
    stage: watch::Sender<Stage>,
    /// Set when a caller cancels it.
    cancel: watch::Sender<bool>,
    /// How many receivers of `stage` were made after the first: dropped with the operation,
    /// which ends the watch of its callers.
    joins: watch::Sender<u64>,
    /// Whether the pack of a job that ended is being read: no caller is needed for that.
    reading: bool,
}

impl Open {
    /// A new receiver of the stage, counted as a join.
    fn subscribe(&self) -> watch::Receiver<Stage> {
        let stage = self.stage.subscribe();
        self.joins
            .send_modify(|joins| *joins = joins.wrapping_add(1));
        stage
    }
}

/// An operation another frontend runs, watched here for a caller that asked for it by name.
struct Followed {
    key: ActionKey,
    stage: watch::Sender<Stage>,
}

/// The table.
pub(crate) struct Ops {
    open: BTreeMap<OperationId, Open>,
    by_key: BTreeMap<ActionKey, OperationId>,
    followed: BTreeMap<OperationId, Followed>,
    /// The finished operations, in the order they finished, and when.
    finished: BTreeMap<OperationId, (ActionKey, Finished)>,
    ended: VecDeque<(Instant, OperationId)>,
    /// The next id of a process with no state store.
    next: u64,
}

/// What the watch of an open operation's callers sees of it: how many receivers of its stage
/// there are, and when another is made.
pub(crate) struct Callers {
    /// The stage, for its receivers alone: a watch holds it and sends nothing on it.
    pub(crate) stage: watch::Sender<Stage>,
    /// Changed at each join; closed once the operation is no longer open here (it finished, or
    /// another process runs it now).
    pub(crate) joins: watch::Receiver<u64>,
}

/// What [`Ops::cancel_unwatched`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unwatched {
    /// The operation is not open here, or a caller cancelled it already.
    Gone,
    /// A caller is on it.
    Watched,
    /// Its job ended and its pack is being read, which a cancel would throw away.
    Reading,
    /// No caller is on it: it is cancelled now.
    Cancelled,
}

/// The operation a request joins: one already open for the same action.
pub(crate) struct Joined {
    pub(crate) operation: OperationId,
    pub(crate) stage: watch::Receiver<Stage>,
}

impl Ops {
    /// An empty table, giving ids from `base` on when the process has no state store.
    pub(crate) fn new(base: Option<u64>) -> Self {
        Self {
            open: BTreeMap::new(),
            by_key: BTreeMap::new(),
            followed: BTreeMap::new(),
            finished: BTreeMap::new(),
            ended: VecDeque::new(),
            next: base.unwrap_or_default(),
        }
    }

    /// The next operation id of a process with no state store.
    pub(crate) fn next_id(&mut self) -> OperationId {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        OperationId(id)
    }

    /// How many operations are open.
    pub(crate) fn open_count(&self) -> usize {
        self.open.len()
    }

    /// Whether `operation` is open here.
    pub(crate) fn is_open(&self, operation: OperationId) -> bool {
        self.open.contains_key(&operation)
    }

    /// The open operation of `key`, for a request of the same action to join; none once a
    /// caller has cancelled it, which a new request would only be answered with.
    pub(crate) fn join(&self, key: &ActionKey) -> Option<Joined> {
        let operation = *self.by_key.get(key)?;
        let open = self
            .open
            .get(&operation)
            .filter(|open| !*open.cancel.borrow())?;
        Some(Joined {
            operation,
            stage: open.subscribe(),
        })
    }

    /// Opens `operation` of `key`: its callers' stage, and the receiving end of its cancel.
    pub(crate) fn open(
        &mut self,
        operation: OperationId,
        key: ActionKey,
    ) -> (watch::Receiver<Stage>, watch::Receiver<bool>) {
        let (stage, seen) = watch::channel(Stage::Queued);
        let (cancel, cancelled) = watch::channel(false);
        let (joins, _) = watch::channel(0);
        // An operation taken up from the state store does not displace a live one of the same
        // action, which new requests go on joining; a cancelled one is displaced.
        let live = self
            .by_key
            .get(&key)
            .and_then(|held| self.open.get(held))
            .is_some_and(|held| !*held.cancel.borrow());
        if !live {
            self.by_key.insert(key.clone(), operation);
        }
        self.open.insert(
            operation,
            Open {
                key,
                stage,
                cancel,
                joins,
                reading: false,
            },
        );
        (seen, cancelled)
    }

    /// What the watch of the callers of the open `operation` sees of it; none when it is not
    /// open here.
    pub(crate) fn callers(&self, operation: OperationId) -> Option<Callers> {
        self.open.get(&operation).map(|open| Callers {
            stage: open.stage.clone(),
            joins: open.joins.subscribe(),
        })
    }

    /// Cancels the open `operation` if no caller is on it and no caller has cancelled it: the
    /// same cancel as [`Ops::cancel`], which its task answers CANCELLED.
    pub(crate) fn cancel_unwatched(&self, operation: OperationId) -> Unwatched {
        let Some(open) = self.open.get(&operation) else {
            return Unwatched::Gone;
        };
        if open.stage.receiver_count() > 0 {
            return Unwatched::Watched;
        }
        if open.reading {
            return Unwatched::Reading;
        }
        if open.cancel.send_replace(true) {
            Unwatched::Gone
        } else {
            Unwatched::Cancelled
        }
    }

    /// Marks the open `operation`'s job as ended with its pack being read (`true`), or the read
    /// as over: while it is read, the operation is not cancelled for want of a caller.
    pub(crate) fn reading(&mut self, operation: OperationId, reading: bool) {
        if let Some(open) = self.open.get_mut(&operation) {
            open.reading = reading;
        }
    }

    /// How many callers of this process wait on the followed `operation`: its receivers.
    pub(crate) fn followers(&self, operation: OperationId) -> usize {
        self.followed
            .get(&operation)
            .map_or(0, |followed| followed.stage.receiver_count())
    }

    /// Shows `stage` to the callers of the open `operation`, if it is not what they see.
    pub(crate) fn show(&self, operation: OperationId, stage: Stage) {
        if let Some(open) = self.open.get(&operation) {
            open.stage.send_if_modified(|seen| {
                let changed = *seen != stage;
                *seen = stage;
                changed
            });
        }
    }

    /// Asks the open `operation` to stop. False when it is not open here.
    pub(crate) fn cancel(&self, operation: OperationId) -> bool {
        self.open.get(&operation).is_some_and(|open| {
            open.cancel.send_replace(true);
            true
        })
    }

    /// Answers the open `operation` `finished`, at `now`, and keeps the answer for a
    /// `WaitExecution` by name.
    pub(crate) fn finish(&mut self, operation: OperationId, finished: Finished, now: Instant) {
        let Some(open) = self.open.remove(&operation) else {
            return;
        };
        if self.by_key.get(&open.key) == Some(&operation) {
            self.by_key.remove(&open.key);
        }
        open.stage.send_replace(Stage::Done(finished.clone()));
        self.finished.insert(operation, (open.key, finished));
        self.ended.push_back((now, operation));
    }

    /// Takes the open `operation` out of this process's hands, for another that runs it now:
    /// its callers' stage becomes a follower's, which [`Ops::follow_done`] ends.
    pub(crate) fn hand_over(&mut self, operation: OperationId) {
        let Some(open) = self.open.remove(&operation) else {
            return;
        };
        if self.by_key.get(&open.key) == Some(&operation) {
            self.by_key.remove(&open.key);
        }
        self.followed.insert(
            operation,
            Followed {
                key: open.key,
                stage: open.stage,
            },
        );
    }

    /// The stage of a follower of `operation` of `key`: a new one, or the follower already
    /// there. True with it when the follower is new, and the caller starts its watch.
    pub(crate) fn follower(
        &mut self,
        operation: OperationId,
        key: ActionKey,
    ) -> (watch::Receiver<Stage>, bool) {
        if let Some(followed) = self.followed.get(&operation) {
            return (followed.stage.subscribe(), false);
        }
        let (stage, seen) = watch::channel(Stage::Queued);
        self.followed.insert(operation, Followed { key, stage });
        (seen, true)
    }

    /// Whether `operation` is followed here.
    pub(crate) fn is_followed(&self, operation: OperationId) -> bool {
        self.followed.contains_key(&operation)
    }

    /// Ends the follower of `operation` with `finished`, kept like an answer of this process.
    pub(crate) fn follow_done(&mut self, operation: OperationId, finished: Finished, now: Instant) {
        let Some(followed) = self.followed.remove(&operation) else {
            return;
        };
        followed.stage.send_replace(Stage::Done(finished.clone()));
        self.finished.insert(operation, (followed.key, finished));
        self.ended.push_back((now, operation));
    }

    /// A ticket on `operation`, open, followed or finished here.
    pub(crate) fn ticket(&self, operation: OperationId) -> Option<Ticket> {
        let ticket = |key: &ActionKey, stage| Ticket {
            name: stored_name(operation),
            instance: key.instance.clone(),
            action: key.action,
            stage,
        };
        if let Some(open) = self.open.get(&operation) {
            return Some(ticket(&open.key, open.subscribe()));
        }
        if let Some(followed) = self.followed.get(&operation) {
            return Some(ticket(&followed.key, followed.stage.subscribe()));
        }
        let (key, finished) = self.finished.get(&operation)?;
        let (_, stage) = watch::channel(Stage::Done(finished.clone()));
        Some(ticket(key, stage))
    }

    /// Forgets the answers kept longer than `retention` at `now`.
    pub(crate) fn prune(&mut self, now: Instant, retention: Duration) {
        while let Some((at, operation)) = self.ended.front().copied() {
            if now.saturating_duration_since(at) < retention {
                break;
            }
            self.ended.pop_front();
            self.finished.remove(&operation);
        }
    }
}

#[cfg(test)]
#[path = "ops_tests.rs"]
mod tests;
