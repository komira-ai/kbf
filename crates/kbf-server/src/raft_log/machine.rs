//! The state machine the log replicates: the metadata state, fed every committed
//! command entry in index order.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use kbf_log::{Bootstrap, DecodeError, Format, LogCommand};
use kbf_meta::{Applied, MetaState, Retention};
use kbf_raft::{Entry, LogIndex, Machine, Payload, Snapshot};

/// Why a snapshot cannot be restored: this version writes none and reads none.
#[derive(Debug, thiserror::Error)]
#[error("the raft dir holds a snapshot; this kbf-server writes and reads none")]
pub struct NoSnapshots;

/// [`MetaState`] as a [`Machine`]: each committed command entry is decoded and
/// applied in order. The first [`Bootstrap`] is kept and every later one is a no-op.
/// An entry that does not decode, or a control command (which this log does not
/// apply), stops the machine: it applies nothing more, and the
/// log refuses to open ([`MetaMachine::failed`] names the first such entry). Once open,
/// the log applies only entries it encoded itself.
///
/// The state sits behind its own lock ([`MetaMachine::state`]), so a query reads it
/// without the host's lock, which a commit holds across its sync. The host applies an
/// entry only after it is synced, so a query never sees a command that is not durable.
#[derive(Debug)]
pub struct MetaMachine {
    meta: Arc<RwLock<MetaState>>,
    bootstrap: Option<Bootstrap>,
    bootstraps: usize,
    /// The outcome of each metadata command applied since [`MetaMachine::keep_answers`]
    /// and not yet taken, by index, for the caller that proposed it. `None` until then:
    /// the entries a start replays have no caller to answer.
    answers: Option<BTreeMap<LogIndex, Applied>>,
    failed: Option<(LogIndex, DecodeError)>,
    /// The last entry handed to [`Machine::apply`], blank entries included.
    applied: LogIndex,
}

impl MetaMachine {
    /// An empty metadata state with `retention`, before any entry.
    pub fn new(retention: Retention) -> Self {
        Self {
            meta: Arc::new(RwLock::new(MetaState::new(retention))),
            bootstrap: None,
            bootstraps: 0,
            answers: None,
            failed: None,
            applied: LogIndex(0),
        }
    }

    /// The metadata state as of the last entry applied.
    pub fn meta(&self) -> RwLockReadGuard<'_, MetaState> {
        read(&self.meta)
    }

    /// The lock the state sits behind, for readers that do not hold the host.
    pub fn state(&self) -> Arc<RwLock<MetaState>> {
        Arc::clone(&self.meta)
    }

    /// The first `Bootstrap` applied, if one has been.
    pub const fn bootstrap(&self) -> Option<&Bootstrap> {
        self.bootstrap.as_ref()
    }

    /// How many `Bootstrap` entries this machine has applied, the no-ops included.
    pub const fn bootstraps(&self) -> usize {
        self.bootstraps
    }

    /// Keeps the outcome of every metadata command applied from now on, until
    /// [`MetaMachine::take_answer`] takes it.
    pub fn keep_answers(&mut self) {
        self.answers.get_or_insert_default();
    }

    /// What applying the metadata command at `index` did, if it was applied since
    /// [`MetaMachine::keep_answers`] and not taken before.
    pub fn take_answer(&mut self, index: LogIndex) -> Option<Applied> {
        self.answers
            .as_mut()
            .and_then(|answers| answers.remove(&index))
    }

    /// The last entry the host handed this machine, blank entries and entries past
    /// a stop included. The core's applied index can be ahead of it: a host that met
    /// a storage error mid-input applies nothing more of that input.
    pub const fn applied(&self) -> LogIndex {
        self.applied
    }

    /// The entry that did not decode, and why, once one has stopped the machine.
    pub const fn failed(&self) -> Option<&(LogIndex, DecodeError)> {
        self.failed.as_ref()
    }
}

impl Machine for MetaMachine {
    type Error = NoSnapshots;

    fn apply(&mut self, entry: &Entry) {
        self.applied = entry.id.index;
        let Payload::Command(bytes) = &entry.payload else {
            return; // a leader's blank entry
        };
        if self.failed.is_some() {
            return;
        }
        match kbf_log::decode(Format::V1, bytes) {
            Ok(LogCommand::Meta(command)) => {
                let applied = self
                    .meta
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .execute(entry.id.index.0, command);
                if let Some(answers) = &mut self.answers {
                    answers.insert(entry.id.index, applied);
                }
            }
            Ok(LogCommand::Bootstrap(bootstrap)) => {
                self.bootstraps += 1;
                self.bootstrap.get_or_insert(bootstrap);
            }
            // A control command: this log applies only metadata commands and its
            // `Bootstrap`, so a later binary wrote it. Stop, as for an entry that does
            // not decode.
            Ok(_) => {
                self.failed = Some((entry.id.index, DecodeError::UnknownCommand("LogCommand")));
            }
            Err(e) => self.failed = Some((entry.id.index, e)),
        }
    }

    /// Never called: the log takes no snapshot in this version (the metadata state has
    /// no snapshot encoding yet), so its segments are never compacted. A snapshot of
    /// no bytes would let the host drop every entry it covers.
    fn snapshot(&self) -> Vec<u8> {
        unreachable!("the metadata log takes no snapshot");
    }

    fn restore(&mut self, _: &Snapshot) -> Result<(), NoSnapshots> {
        Err(NoSnapshots)
    }
}

/// Reads `state`. `MetaState::execute` does not panic midway through a change, so a
/// poisoned state is still whole.
pub fn read(state: &RwLock<MetaState>) -> RwLockReadGuard<'_, MetaState> {
    state.read().unwrap_or_else(PoisonError::into_inner)
}
