//! `--meta-log=raft`: the metadata log as a single-voter Raft log on the local disk
//! (docs/design/ha.md, section 16, PR 5).
//!
//! [`RaftMetaLog`] implements the front's [`MetaLog`] over a [`kbf_raft::Host`] whose
//! storage is a [`kbf_store::Store`] in the raft directory, whose transport sends
//! nothing (a single voter has no peer), and whose state machine applies each
//! committed `kbf-log` entry to a [`MetaState`]:
//!
//! - **Opening.** An empty directory gets a new log: its id is drawn, and recorded
//!   with the voter set ([`RECORD`]) before the store writes anything. Every later
//!   start reads the id from the record. The server then elects itself, which commits
//!   its term's blank entry and so applies every entry already in the log, once each.
//!   If no `Bootstrap` has been applied, it commits one naming the log id and the
//!   object key prefix `<prefix root><log id>/`. A start whose record or prefix root
//!   disagrees with the committed `Bootstrap` is refused.
//! - **Commit.** A command is encoded, proposed, made durable (the store syncs before
//!   every persist returns), applied, and only then answered. The proposal and its
//!   sync run on the calling thread under the log's lock, as the single-process log
//!   applies under its lock: one commit at a time, no group commit yet.
//! - **Queries** read this server's applied state. A single voter is always its own
//!   leader, so that is the committed state.
//! - **Leadership** ([`MetaLog::leadership`]): the log id and the term the open
//!   elected this server in. With no peer, nothing can take the leadership away, so
//!   the term is the same for the life of the process, and every start's is greater
//!   than every earlier start's. The farm grants its leases under this term and names
//!   the log id as its lease epoch in `Welcome` ([`crate::farm`]).
//! - **Farm time** continues from the committed one: at open the log takes the
//!   applied farm time as its base, and [`RaftMetaLog::now`] is the base plus the time
//!   since the open, so it never goes back across restarts. A task commits
//!   `Tick(now)` every [`TICK`] while the log is open. A tick is a proposal, which
//!   only a leader may make.
//! - **Machine version.** A new log is at [`MachineVersion::V1`], whose entries are
//!   format 1. [`RaftMetaLog::raise_machine_version`] commits a `MachineVersion`
//!   entry; every entry after it is in the new version's format, so a log raised to
//!   [`MachineVersion::V2`] takes the write tier's commands. A start replays the raise
//!   with the log. A binary that does not run the version (any older one) refuses the
//!   log at that entry.
//! - **A storage error** stops the log for the life of the process: every later
//!   commit is refused `Unavailable`.
//!
//! Not yet: snapshots (the metadata state has no snapshot encoding, so the log is
//! never compacted, and a directory holding a snapshot is refused), more than one
//! voter, and the scheduler's state, which stays in memory.

mod machine;
mod recorded;

use std::fmt::{self, Display};
use std::io::{self, Read};
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use kbf_front::{Leadership, MetaLog, MetaLogError};
use kbf_log::{Bootstrap, DecodeError, EncodeError, FarmLogId, LogCommand, MachineVersion};
use kbf_meta::{Applied, Command, MetaState, Retention};
use kbf_objstore::{KeyError, KeyPrefix};
use kbf_raft::{
    Config, ConfigError, Host, HostError, Membership, Message, OpenError, Role, ServerId, Transport,
};
use kbf_store::{Fs, Options, StdFs, Store, StoreError};
use kbf_types::FarmTime;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use self::machine::MetaMachine;
pub use self::machine::NoSnapshots;
pub use self::recorded::{RECORD, RECORD_TMP};

/// The highest machine version this binary runs: [`MachineVersion::V2`], the write
/// tier's commands. A log stays at the version it was raised to; a new one starts at
/// [`MachineVersion::V1`].
pub const RUNS: MachineVersion = MachineVersion::V2;

/// How often the log commits farm time.
pub const TICK: Duration = Duration::from_secs(1);

/// The only server of a single-voter log.
const ME: ServerId = ServerId(1);

/// The transport of a single voter: it has no peer, so the core never sends.
#[derive(Debug)]
struct NoTransport;

impl Transport for NoTransport {
    fn send(&mut self, _: ServerId, _: Message) {}
}

type RaftHost<F> = Host<Store<F>, NoTransport, MetaMachine>;

/// Why the log could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum RaftLogError {
    /// A filesystem call on the raft directory failed.
    #[error("raft dir: {0}")]
    Io(#[from] io::Error),
    /// The log record is not one this binary writes.
    #[error("raft dir: {RECORD} is not a log record this kbf-server reads")]
    Record,
    /// The directory holds files but no log record: not a log this server created.
    #[error("raft dir: holds files but no {RECORD}")]
    NotEmpty,
    /// The object key prefix is not a valid prefix.
    #[error("the object key prefix: {0}")]
    Prefix(#[from] KeyError),
    /// The store refused to open.
    #[error("raft dir: {0}")]
    Store(#[from] StoreError),
    /// The host refused what the store holds.
    #[error("raft dir: {0}")]
    Open(#[from] OpenError<StoreError, NoSnapshots>),
    /// The single voter's membership was refused.
    #[error("raft membership: {0}")]
    Membership(#[from] ConfigError),
    /// The host stopped while the log opened.
    #[error("raft log: {0}")]
    Host(#[from] HostError<StoreError>),
    /// An entry could not be encoded.
    #[error("raft log: {0}")]
    Encode(#[from] EncodeError),
    /// A committed entry does not decode; the log applies nothing past it.
    #[error("raft log: entry {index}: {source}")]
    Decode {
        /// The entry's index.
        index: u64,
        /// Why it does not decode.
        source: DecodeError,
    },
    /// The machine version this binary was asked to run is not one it knows.
    #[error("raft log: this kbf-server does not run machine version {}", .0.get())]
    Runs(MachineVersion),
    /// The committed `Bootstrap` is not the one this start expects: the record names
    /// another log, or the prefix root changed since the log was created.
    #[error(
        "raft log: committed bootstrap {committed:?}, but this start expects {expected:?}; \
         the log record or the object key prefix changed"
    )]
    Mismatch {
        /// The `Bootstrap` the log applied.
        committed: Option<Bootstrap>,
        /// The one this start would have committed.
        expected: Bootstrap,
    },
}

/// The metadata log as a single-voter Raft log in one directory (see the module
/// docs). Dropping it stops its tick task.
pub struct RaftMetaLog<F: Fs = StdFs> {
    shared: Arc<Shared<F>>,
    ticker: AbortHandle,
}

struct Shared<F: Fs> {
    host: Mutex<RaftHost<F>>,
    log_id: FarmLogId,
    /// The term the open elected this server in.
    term: u64,
    prefix: KeyPrefix,
    /// The farm time applied when the log opened.
    base: FarmTime,
    opened: Instant,
    /// The `Bootstrap` entries applied by the time the log opened.
    bootstraps: usize,
}

impl RaftMetaLog<StdFs> {
    /// Opens the log in `dir` (created if missing), creating a new one with a random
    /// id if the directory is empty. Objects go under `<prefix_root><log id>/`.
    ///
    /// # Errors
    /// As [`RaftMetaLog::open`].
    pub async fn open_dir(
        dir: &Path,
        retention: Retention,
        prefix_root: &str,
    ) -> Result<Self, RaftLogError> {
        Self::open(StdFs::new(dir)?, retention, prefix_root, random_log_id).await
    }
}

/// Eight bytes from the system's random source; a zero draw becomes 1.
fn random_log_id() -> io::Result<FarmLogId> {
    let mut bytes = [0; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let n = NonZeroU64::new(u64::from_le_bytes(bytes)).unwrap_or(NonZeroU64::MIN);
    Ok(FarmLogId::new(n))
}

impl<F> RaftMetaLog<F>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    /// Opens the log in `fs`, as the module docs describe. `new_log_id` is called
    /// only when `fs` is empty, to draw the id of the log created there. Reading the
    /// log runs on the calling thread; it is called once, at start.
    ///
    /// # Errors
    /// [`RaftLogError`]: the directory, its record or its store is refused, a
    /// committed entry does not decode, or the committed `Bootstrap` is not this
    /// start's.
    pub async fn open(
        fs: F,
        retention: Retention,
        prefix_root: &str,
        new_log_id: impl FnOnce() -> io::Result<FarmLogId>,
    ) -> Result<Self, RaftLogError> {
        Self::open_running(fs, retention, prefix_root, new_log_id, RUNS).await
    }

    /// [`RaftMetaLog::open`], for a binary that runs machine versions up to `runs`: a
    /// log raised past it is refused at the raise (`RaftLogError::Decode`), as a
    /// rollback to an older binary is. The server runs [`RUNS`]; a lower `runs` stands
    /// for an older binary in tests of that rollback.
    ///
    /// # Errors
    /// As [`RaftMetaLog::open`], and [`RaftLogError::Runs`] for a version this binary
    /// does not know.
    pub async fn open_running(
        fs: F,
        retention: Retention,
        prefix_root: &str,
        new_log_id: impl FnOnce() -> io::Result<FarmLogId>,
        runs: MachineVersion,
    ) -> Result<Self, RaftLogError> {
        let machine = MetaMachine::new(retention, runs).ok_or(RaftLogError::Runs(runs))?;
        let log_id = match recorded::read(&fs)? {
            Some(id) => id,
            None => {
                let id = new_log_id()?;
                recorded::write(&fs, id)?;
                id
            }
        };
        let prefix = KeyPrefix::new(format!("{prefix_root}{log_id}/"))?;
        let (store, _) = Store::open(fs, Options::default())?;
        let config = Config {
            id: ME,
            membership: Membership::new([ME], [])?,
            election_ticks: 2,
            heartbeat_ticks: 1,
            max_entries_per_append: 1,
        };
        let mut host = Host::open(config, store, NoTransport, machine, 0)?;
        // A single voter elects itself within two election timeouts; winning commits
        // the term's blank entry, which applies every entry before it.
        while host.core().role() != Role::Leader {
            host.tick(0)?;
        }
        decoded(host.machine())?;
        let expected = Bootstrap {
            log_id,
            store_prefix: prefix.as_str().to_owned(),
        };
        if host.machine().bootstrap().is_none() {
            let format = host.machine().format();
            let entry = kbf_log::encode(format, &LogCommand::Bootstrap(expected.clone()))?;
            host.propose(entry)?;
        }
        let committed = host.machine().bootstrap().cloned();
        if committed.as_ref() != Some(&expected) {
            return Err(RaftLogError::Mismatch {
                committed,
                expected,
            });
        }
        let shared = Arc::new(Shared {
            base: host.machine().meta().now(),
            bootstraps: host.machine().bootstraps(),
            term: host.core().hard_state().term.0,
            host: Mutex::new(host),
            log_id,
            prefix,
            opened: Instant::now(),
        });
        let ticker = tokio::spawn(tick_every(Arc::downgrade(&shared))).abort_handle();
        Ok(Self { shared, ticker })
    }

    /// Raises the log's machine version to `to` by committing a `MachineVersion`
    /// entry, unless the log is there already (a version is never lowered). Every
    /// entry after it is encoded and decoded in `to`'s format: raising to
    /// [`MachineVersion::V2`] lets the write tier's commands commit
    /// (docs/design/metadata-snapshots.md, section 4). It is one-way: a binary that
    /// runs only an earlier version refuses the log from then on.
    ///
    /// # Errors
    /// [`MetaLogError::Unavailable`] if `to` is past what this binary runs, or the
    /// log has stopped.
    pub fn raise_machine_version(&self, to: MachineVersion) -> Result<(), MetaLogError> {
        let mut host = self.shared.lock();
        let machine = host.machine();
        if to > machine.runs() {
            let runs = machine.runs().get();
            let d = format!(
                "machine version {} is past {runs}, the one this binary runs",
                to.get()
            );
            return Err(MetaLogError::Unavailable(d));
        }
        if machine.version() >= to {
            return Ok(());
        }
        let entry = kbf_log::encode(machine.format(), &LogCommand::MachineVersion(to))
            .map_err(unavailable)?;
        host.propose(entry).map_err(unavailable)?;
        if host.machine().version() == to {
            Ok(())
        } else {
            Err(MetaLogError::Unavailable(NOT_APPLIED.to_owned()))
        }
    }

    /// Commits `Tick(now)` and returns the committed farm time after it.
    ///
    /// # Errors
    /// [`MetaLogError::Unavailable`] if the log has stopped.
    pub fn tick(&self) -> Result<FarmTime, MetaLogError> {
        self.shared.tick()
    }
}

impl<F: Fs> RaftMetaLog<F> {
    /// The log's id, from its record.
    #[must_use]
    pub fn log_id(&self) -> FarmLogId {
        self.shared.log_id
    }

    /// The key prefix of every object, as the log's `Bootstrap` committed it.
    #[must_use]
    pub fn store_prefix(&self) -> KeyPrefix {
        self.shared.prefix.clone()
    }

    /// Farm time now: the farm time applied when the log opened, plus the time since.
    #[must_use]
    pub fn now(&self) -> FarmTime {
        self.shared.now()
    }

    /// The index of the last entry the metadata state has applied (blank entries
    /// count). Every entry through it was durable in the raft directory before it was
    /// applied.
    #[must_use]
    pub fn applied_index(&self) -> u64 {
        self.shared.lock().machine().applied().0
    }

    /// The machine version the log has committed: [`MachineVersion::V1`] until
    /// [`RaftMetaLog::raise_machine_version`] raises it.
    #[must_use]
    pub fn machine_version(&self) -> MachineVersion {
        self.shared.lock().machine().version()
    }

    /// How many `Bootstrap` entries the log held when it opened, the one it may have
    /// just committed included. One for every log this version writes.
    #[must_use]
    pub fn bootstraps(&self) -> usize {
        self.shared.bootstraps
    }
}

impl<F: Fs> fmt::Debug for RaftMetaLog<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RaftMetaLog")
            .field("log_id", &self.shared.log_id)
            .field("prefix", &self.shared.prefix)
            .finish_non_exhaustive()
    }
}

impl<F: Fs> Drop for RaftMetaLog<F> {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}

impl<F> MetaLog for RaftMetaLog<F>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.shared.execute(command)
    }

    async fn query<R, Q>(&self, f: Q) -> Result<R, MetaLogError>
    where
        Q: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        Ok(f(self.shared.lock().machine().meta()))
    }

    fn leadership(&self) -> Option<Leadership> {
        Some(Leadership {
            log_id: self.shared.log_id.non_zero(),
            term: self.shared.term,
        })
    }
}

impl<F: Fs> Shared<F> {
    fn lock(&self) -> MutexGuard<'_, RaftHost<F>> {
        // The host stops itself on a storage error rather than panicking midway, so a
        // poisoned lock still guards a whole host.
        self.host.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> FarmTime {
        self.base.saturating_add(self.opened.elapsed())
    }

    fn execute(&self, command: Command) -> Result<Applied, MetaLogError> {
        let mut host = self.lock();
        let format = host.machine().format();
        let entry = kbf_log::encode(format, &LogCommand::Meta(command)).map_err(unavailable)?;
        let index = host.propose(entry).map_err(unavailable)?;
        // A single voter commits and applies its own proposal before `propose` returns.
        host.machine()
            .answer(index)
            .ok_or(MetaLogError::Unavailable(NOT_APPLIED.to_owned()))
    }

    fn tick(&self) -> Result<FarmTime, MetaLogError> {
        self.execute(Command::Tick(self.now()))?;
        Ok(self.lock().machine().meta().now())
    }
}

const NOT_APPLIED: &str = "the metadata log did not apply its own proposal";

/// A log error as the front sees it: the leader could not answer.
fn unavailable(e: impl Display) -> MetaLogError {
    MetaLogError::Unavailable(format!("the metadata log: {e}"))
}

/// The machine's decode failure, if an entry stopped it.
fn decoded(machine: &MetaMachine) -> Result<(), RaftLogError> {
    machine.failed().map_or(Ok(()), |(index, e)| {
        Err(RaftLogError::Decode {
            index: index.0,
            source: e.clone(),
        })
    })
}

/// Commits farm time every [`TICK`] for as long as the log is open. An error is
/// dropped: it has stopped the log, and the next commit reports it.
async fn tick_every<F: Fs>(shared: Weak<Shared<F>>) {
    let mut every = tokio::time::interval(TICK);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let _ = shared.upgrade().map(|log| log.tick());
    }
}

#[cfg(test)]
mod tests;
