//! The names and the formats of what the proxy and the reconciler keep in the hot bucket.
//!
//! - `_journal/<proxy_id>/<seq>`: the journal of one proxy, a small object for each batch of
//!   entries, written create-only. `seq` is 20 digits, so that the listing order is the
//!   numeric order. The numbers of one proxy start at 1, and a number is skipped only when an
//!   object holds it, so the objects that exist have no gaps above the checkpoint of the
//!   reconciler; the ones at or below it may have been deleted.
//! - `_tiering/proxies/<proxy_id>`: written when a proxy starts, so that the reconciler finds
//!   the journals without listing the journal prefix of every proxy.
//! - `_tiering/checkpoint/<proxy_id>`: a [`Checkpoint`], rewritten by the reconciler.
//! - `_tiering/usage`: the usage the reconciler publishes, for the proxies.
//! - `_tiering/lease`: the lease that makes one reconciler the runner.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{Backend, Error};

/// The prefix of the journals.
pub(crate) const JOURNAL_PREFIX: &str = "_journal/";
/// The prefix of everything else the proxy and the reconciler keep.
pub(crate) const TIERING_PREFIX: &str = "_tiering/";
/// The prefix of the objects that say which proxies have journals.
pub(crate) const PROXIES_PREFIX: &str = "_tiering/proxies/";
/// The usage the reconciler publishes.
pub(crate) const USAGE_KEY: &str = "_tiering/usage";
/// The lease of the reconciler.
pub(crate) const LEASE_KEY: &str = "_tiering/lease";

/// Whether a key is under a prefix that the proxy keeps for itself.
pub(crate) fn is_reserved(key: &str) -> bool {
    let reserved = [JOURNAL_PREFIX, TIERING_PREFIX];
    reserved.iter().any(|prefix| key.starts_with(prefix))
}

/// The prefix of the journal objects of one proxy.
pub(crate) fn journal_prefix(proxy_id: &str) -> String {
    format!("{JOURNAL_PREFIX}{proxy_id}/")
}

/// The key of journal object number `seq` of one proxy.
pub(crate) fn journal_key(proxy_id: &str, seq: u64) -> String {
    format!("{JOURNAL_PREFIX}{proxy_id}/{seq:020}")
}

/// The number of the journal object that a listing gave under `prefix`, if the key has one.
pub(crate) fn seq_of(prefix: &str, key: &str) -> Option<u64> {
    key.strip_prefix(prefix).and_then(|rest| rest.parse().ok())
}

/// The key that tells the reconciler about a proxy.
pub(crate) fn proxy_key(proxy_id: &str) -> String {
    format!("{PROXIES_PREFIX}{proxy_id}")
}

/// The key of the checkpoint of one proxy.
pub(crate) fn checkpoint_key(proxy_id: &str) -> String {
    format!("{TIERING_PREFIX}checkpoint/{proxy_id}")
}

/// What a journal entry says happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum JournalOp {
    /// An object was written to the hot tier, and is to be copied to the cold tier.
    Put,
    /// An object was deleted from both tiers; the cold tier is to be told again.
    Delete,
    /// Anything a newer writer adds: a reader skips the entry.
    #[serde(other)]
    Unknown,
}

/// One entry of a proxy's journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// What happened.
    pub op: JournalOp,
    /// The object's key.
    pub key: String,
    /// The object's size in bytes for a put, and zero for a delete.
    pub size: u64,
    /// When it happened, in milliseconds, by the clock of the proxy.
    pub time: u64,
}

fn corrupt(what: &str, error: &serde_json::Error) -> Error {
    Error::Other(format!("corrupt {what}: {error}"))
}

/// The JSON body of a small object.
pub(crate) fn to_body<T: Serialize>(value: &T) -> Bytes {
    let json = serde_json::to_vec(value).expect("values serialize");
    Bytes::from(json)
}

/// The body of a journal object: its entries, in the order they were appended.
#[must_use]
pub fn encode_journal(entries: &[JournalEntry]) -> Bytes {
    to_body(&entries)
}

/// The entries of the body of a journal object.
///
/// # Errors
/// [`Error::Other`] if the body is not the encoding of a list of entries.
pub fn decode_journal(body: &[u8]) -> Result<Vec<JournalEntry>, Error> {
    let parsed = serde_json::from_slice(body);
    parsed.map_err(|error| corrupt("journal object", &error))
}

/// What the reconciler has done for one proxy, as it is stored.
///
/// Every journal object up to `seq` has been applied: its puts are in the cold tier and its
/// deletes are done there. The entries before the position (`evict_seq`, `evict_idx`) in the
/// journal (the object number, and the index of the entry in that object) are done with: a
/// delete, or a put that the cold tier holds and that left the hot tier or was superseded.
/// With the journal objects from `evict_seq` on, it is all the reconciler needs to know about
/// the hot tier after a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    pub(crate) seq: u64,
    pub(crate) evict_seq: u64,
    pub(crate) evict_idx: u64,
}

/// What the reconciler publishes in `_tiering/usage` after each pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Usage {
    /// The bytes of the objects the hot tier holds, by the journals.
    pub(crate) hot_bytes: u64,
    /// The bytes of those that the cold tier does not hold yet.
    pub(crate) copy_lag: u64,
    /// When it was published, by the clock of the reconciler.
    pub(crate) time: u64,
}

/// The checkpoint of a proxy, or the first one if there is none.
pub(crate) async fn load_checkpoint(
    hot: &dyn Backend,
    proxy_id: &str,
) -> Result<Checkpoint, Error> {
    match hot.get(&checkpoint_key(proxy_id), None).await {
        Ok(body) => {
            let parsed = serde_json::from_slice(&body);
            let what = format!("checkpoint of {proxy_id}");
            parsed.map_err(|error| corrupt(&what, &error))
        }
        Err(Error::NotFound(_)) => Ok(Checkpoint::default()),
        Err(error) => Err(error),
    }
}

/// Writes a small object that is rewritten in place: create-only the first time, and then
/// only over the object whose content tag a HEAD just gave.
pub(crate) async fn write_object(hot: &dyn Backend, key: &str, body: Bytes) -> Result<(), Error> {
    match hot.head(key).await {
        Ok(meta) => hot.put_if_match(key, body, &meta.tag).await.map(|_| ()),
        Err(Error::NotFound(_)) => hot.put(key, body).await,
        Err(error) => Err(error),
    }
}
