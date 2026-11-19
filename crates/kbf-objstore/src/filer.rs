//! Which objects a SeaweedFS filer holds, and how long each is, asked many at a time:
//! [`FilerPresence`], a client of the filer's gRPC `LookupDirectoryEntries` (SeaweedFS
//! 4.54, `weed/pb/filer.proto`), behind the trait [`PresenceCheck`].
//!
//! - **One call, at most [`MAX_NAMES_PER_CALL`] names.** The filer refuses an empty call and
//!   one over 4,096 names. A check of more keys is split into calls of at most that many,
//!   sent at once on the one connection, and answered in the order the keys were given.
//! - **A key is a filer path under the root.** The S3 gateway keeps the objects of a bucket
//!   under its directory, `/buckets/<bucket>` by default ([`FilerRoot`]), so the key
//!   `cas/l/<hex>` is the entry `<hex>` of the directory `<root>/cas/l`. A key the filer
//!   cannot name that way (an empty segment, as in `a//b` or a key that begins or ends with
//!   `/`, or a NUL) is not sent, and is answered [`Unanswered::NotAPath`]: one odd key does
//!   not fail the call of the others.
//! - **What an entry answers** ([`Presence`]): a file at least as long as the size asked is
//!   [`Presence::Present`] with its length, a shorter one [`Presence::Short`]. No entry, or
//!   a directory, is [`Presence::Absent`]. An item the filer marks with an error (a volume
//!   it could not locate, a store read that failed) is [`Presence::Unanswered`]: the filer
//!   did not say the object is gone. The length is SeaweedFS's own (`filer.FileSize`): the
//!   largest of the recorded file size, the end of the furthest chunk, and the size of a
//!   remote copy newer than the entry, the length its S3 gateway answers a HEAD with.
//! - **A failed call fails the check** ([`FilerError`]): a refused, timed-out or unreachable
//!   call, or an answer with another number of results than names, is never an absence.
//!   The caller decides what to do without an answer.
//!
//! The volume locations of an entry's chunks are looked up from the filer's cache
//! (`unavailable_volume_is_miss` is off): a lookup proves an entry and its length, not that
//! every chunk can be read, as a HEAD through the S3 gateway does.

#[cfg(feature = "testkit")]
pub mod fake;

use std::time::Duration;

use kbf_proto::filer_pb::seaweed_filer_client::SeaweedFilerClient;
use kbf_proto::filer_pb::{
    Entry, LookupDirectoryEntriesRequest, LookupDirectoryEntryRequest, LookupDirectoryEntryResult,
};
use tonic::transport::{Channel, Endpoint};

use crate::ObjectKey;

/// The most names one `LookupDirectoryEntries` call may carry: the filer refuses more
/// (`maxFilerBatchLookupRequests` in `weed/server/filer_grpc_server_batch_lookup.go`).
pub const MAX_NAMES_PER_CALL: usize = 4096;

/// One object to ask about: its key, and the size kbf recorded for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    /// The object's key in the bucket.
    pub key: ObjectKey,
    /// The length the object must have at least to be [`Presence::Present`].
    pub size: u64,
}

/// What the store answered for one [`Ask`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    /// A file at least as long as asked; `size` is its length.
    Present {
        /// The file's length in bytes.
        size: u64,
    },
    /// A file shorter than asked; `size` is its length.
    Short {
        /// The file's length in bytes.
        size: u64,
    },
    /// No file under the key.
    Absent,
    /// No answer for this key.
    Unanswered(Unanswered),
}

/// Why one key has no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unanswered {
    /// The key is not a filer path (an empty segment or a NUL), so it was not asked.
    NotAPath,
    /// The filer marked the item with this error.
    Filer(String),
    /// The filer said it found the entry and sent none.
    NoEntry,
}

/// Why a check, or a client, failed as a whole.
#[derive(Debug, thiserror::Error)]
pub enum FilerError {
    /// The filer's address is not `<host>:<port>`.
    #[error("the filer address {0:?} is not <host>:<port>")]
    Address(String),
    /// The root is not an absolute directory path.
    #[error("the filer root {0:?} is not a directory path such as /buckets/<bucket>")]
    Root(String),
    /// A call was refused, timed out, or could not reach the filer.
    #[error("the filer at {filer} failed a lookup of {names} names: {status}")]
    Call {
        /// The filer's address.
        filer: String,
        /// The names the failed call carried.
        names: usize,
        /// What gRPC said.
        status: Box<tonic::Status>,
    },
    /// An answer with another number of results than names.
    #[error("the filer at {filer} answered {answered} results to a lookup of {asked} names")]
    Count {
        /// The filer's address.
        filer: String,
        /// The names asked.
        asked: usize,
        /// The results answered.
        answered: usize,
    },
}

/// The filer directory that holds a bucket's objects: an absolute path such as
/// `/buckets/kbf-cas`, with no empty, `.` or `..` segment and no NUL, and not `/` itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilerRoot(String);

impl FilerRoot {
    /// Checks and wraps a root.
    ///
    /// # Errors
    /// [`FilerError::Root`] for a path that is not one (see the type).
    pub fn new(root: impl Into<String>) -> Result<Self, FilerError> {
        let root = root.into();
        let segments_ok = root
            .strip_prefix('/')
            .is_some_and(|rest| rest.split('/').all(|s| !matches!(s, "" | "." | "..")));
        if !segments_ok || root.contains('\0') {
            return Err(FilerError::Root(root));
        }
        Ok(Self(root))
    }

    /// The root as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The directory and the name of the entry that holds `key`, or `None` when the key has
    /// an empty segment or a NUL.
    fn entry_of(&self, key: &str) -> Option<LookupDirectoryEntryRequest> {
        if key.contains('\0') || key.split('/').any(str::is_empty) {
            return None;
        }
        let (directory, name) = match key.rsplit_once('/') {
            Some((parent, name)) => (format!("{}/{parent}", self.0), name),
            None => (self.0.clone(), key),
        };
        let name = name.to_owned();
        Some(LookupDirectoryEntryRequest { directory, name })
    }
}

/// A store that says, for many keys at once, which objects it holds and how long they are.
pub trait PresenceCheck: Send + Sync {
    /// Why a check failed as a whole.
    type Error: std::error::Error + Send + Sync + 'static;

    /// One [`Presence`] for each of `asks`, in their order.
    fn check(
        &self,
        asks: &[Ask],
    ) -> impl Future<Output = Result<Vec<Presence>, Self::Error>> + Send;
}

/// The batch lookup of one filer (see the module docs).
#[derive(Clone, Debug)]
pub struct FilerPresence {
    client: SeaweedFilerClient<Channel>,
    root: FilerRoot,
    filer: String,
}

impl FilerPresence {
    /// A client of the filer whose gRPC port is at `filer` (`<host>:<port>`, plain HTTP/2),
    /// for the objects under `root`. Every call, and opening the connection, is given at
    /// most `timeout`. The connection is opened by the first call, so this must run inside
    /// a Tokio runtime, and an unreachable filer fails its calls, not this.
    ///
    /// # Errors
    /// [`FilerError::Address`] for an address that is not `<host>:<port>`.
    pub fn connect_lazy(
        filer: &str,
        root: FilerRoot,
        timeout: Duration,
    ) -> Result<Self, FilerError> {
        let bad = || FilerError::Address(filer.to_owned());
        let (host, port) = filer.rsplit_once(':').ok_or_else(bad)?;
        let port_ok = port.parse::<u16>().is_ok_and(|port| port != 0);
        if host.is_empty() || host.contains('/') || !port_ok {
            return Err(bad());
        }
        let endpoint = Endpoint::from_shared(format!("http://{filer}")).map_err(|_| bad())?;
        let channel = endpoint
            .connect_timeout(timeout)
            .timeout(timeout)
            .connect_lazy();
        // An answer carries every entry whole, with its chunk list, and the filer is the
        // one this host trusts: it is not held to tonic's default of 4 MiB.
        let client = SeaweedFilerClient::new(channel).max_decoding_message_size(usize::MAX);
        let filer = filer.to_owned();
        Ok(Self {
            client,
            root,
            filer,
        })
    }

    /// One call of at most [`MAX_NAMES_PER_CALL`] names; its results, one per name.
    async fn call(
        &self,
        requests: Vec<LookupDirectoryEntryRequest>,
    ) -> Result<Vec<LookupDirectoryEntryResult>, FilerError> {
        let asked = requests.len();
        let request = LookupDirectoryEntriesRequest {
            requests,
            unavailable_volume_is_miss: false,
        };
        let mut client = self.client.clone();
        let answer = client.lookup_directory_entries(request).await;
        let results = answer
            .map_err(|status| FilerError::Call {
                filer: self.filer.clone(),
                names: asked,
                status: Box::new(status),
            })?
            .into_inner()
            .results;
        if results.len() != asked {
            return Err(FilerError::Count {
                filer: self.filer.clone(),
                asked,
                answered: results.len(),
            });
        }
        Ok(results)
    }
}

impl PresenceCheck for FilerPresence {
    type Error = FilerError;

    async fn check(&self, asks: &[Ask]) -> Result<Vec<Presence>, FilerError> {
        let mut answers = vec![Presence::Unanswered(Unanswered::NotAPath); asks.len()];
        // The asks each call answers (by index), and the call's names, in the same order.
        let mut batches: Vec<(Vec<usize>, Vec<LookupDirectoryEntryRequest>)> = Vec::new();
        for (index, ask) in asks.iter().enumerate() {
            let Some(entry) = self.root.entry_of(ask.key.as_str()) else {
                continue;
            };
            match batches.last_mut() {
                Some((indices, requests)) if requests.len() < MAX_NAMES_PER_CALL => {
                    indices.push(index);
                    requests.push(entry);
                }
                _ => batches.push((vec![index], vec![entry])),
            }
        }
        let (indices, requests): (Vec<_>, Vec<_>) = batches.into_iter().unzip();
        let calls = requests.into_iter().map(|requests| self.call(requests));
        let answered = futures::future::try_join_all(calls).await?;
        for (indices, results) in indices.into_iter().zip(answered) {
            for (index, result) in indices.into_iter().zip(results) {
                answers[index] = judge(result, asks[index].size);
            }
        }
        Ok(answers)
    }
}

/// What one result of the filer answers for an object of at least `asked` bytes.
fn judge(result: LookupDirectoryEntryResult, asked: u64) -> Presence {
    if !result.error.is_empty() {
        return Presence::Unanswered(Unanswered::Filer(result.error));
    }
    if !result.found {
        return Presence::Absent;
    }
    let Some(entry) = result.entry else {
        return Presence::Unanswered(Unanswered::NoEntry);
    };
    if entry.is_directory {
        return Presence::Absent;
    }
    let size = file_size(&entry);
    if size >= asked {
        Presence::Present { size }
    } else {
        Presence::Short { size }
    }
}

/// An entry's length as SeaweedFS reckons it (`filer.FileSize` in `weed/filer/filechunks.go`):
/// 0 with no attributes; otherwise the largest of the recorded file size, the end of the
/// furthest chunk, and the size of a remote copy modified after the entry.
fn file_size(entry: &Entry) -> u64 {
    let Some(attributes) = &entry.attributes else {
        return 0;
    };
    let remote = entry
        .remote_entry
        .as_ref()
        .filter(|remote| remote.remote_mtime > attributes.mtime)
        .map_or(0, |remote| u64::try_from(remote.remote_size).unwrap_or(0));
    let chunks = entry
        .chunks
        .iter()
        .map(|chunk| {
            u64::try_from(chunk.offset)
                .unwrap_or(0)
                .saturating_add(chunk.size)
        })
        .max()
        .unwrap_or(0);
    attributes.file_size.max(remote).max(chunks)
}
