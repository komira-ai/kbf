//! The one order spool of the proxy side: every record the proxy writes into `orders/`
//! (a `frontend_update`, a `confirm`, a `revert`) goes through an [`OrderSpool`], which
//! owns the counter that numbers them. The update API writes through its
//! [`OrderWriter`] (`&mut self`); the swap writes through the same object's `&self`
//! methods ([`OrderSpool::frontend_update`], [`confirm`](OrderSpool::confirm),
//! [`revert`](OrderSpool::revert)), which have the shape of the write half of
//! [`RootOrders`](super::ports::RootOrders). That trait also reads `status.json`, so it is
//! implemented by an adapter that holds a clone of the spool for its writes. A clone shares
//! the counter and the lock.
//!
//! # The id
//!
//! `id = max(wall clock in ms, high_water + 1)`, written as `order_id`; the epoch is
//! [`EPOCH`]; `generation` is the id again (the generation the root half carries is a store
//! fact the proxy has no way to name, and it is carried, not applied). The root half orders
//! records by (epoch, id) and drops one at or below the last it acted on, silently, so the
//! only rule that matters is that ids only grow. The clock keeps them growing across a
//! lost counter; the high-water mark keeps them growing across a clock that stepped back.
//!
//! # Durability and the gap
//!
//! The counter, `<state dir>/orders.json` (`{epoch, high_water}`), is replaced by a temp
//! file, an fsync, a rename and an fsync of the directory, and only then is the record
//! written (by [`kbf_update::spool::Spool`]: temp file, fsync, rename, fsync of the
//! directory). A crash between the two leaves the counter ahead of `orders/`: **a gap**,
//! an id that was reserved and never used. A gap is harmless, because ids only need to
//! grow; the other order would not be: a record visible under an id the counter has not
//! reserved is an id the next start hands out again. When the counter cannot be made
//! durable no record is written. When `orders/` is full or cannot be listed nothing is
//! reserved either, so the counter stays where it was; when listing succeeds and the
//! record then cannot be written, the id stays reserved (a gap).
//!
//! Allocating, syncing the counter and writing the record happen under one lock, because
//! the update API's actor and the swap's thread write at the same time.
//!
//! # Limits and assumptions
//!
//! - The root half reads at most [`MAX_RECORDS`] records and deletes the rest unread,
//!   newest first, so a write is refused once [`MAX_PENDING`] are waiting in `orders/`.
//! - [`EPOCH`] is fixed; see there for what that assumes.
//! - A lost `orders.json` (the file is simply not there) starts again from the clock: the
//!   ids stay above the old ones unless the clock is also behind the highest id issued.
//!   One that cannot be read or parsed, or is for another epoch, is refused at open: the
//!   id it held is unknown, so the spool guesses nothing.
//! - One spool per `orders/` directory in the whole system. Opening removes the
//!   `.partial-` files of an earlier run, so a second spool, in this process or another,
//!   would delete the file the first is writing.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use kbf_update::record::parse_file_name;
use kbf_update::spool::Spool;
use kbf_update::{MAX_RECORDS, OrderRecord, Verb as RecordVerb};
use serde::{Deserialize, Serialize};

use super::api::traits::{OrderError, OrderWriter};
use super::model::{Locator, PortError, Verb};
use super::record::{Durable, Fsync};
use crate::CommitId;

/// The epoch of every record this spool writes.
///
/// The root half refuses a record whose epoch is below the last it acted on for that kind
/// of order, and a store restore is what raises an epoch; the proxy has no store epoch to
/// read. So the epoch is one fixed number, which assumes that no node activation order
/// reaches this host's spool (a frontend or all-in-one host enrolls no node of its own):
/// the proxy is the only writer, and one number orders everything it writes. Where the
/// root half already acted on a higher epoch it refuses every record of this spool as
/// older, and nothing in `status.json` names the refused order, so the update API sees an
/// update that never stages.
pub const EPOCH: u64 = 1;

/// Records waiting in `orders/` at which a write is refused: three quarters of what the
/// root half reads. Reaching it means the root half is not taking them, and the records
/// past [`MAX_RECORDS`] would be deleted unread.
pub const MAX_PENDING: usize = MAX_RECORDS * 3 / 4;

/// The counter's file, in the state directory.
const COUNTER: &str = "orders.json";

/// Its temp file, replaced into [`COUNTER`].
const COUNTER_TEMP: &str = ".orders.json.tmp";

/// The confirm window of a record that has none: the root half reads `window_s` only to arm
/// an activation.
const NO_WINDOW: u32 = 0;

type WallMs = Box<dyn Fn() -> u64 + Send + Sync>;

/// Why a record was not written, or the spool not opened.
#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    #[error("orders/ cannot be opened or listed: {0}")]
    Orders(io::Error),
    #[error(
        "orders/ holds {held} records, at most {MAX_PENDING} are written: the root half is not taking them"
    )]
    Full { held: usize },
    #[error("the order counter cannot be read or made durable: {0}")]
    Counter(io::Error),
    #[error("orders.json is not a counter ({0}); remove it to number again from the clock")]
    CounterCorrupt(serde_json::Error),
    #[error("orders.json is for epoch {found}, this build writes epoch {EPOCH}")]
    CounterEpoch { found: u64 },
    #[error("the order counter is at the largest id")]
    Exhausted,
    #[error("the record cannot be written to orders/: {0}")]
    Write(io::Error),
    #[error("the proxy never writes an activate order")]
    NoActivate,
    #[error("that is not a frontend_update locator this spool issued")]
    NotIssued,
}

impl From<SpoolError> for OrderError {
    fn from(e: SpoolError) -> OrderError {
        OrderError(e.to_string())
    }
}

impl From<SpoolError> for PortError {
    fn from(e: SpoolError) -> PortError {
        PortError(e.to_string())
    }
}

/// `orders.json`. An unknown key is an error: a counter another build wrote is not read
/// as ours.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Counter {
    epoch: u64,
    high_water: u64,
}

struct Inner {
    spool: Spool,
    state_dir: PathBuf,
    wall_ms: WallMs,
    sync: Box<dyn Durable + Send + Sync>,
    /// The highest id reserved: durable in `orders.json` before it is used. Held for the
    /// whole of a write.
    high_water: Mutex<u64>,
}

/// Writes the proxy's orders into `orders/`; see the module docs. Cloning shares the
/// counter and the lock.
#[derive(Clone)]
pub struct OrderSpool {
    inner: Arc<Inner>,
}

impl OrderSpool {
    /// The spool over `orders` (made, mode 0700, if missing; the `.partial-` files of a
    /// crash removed), numbered by `<state_dir>/orders.json`. A counter that is not there
    /// is written (high-water mark 0), so a state directory that cannot be written fails
    /// here and not at the first update.
    ///
    /// # Errors
    /// `orders/` cannot be opened, or the counter is unreadable, corrupt, for another
    /// epoch, or cannot be written.
    pub fn open(orders: &Path, state_dir: &Path) -> Result<OrderSpool, SpoolError> {
        OrderSpool::open_with(orders, state_dir, Box::new(system_wall_ms), Box::new(Fsync))
    }

    /// [`open`](Self::open) with the clock and the syncs a test controls.
    pub(super) fn open_with(
        orders: &Path,
        state_dir: &Path,
        wall_ms: WallMs,
        sync: Box<dyn Durable + Send + Sync>,
    ) -> Result<OrderSpool, SpoolError> {
        let spool = Spool::open(orders).map_err(SpoolError::Orders)?;
        let loaded = load(&state_dir.join(COUNTER))?;
        let inner = Inner {
            spool,
            state_dir: state_dir.to_owned(),
            wall_ms,
            sync,
            high_water: Mutex::new(loaded.unwrap_or(0)),
        };
        if loaded.is_none() {
            inner.persist(0).map_err(SpoolError::Counter)?;
        }
        Ok(OrderSpool {
            inner: Arc::new(inner),
        })
    }

    /// Writes a record for `verb` and returns its locator.
    fn issue(&self, verb: Verb, commit: &CommitId, window_s: u32) -> Result<Locator, SpoolError> {
        let record_verb = match verb {
            Verb::Activate => return Err(SpoolError::NoActivate),
            Verb::Confirm => RecordVerb::Confirm,
            Verb::Revert => RecordVerb::Revert,
            Verb::FrontendUpdate => RecordVerb::FrontendUpdate,
        };
        let inner = &self.inner;
        let mut high_water = inner
            .high_water
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        inner.make_room()?;
        let next = high_water.checked_add(1).ok_or(SpoolError::Exhausted)?;
        let id = (inner.wall_ms)().max(next);
        inner.persist(id).map_err(SpoolError::Counter)?;
        *high_water = id;
        let locator = Locator {
            order_id: id,
            epoch: EPOCH,
            generation: id,
            verb,
            commit: commit.clone(),
            window_s,
        };
        inner
            .spool
            .write(&record(&locator, record_verb))
            .map_err(SpoolError::Write)?;
        Ok(locator)
    }

    /// Makes sure the record of a `frontend_update` locator this spool issued is in
    /// `orders/`: writes it if it is not there, and does nothing if it is. The shape of
    /// [`RootOrders::frontend_update`](super::ports::RootOrders::frontend_update).
    ///
    /// # Errors
    /// The locator is not a `frontend_update` of this spool's epoch with an id it issued,
    /// or `orders/` is full or cannot be written.
    pub fn frontend_update(&self, locator: &Locator) -> Result<(), PortError> {
        Ok(self.reissue(locator)?)
    }

    fn reissue(&self, locator: &Locator) -> Result<(), SpoolError> {
        let inner = &self.inner;
        let high_water = inner
            .high_water
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if locator.verb != Verb::FrontendUpdate
            || locator.epoch != EPOCH
            || locator.order_id > *high_water
        {
            return Err(SpoolError::NotIssued);
        }
        if inner.spool.has(locator.epoch, locator.order_id) {
            return Ok(());
        }
        inner.make_room()?;
        inner
            .spool
            .write(&record(locator, RecordVerb::FrontendUpdate))
            .map_err(SpoolError::Write)
    }

    /// Writes a `confirm` for `commit`. The shape of
    /// [`RootOrders::confirm`](super::ports::RootOrders::confirm).
    ///
    /// # Errors
    /// `orders/` is full or cannot be written, or the counter cannot be made durable.
    pub fn confirm(&self, commit: &CommitId) -> Result<(), PortError> {
        self.issue(Verb::Confirm, commit, NO_WINDOW)?;
        Ok(())
    }

    /// Writes a `revert` for `commit`. The shape of
    /// [`RootOrders::revert`](super::ports::RootOrders::revert).
    ///
    /// # Errors
    /// As [`confirm`](Self::confirm).
    pub fn revert(&self, commit: &CommitId) -> Result<(), PortError> {
        self.issue(Verb::Revert, commit, NO_WINDOW)?;
        Ok(())
    }
}

impl OrderWriter for OrderSpool {
    /// A `frontend_update`, a `confirm` or a `revert`. An `activate` is refused: it would be
    /// a record without components, which the root half never takes.
    fn write(
        &mut self,
        verb: Verb,
        commit: &CommitId,
        window_s: u32,
    ) -> Result<Locator, OrderError> {
        Ok(self.issue(verb, commit, window_s)?)
    }
}

impl Inner {
    /// Refuses when `orders/` already holds [`MAX_PENDING`] records. Reserves nothing.
    fn make_room(&self) -> Result<(), SpoolError> {
        let mut held = 0;
        for entry in fs::read_dir(self.spool.path()).map_err(SpoolError::Orders)? {
            let name = entry.map_err(SpoolError::Orders)?.file_name();
            held += usize::from(name.to_str().and_then(parse_file_name).is_some());
        }
        if held >= MAX_PENDING {
            return Err(SpoolError::Full { held });
        }
        Ok(())
    }

    /// Makes `high_water` the durable counter: file data, rename, directory entry.
    fn persist(&self, high_water: u64) -> io::Result<()> {
        let bytes = serde_json::to_vec(&Counter {
            epoch: EPOCH,
            high_water,
        })
        .map_err(io::Error::other)?;
        let temp = self.state_dir.join(COUNTER_TEMP);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        self.sync.file(&file)?;
        fs::rename(&temp, self.state_dir.join(COUNTER))?;
        self.sync.dir(&self.state_dir)
    }
}

/// The record of `locator`: the six keys of every record and nothing else (no components,
/// no facts), as the root half's `frontend_update`, `confirm` and `revert` are.
fn record(locator: &Locator, verb: RecordVerb) -> OrderRecord {
    OrderRecord {
        order_id: locator.order_id,
        epoch: locator.epoch,
        generation: locator.generation,
        verb,
        commit: locator.commit.as_str().to_owned(),
        window_s: locator.window_s,
        components: Vec::new(),
        expected: None,
    }
}

/// The high-water mark `orders.json` holds; `None` when there is no file.
fn load(path: &Path) -> Result<Option<u64>, SpoolError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SpoolError::Counter(e)),
    };
    let counter: Counter = serde_json::from_slice(&bytes).map_err(SpoolError::CounterCorrupt)?;
    if counter.epoch != EPOCH {
        return Err(SpoolError::CounterEpoch {
            found: counter.epoch,
        });
    }
    Ok(Some(counter.high_water))
}

/// Milliseconds since the Unix epoch; 0 for a clock before it.
fn system_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
