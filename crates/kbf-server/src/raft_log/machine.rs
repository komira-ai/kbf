//! The state machine the log replicates: the metadata state, fed every committed
//! command entry in index order.

use kbf_log::{Bootstrap, DecodeError, Format, LogCommand};
use kbf_meta::{Applied, MetaState, Retention};
use kbf_raft::{Entry, LogIndex, Machine, Payload, Snapshot};

/// Why a snapshot cannot be restored: this version writes none and reads none.
#[derive(Debug, thiserror::Error)]
#[error("the raft dir holds a snapshot; this kbf-server writes and reads none")]
pub struct NoSnapshots;

/// [`MetaState`] as a [`Machine`]: each committed command entry is decoded and
/// applied in order. The first [`Bootstrap`] is kept and every later one is a no-op.
/// An entry that does not decode stops the machine: it applies nothing more, and the
/// log refuses to open ([`MetaMachine::failed`] names the first such entry). Once open,
/// the log applies only entries it encoded itself.
#[derive(Debug)]
pub struct MetaMachine {
    meta: MetaState,
    bootstrap: Option<Bootstrap>,
    bootstraps: usize,
    /// The index and outcome of the last metadata command applied, for the caller
    /// that proposed it.
    answer: Option<(LogIndex, Applied)>,
    failed: Option<(LogIndex, DecodeError)>,
}

impl MetaMachine {
    /// An empty metadata state with `retention`, before any entry.
    pub fn new(retention: Retention) -> Self {
        Self {
            meta: MetaState::new(retention),
            bootstrap: None,
            bootstraps: 0,
            answer: None,
            failed: None,
        }
    }

    /// The metadata state as of the last entry applied.
    pub const fn meta(&self) -> &MetaState {
        &self.meta
    }

    /// The first `Bootstrap` applied, if one has been.
    pub const fn bootstrap(&self) -> Option<&Bootstrap> {
        self.bootstrap.as_ref()
    }

    /// How many `Bootstrap` entries this machine has applied, the no-ops included.
    pub const fn bootstraps(&self) -> usize {
        self.bootstraps
    }

    /// What applying the entry at `index` did, if it was the last metadata command.
    pub fn answer(&self, index: LogIndex) -> Option<Applied> {
        self.answer
            .as_ref()
            .filter(|(at, _)| *at == index)
            .map(|(_, applied)| applied.clone())
    }

    /// The entry that did not decode, and why, once one has stopped the machine.
    pub const fn failed(&self) -> Option<&(LogIndex, DecodeError)> {
        self.failed.as_ref()
    }
}

impl Machine for MetaMachine {
    type Error = NoSnapshots;

    fn apply(&mut self, entry: &Entry) {
        let Payload::Command(bytes) = &entry.payload else {
            return; // a leader's blank entry
        };
        if self.failed.is_some() {
            return;
        }
        match kbf_log::decode(Format::V1, bytes) {
            Ok(LogCommand::Meta(command)) => {
                let applied = self.meta.execute(entry.id.index.0, command);
                self.answer = Some((entry.id.index, applied));
            }
            Ok(LogCommand::Bootstrap(bootstrap)) => {
                self.bootstraps += 1;
                self.bootstrap.get_or_insert(bootstrap);
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
