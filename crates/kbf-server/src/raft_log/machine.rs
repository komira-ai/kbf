//! The state machine the log replicates: the metadata state, fed every committed
//! command entry in index order.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use kbf_log::{Bootstrap, DecodeError, Format, LogCommand, MachineVersion};
use kbf_meta::{Applied, MetaState, Retention};
use kbf_raft::{Entry, LogIndex, Machine, Payload, Snapshot};

/// Why a snapshot cannot be restored: this version writes none and reads none.
#[derive(Debug, thiserror::Error)]
#[error("the raft dir holds a snapshot; this kbf-server writes and reads none")]
pub struct NoSnapshots;

/// [`MetaState`] as a [`Machine`]: each committed command entry is decoded, under the
/// format of the machine version committed before it, and applied in order. The first
/// [`Bootstrap`] is kept and every later one is a no-op. A [`MachineVersion`] entry
/// raises the version (a lower or equal one changes nothing), and with it the format
/// of every later entry. An entry that does not decode stops the machine: it applies
/// nothing more, and the log refuses to open ([`MetaMachine::failed`] names the first
/// such entry). Once open, the log applies only entries it encoded itself.
///
/// Every format changes additively, and a binary skips what it does not know instead of
/// refusing it. So the machine skips, and goes on with the next entry, each of:
/// - a command this binary does not know ([`LogCommand::Unknown`]);
/// - a control command, which this log does not use (`Farm`, `Report`, `Takeover` and
///   `Checkpoint`);
/// - a [`MachineVersion`] entry naming a version past the one this binary runs, or one it
///   does not know. The machine keeps the version and format it has; decoding does not
///   depend on the format, so every command it knows still applies.
///
/// A skipped entry changes nothing and sets no [`MetaMachine::failed`]. It is counted
/// ([`MetaMachine::skipped`]), and the first of each kind is logged with its index. What
/// the skipped entry would have done is lost to this replica, so a log with more than one
/// voter needs every voter on releases that know the same commands (logs are single-voter
/// today).
///
/// The state sits behind its own lock ([`MetaMachine::state`]), so a query reads it
/// without the host's lock, which a commit holds across its sync. The host applies an
/// entry only after it is synced, so a query never sees a command that is not durable.
#[derive(Debug)]
pub struct MetaMachine {
    meta: Arc<RwLock<MetaState>>,
    bootstrap: Option<Bootstrap>,
    bootstraps: usize,
    /// The highest machine version this binary runs.
    runs: MachineVersion,
    /// The machine version committed so far; [`MachineVersion::V1`] in a new log.
    version: MachineVersion,
    /// The entry format of `version`.
    format: Format,
    /// The outcome of each metadata command applied since [`MetaMachine::keep_answers`]
    /// and not yet taken, by index, for the caller that proposed it. `None` until then:
    /// the entries a start replays have no caller to answer.
    answers: Option<BTreeMap<LogIndex, Applied>>,
    failed: Option<(LogIndex, DecodeError)>,
    /// How many entries were skipped, of any kind.
    skipped: u64,
    /// The kinds of entry skipped so far: each is logged once, at its first.
    skipped_kinds: BTreeSet<&'static str>,
    /// The last entry handed to [`Machine::apply`], blank entries included.
    applied: LogIndex,
}

impl MetaMachine {
    /// An empty metadata state with `retention`, before any entry, for a binary that
    /// runs machine versions up to `runs`; `None` if this binary does not know `runs`.
    pub fn new(retention: Retention, runs: MachineVersion) -> Option<Self> {
        runs.format()?;
        Some(Self {
            meta: Arc::new(RwLock::new(MetaState::new(retention))),
            bootstrap: None,
            bootstraps: 0,
            runs,
            version: MachineVersion::V1,
            format: Format::V1,
            answers: None,
            failed: None,
            skipped: 0,
            skipped_kinds: BTreeSet::new(),
            applied: LogIndex(0),
        })
    }

    /// The metadata state as of the last entry applied.
    pub fn meta(&self) -> RwLockReadGuard<'_, MetaState> {
        read(&self.meta)
    }

    /// The lock the state sits behind, for readers that do not hold the host.
    pub fn state(&self) -> Arc<RwLock<MetaState>> {
        Arc::clone(&self.meta)
    }

    /// The machine version committed so far.
    pub const fn version(&self) -> MachineVersion {
        self.version
    }

    /// The entry format of the committed machine version: what the next entry is
    /// encoded under.
    pub const fn format(&self) -> Format {
        self.format
    }

    /// The highest machine version this binary runs.
    pub const fn runs(&self) -> MachineVersion {
        self.runs
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

    /// How many entries the machine has skipped: commands this binary does not know,
    /// control commands, and machine versions past the one it runs.
    pub const fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Counts the entry at `index` as skipped, and logs it if it is the first of its
    /// `kind`.
    fn skip(&mut self, index: LogIndex, kind: &'static str) {
        self.skipped += 1;
        if self.skipped_kinds.insert(kind) {
            tracing::warn!(
                %kind,
                index = index.0,
                "skipped a log entry this binary does not apply (the kind is logged once)"
            );
        }
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
        match kbf_log::decode(self.format, bytes) {
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
            Ok(LogCommand::MachineVersion(version)) if version <= self.runs => {
                self.version = self.version.max(version);
                self.format = self.version.format().unwrap_or(self.format);
            }
            // A later binary raised the log past what this one runs: skip the raise and
            // go on at the version and format this machine has.
            Ok(LogCommand::MachineVersion(_)) => self.skip(entry.id.index, "MachineVersion"),
            // This log applies only metadata commands, its `Bootstrap` and the raises of
            // its version, so a later binary wrote a control command.
            Ok(LogCommand::Farm(_)) => self.skip(entry.id.index, "Farm"),
            Ok(LogCommand::Report(_)) => self.skip(entry.id.index, "Report"),
            Ok(LogCommand::Takeover(_)) => self.skip(entry.id.index, "Takeover"),
            Ok(LogCommand::Checkpoint(_)) => self.skip(entry.id.index, "Checkpoint"),
            Ok(LogCommand::Unknown) => self.skip(entry.id.index, "Unknown"),
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
