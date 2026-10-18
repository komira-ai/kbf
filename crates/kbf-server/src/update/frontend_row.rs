//! The `update/frontend` row: the frontend's own update (`POST /v1/update/frontend`), at most
//! one in flight.
//!
//! The row is `{order_id, epoch, generation, commit, state, written_at}`. It is written
//! **before** the order record the installer reads, so a crash between the two leaves an
//! open row and no record: the route's deadline closes it (the row never has a time to live
//! of its own, like every `update/*` row). It is never deleted: closed, it keeps the last
//! generation, which the next update draws from (`generation + 1`), so an older commit offered
//! again, a rollback, outranks the update it undoes. Every write is a compare-and-set on the
//! version read, so two requests that race for the one place are not both given it.
//!
//! Order ids are drawn from the same counter as the nodes' ([`Rows::mint_order`]); the
//! installer keeps a separate floor for each kind of order, so the two sequences do not
//! disturb each other.
//!
//! The bytes are a `prost` message like the other rows' (`codec`): an unknown field is
//! skipped, a field added later must be valid at zero, and the state is numbered from 1 so a
//! zero is corrupt rather than read as the first state.

use kbf_state::Txn;
use prost::Message as _;

use super::codec::Corrupt;
use super::facts::{OrderId, Release};
use super::rows::{ATTEMPTS, RowError, Rows, Stored, read};

const FRONTEND: &str = "update/frontend";

/// Whether the frontend's update is in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontendState {
    /// An order was drawn and may be in the spool; nothing says the installer is done.
    Open,
    /// The installer finished with it, or the route gave up on it.
    Closed,
}

impl FrontendState {
    /// The word the operator API shows.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// `update/frontend`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendRow {
    /// The order's id and the epoch it was drawn under.
    pub order: OrderId,
    /// The generation of this update: one above the last row's, and 1 for the first.
    pub generation: u64,
    /// The release the frontend was ordered to.
    pub commit: Release,
    /// Whether the update is in flight.
    pub state: FrontendState,
    /// Wall-clock milliseconds since the Unix epoch when the row was opened: the frontend
    /// restarts as part of its own update, so a clock of the process would not survive it.
    pub written_at_ms: u64,
}

/// What [`Rows::begin_frontend`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Began {
    /// The row is open under the new order: the record may be written.
    Opened(FrontendRow),
    /// An update is already in flight (the row as it is): nothing was written.
    Busy(FrontendRow),
}

#[derive(Clone, PartialEq, prost::Message)]
struct FrontendMsg {
    #[prost(uint64, tag = "1")]
    order_id: u64,
    #[prost(uint64, tag = "2")]
    epoch: u64,
    #[prost(uint64, tag = "3")]
    generation: u64,
    #[prost(string, tag = "4")]
    commit: String,
    /// 1 open, 2 closed.
    #[prost(uint32, tag = "5")]
    state: u32,
    #[prost(uint64, tag = "6")]
    written_at_ms: u64,
}

fn encode(row: &FrontendRow) -> Vec<u8> {
    FrontendMsg {
        order_id: row.order.seq,
        epoch: row.order.epoch,
        generation: row.generation,
        commit: row.commit.as_str().to_owned(),
        state: match row.state {
            FrontendState::Open => 1,
            FrontendState::Closed => 2,
        },
        written_at_ms: row.written_at_ms,
    }
    .encode_to_vec()
}

fn decode(bytes: &[u8]) -> Result<FrontendRow, Corrupt> {
    let msg = FrontendMsg::decode(bytes).map_err(|e| Corrupt(e.to_string()))?;
    let state = match msg.state {
        1 => FrontendState::Open,
        2 => FrontendState::Closed,
        other => return Err(Corrupt(format!("the frontend update has state {other}"))),
    };
    if msg.commit.is_empty() || msg.generation == 0 {
        return Err(Corrupt(
            "the frontend update has no commit or no generation".to_owned(),
        ));
    }
    Ok(FrontendRow {
        order: OrderId::new(msg.epoch, msg.order_id),
        generation: msg.generation,
        commit: Release::new(msg.commit),
        state,
        written_at_ms: msg.written_at_ms,
    })
}

impl Rows {
    /// The frontend's update row, if one was ever begun.
    ///
    /// # Errors
    /// [`RowError`]: the store failed or the row cannot be read.
    pub fn frontend(&self) -> Result<Stored<Option<FrontendRow>>, RowError> {
        match self.store.get(FRONTEND)? {
            None => Ok(Stored {
                value: None,
                version: None,
            }),
            Some(row) => Ok(Stored {
                value: Some(read(FRONTEND, &row, decode)?),
                version: Some(row.version),
            }),
        }
    }

    /// Opens the row for an update to `commit`: a new order id of `epoch` and the generation
    /// above the last row's, written by compare-and-set on the version read. Busy, with
    /// nothing written, when the row is open. An id drawn for a write that then lost the race
    /// is skipped, never reused.
    ///
    /// # Errors
    /// [`RowError`]: the store failed, the row cannot be read, or writers kept racing.
    pub fn begin_frontend(
        &self,
        commit: &Release,
        epoch: u64,
        now_ms: u64,
    ) -> Result<Began, RowError> {
        for _ in 0..ATTEMPTS {
            let current = self.frontend()?;
            if let Some(open) = current
                .value
                .as_ref()
                .filter(|r| r.state == FrontendState::Open)
            {
                return Ok(Began::Busy(open.clone()));
            }
            let next = FrontendRow {
                order: self.mint_order(epoch)?,
                generation: current
                    .value
                    .as_ref()
                    .map_or(1, |r| r.generation.saturating_add(1)),
                commit: commit.clone(),
                state: FrontendState::Open,
                written_at_ms: now_ms,
            };
            let txn = Txn::new().put(FRONTEND, encode(&next), current.cond());
            if self.store.txn(txn)?.is_ok() {
                return Ok(Began::Opened(next));
            }
        }
        Err(RowError::Contended(FRONTEND))
    }

    /// Closes the row if it is open for `order`; a row that is closed, or open for another
    /// order, is left as it is. Returns whether this call closed it.
    ///
    /// # Errors
    /// [`RowError`]: the store failed, the row cannot be read, or writers kept racing.
    pub fn end_frontend(&self, order: OrderId) -> Result<bool, RowError> {
        for _ in 0..ATTEMPTS {
            let current = self.frontend()?;
            let Some(open) = current
                .value
                .as_ref()
                .filter(|r| r.state == FrontendState::Open && r.order == order)
            else {
                return Ok(false);
            };
            let closed = FrontendRow {
                state: FrontendState::Closed,
                ..open.clone()
            };
            let txn = Txn::new().put(FRONTEND, encode(&closed), current.cond());
            if self.store.txn(txn)?.is_ok() {
                return Ok(true);
            }
        }
        Err(RowError::Contended(FRONTEND))
    }
}
