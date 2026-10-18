//! An order record: what the unprivileged `kbf` leaves in the order spool after the
//! frontend ordered this host to activate, confirm or revert a release.
//!
//! The format is `kbf_update::OrderRecord`, which the writer builds and this side reads, in
//! a file named `<epoch:020>-<order_id:020>.json`. The writer is the service user, so the
//! record is untrusted: the name must be the pair inside, the verb one of three words, the
//! commit a full commit, an activation's claims complete, and no other or repeated key is
//! accepted.

use kbf_update::{Locator, OrderRecord, RecordError, Verb as RecordVerb};

use crate::CommitId;

/// How far above the last acted-on epoch an order's may be. Epochs move only when a store is
/// restored (a handover keeps it), so a jump of more than this is a mistake or an attack;
/// either would put every real order below it. Such an order is refused, not acknowledged.
/// Not applied to the first order a host acts on (it has no epoch to measure from).
pub const MAX_EPOCH_JUMP: u64 = 1000;

/// The biggest record read.
pub const MAX_BYTES: u64 = kbf_update::MAX_RECORD_BYTES;

/// What an order asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Activate,
    Confirm,
    Revert,
    /// The frontend's own update (a record the frontend writes into its own host's spool).
    FrontendUpdate,
}

/// What an activation says the release is and where it is, from the record: claims, not
/// trust.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    pub components: Vec<Locator>,
    pub platform: String,
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    pub profile: String,
}

/// A checked order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub id: u64,
    /// The `Welcome` epoch of the session the node received it in.
    pub epoch: u64,
    /// The generation of the desired release. Carried; the rule on it is the next change's.
    pub generation: u64,
    pub verb: Verb,
    pub commit: CommitId,
    /// The confirm window the frontend asks for; the helper clamps it.
    pub window_s: u32,
    /// For an activation.
    pub claims: Option<Claims>,
}

/// Why a file is not an order.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OrderFault {
    #[error("not an order: {0}")]
    NotAnOrder(RecordError),
    #[error(
        "the file is named for order {file} of epoch {file_epoch} but holds order {inside} of epoch {inside_epoch}"
    )]
    NameIsNotTheOrder {
        file: u64,
        file_epoch: u64,
        inside: u64,
        inside_epoch: u64,
    },
}

/// The (epoch, id) an order file's name stands for.
#[must_use]
pub fn key_of(name: &str) -> Option<(u64, u64)> {
    kbf_update::record::parse_file_name(name)
}

/// The order in `bytes`, which was read from the file named for (`epoch`, `id`).
pub fn parse(epoch: u64, id: u64, bytes: &[u8]) -> Result<Order, OrderFault> {
    let record = OrderRecord::parse(bytes).map_err(OrderFault::NotAnOrder)?;
    if (record.epoch, record.order_id) != (epoch, id) {
        return Err(OrderFault::NameIsNotTheOrder {
            file: id,
            file_epoch: epoch,
            inside: record.order_id,
            inside_epoch: record.epoch,
        });
    }
    // The record checked the commit's shape already; this is the installer's own type.
    let commit = CommitId::new(&record.commit)
        .map_err(|e| OrderFault::NotAnOrder(RecordError::Commit(e.0)))?;
    let verb = match record.verb {
        RecordVerb::Activate => Verb::Activate,
        RecordVerb::Confirm => Verb::Confirm,
        RecordVerb::Revert => Verb::Revert,
        RecordVerb::FrontendUpdate => Verb::FrontendUpdate,
    };
    let claims = record.expected.map(|f| Claims {
        components: record.components,
        platform: f.platform,
        protocol: f.protocol,
        min_protocol: f.min_protocol,
        state_schema: f.state_schema,
        profile: f.profile,
    });
    Ok(Order {
        id,
        epoch,
        generation: record.generation,
        verb,
        commit,
        window_s: record.window_s,
        claims,
    })
}
