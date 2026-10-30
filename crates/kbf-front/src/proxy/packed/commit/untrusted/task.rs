//! Ingest of the pack a task wrote: [`ingest`] reads the object a task put under the key the
//! frontend minted for its lease, trusts nothing in it, and rebuilds the action's result from
//! what it verified.
//!
//! The task reports nothing but an exit code, so the frontend derives everything from the object
//! itself, in this order, each step refusing before the next reads more:
//!
//! 1. **The key.** It must be the key minted for this lease and action
//!    ([`PackKeyMint::verify`], a constant-time MAC check): a key that did not come from the
//!    frontend is refused without a request to the store.
//! 2. **The length** ([`kbf_objstore::ObjectStore::len`], a HEAD for a store over the network): no
//!    object is `Missing`, one over the cap is `TooLarge`, and nothing has been read yet.
//! 3. **The footer.** The trailer, then the footer it says is there (at most as many records as
//!    the limits allow), parsed with its CRC. [`kbf_pack::check_shape`] then applies the rules of
//!    a task pack: records back to back from offset 0 to the footer, none of size 0, none over a
//!    cap, the record at offset 0 (the manifest) at most [`MAX_UNTRUSTED_MANIFEST_BYTES`]. The
//!    manifest is refused when it is too long, before it is read.
//! 4. **The manifest.** The record at offset 0 is read, checked against its CRC and digest, and
//!    parsed. It must be for this lease and this action, and every digest it names must be a
//!    record of the pack (or the empty blob, which has no record).
//! 5. **The trees.** Each output directory's `Tree` is read from the pack itself (its record is
//!    at the offset and size of its footer entry), checked against its CRC and SHA-256, decoded
//!    (a record is at most the cap of a record), and every file it names must be a record of the
//!    pack as well, or the empty blob. This is before anything is recorded: a hollow or damaged
//!    tree leaves nothing indexed.
//! 6. **Every byte.** The footer is turned into the pack's manifest for
//!    [`Packs::commit_untrusted_pack`], which hashes every record and records the pack where it
//!    lies. Nothing a manifest says is believed beyond this: it names digests, the pack's own
//!    hashing says the records are those digests.
//! 7. **The result** is rebuilt from the manifest's now-verified digests, sorted by path, with
//!    no execution metadata (the task's timings are advisory and not carried). It is written to
//!    the action cache only if the action exited 0 and its Action does not say `do_not_cache`,
//!    by the same two calls a daemon's result takes ([`Backend::prepare_action_result`] and
//!    [`Backend::commit_action_record`]); a failed action is reported and not cached.
//!
//! A refusal at steps 1 to 5 indexes nothing; one at step 6 (a record that is not its digest, a
//! key recorded before with another pack) records nothing new. A failure of the action cache at
//! step 7 leaves a recorded pack with no action-cache entry, which is harmless (its blobs are
//! readable and a re-run finds them).
//!
//! What is not decided here:
//!
//! - **The age of the key.** [`PackKeyMint::verify`] says the minute a key was minted in, and
//!   `ingest` enforces no age: the executor knows the run's wall time and decides when a key has
//!   expired.
//! - Whether the paths are the Command's declared outputs (the executor knows the Command).
//! - A replaced object (the store's create-only write is the guard).
//!
//! The `backend` must be built over the same [`Packs`] as `packs`
//! (`Backend::remote_over(node, Some(packs))`): the Action it reads to decide on caching may be a
//! blob a frontend keeps in a pack, which a backend over other packs, or none, does not find (the
//! ingest is then `Unavailable`). The trees do not need it: they are read from the task's pack.
//! The wiring of the executor builds the backend so.

use std::collections::BTreeSet;

use kbf_meta::Closure;
use kbf_objstore::{ByteRange, ObjectKey};
use kbf_pack::{MANIFEST_VERSION, ShapeError, TaskManifest, check_shape};
use kbf_pack_index::{BlobDigest, NewPack, PackBlob};
use kbf_proto::reapi::{self, ActionResult};
use kbf_segments::layout::{TRAILER_LEN, footer_len};
use kbf_segments::{Footer, IndexEntry};
use kbf_types::Digest;
use prost::Message as _;

use super::{CommitError, IssuedKeySet, IssuedPackKey, PackKeyMint, Packs, UntrustedLimits};
use crate::backend::Backend;
use crate::closure;
use crate::wire::digest_to_proto;

/// Why a pack was refused. Also the label of the count of refusals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// No object under the key.
    Missing,
    /// The key is not the one issued for the lease and the action, or the manifest is for
    /// another lease or action, or the key was recorded before with another pack.
    WrongKey,
    /// The pack, its record count, a record or the manifest record is over its cap.
    TooLarge,
    /// The footer or trailer is damaged, or the records are out of order, overlap, leave a gap,
    /// or include one of no bytes.
    Malformed,
    /// A record's bytes do not hash to its digest.
    HashMismatch,
    /// The record at offset 0 is not a valid manifest, or a tree is not a REAPI `Tree`.
    BadManifest,
    /// A digest the manifest or a tree names is not a record of the pack.
    Incomplete,
}

impl RejectReason {
    /// Every reason, in the order of the variants (the order of the counters).
    pub const ALL: [Self; 7] = [
        Self::Missing,
        Self::WrongKey,
        Self::TooLarge,
        Self::Malformed,
        Self::HashMismatch,
        Self::BadManifest,
        Self::Incomplete,
    ];

    /// The label of the reason in the metrics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::WrongKey => "wrong_key",
            Self::TooLarge => "too_large",
            Self::Malformed => "malformed",
            Self::HashMismatch => "hash_mismatch",
            Self::BadManifest => "bad_manifest",
            Self::Incomplete => "incomplete",
        }
    }
}

/// The object store, the pack index or the action cache could not answer. Nothing says the pack
/// is bad: the same ingest may succeed later, or the lease is run again under a new key.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StoreError(pub String);

/// What [`ingest`] made of a pack.
#[derive(Debug, PartialEq)]
pub enum Ingested {
    /// The action ran. `result` is its result as the frontend rebuilt it (the exit code, the
    /// outputs, stdout and stderr); `cached` says whether it was written to the action cache.
    Ran {
        /// The rebuilt result (boxed: it is far larger than the other endings).
        result: Box<ActionResult>,
        /// Whether the action cache has it now.
        cached: bool,
    },
    /// The pack is not acceptable; the action is run again on a new lease with a new key.
    Reject(RejectReason),
    /// The store, the index or the action cache could not answer.
    Unavailable(StoreError),
}

/// Where an ingest stopped.
enum Stop {
    Reject(RejectReason),
    Unavailable(StoreError),
}

type Step<T> = Result<T, Stop>;

impl From<RejectReason> for Stop {
    fn from(reason: RejectReason) -> Self {
        Self::Reject(reason)
    }
}

impl From<CommitError> for Stop {
    fn from(error: CommitError) -> Self {
        match error {
            CommitError::Forbidden(_) | CommitError::Conflict(_) => RejectReason::WrongKey.into(),
            CommitError::Invalid(_) => RejectReason::Malformed.into(),
            CommitError::Mismatch(_) => RejectReason::HashMismatch.into(),
            CommitError::Missing(_) => RejectReason::Missing.into(),
            CommitError::Unavailable(message) => Self::Unavailable(StoreError(message)),
        }
    }
}

/// Reads, verifies and commits the pack a task wrote under `key` for `action`, on the lease
/// `lease`; the module docs give the steps. `key` and `lease` come from the frontend's own lease
/// row, never from the task. `mint` is the frontend's, and `limits` bound the pack.
///
/// Every ending is counted: an ingest (`kbf_pack_ingests_total`), a refusal by reason
/// (`kbf_pack_ingest_rejects_total`) or a store that could not answer
/// (`kbf_pack_ingest_unavailable_total`).
pub async fn ingest(
    packs: &Packs,
    backend: &Backend,
    mint: &PackKeyMint,
    lease: &str,
    key: &IssuedPackKey,
    action: &Digest,
    limits: &UntrustedLimits,
) -> Ingested {
    let done = Ingest {
        packs,
        backend,
        mint,
        lease,
        key,
        action,
        limits,
    };
    let metrics = &packs.metrics;
    match done.run().await {
        Ok((result, cached)) => {
            metrics.ingests.add(1);
            Ingested::Ran {
                result: Box::new(result),
                cached,
            }
        }
        Err(Stop::Reject(reason)) => {
            metrics.ingest_rejects[reason as usize].add(1);
            Ingested::Reject(reason)
        }
        Err(Stop::Unavailable(error)) => {
            metrics.ingest_unavailable.add(1);
            Ingested::Unavailable(error)
        }
    }
}

/// Reads, verifies and commits the whole run `keys` names (red-first stub: only pack 0 is read).
pub async fn ingest_set(
    packs: &Packs,
    backend: &Backend,
    mint: &PackKeyMint,
    lease: &str,
    keys: &IssuedKeySet,
    action: &Digest,
    limits: &UntrustedLimits,
) -> Ingested {
    match keys.packs().first() {
        Some(key) => ingest(packs, backend, mint, lease, key, action, limits).await,
        None => Ingested::Reject(RejectReason::WrongKey),
    }
}

/// The inputs of one ingest.
struct Ingest<'a> {
    packs: &'a Packs,
    backend: &'a Backend,
    mint: &'a PackKeyMint,
    lease: &'a str,
    key: &'a IssuedPackKey,
    action: &'a Digest,
    limits: &'a UntrustedLimits,
}

impl Ingest<'_> {
    async fn run(&self) -> Step<(ActionResult, bool)> {
        let object = &self.key.object;
        let wrong_key = || Stop::from(RejectReason::WrongKey);
        if self.key.build() != self.lease {
            return Err(wrong_key());
        }
        self.mint
            .verify(self.key, self.action)
            .map_err(|_| wrong_key())?;
        let size = self.length(object).await?;
        let (footer, entry) = self.footer(object, size).await?;
        let manifest = self.manifest(object, &entry).await?;
        let held: BTreeSet<Digest> = footer.entries().iter().map(|e| e.digest).collect();
        if manifest
            .output_digests()
            .any(|digest| !held.contains(&digest))
        {
            return Err(RejectReason::Incomplete.into());
        }
        self.trees_held(object, &manifest, &footer, &held).await?;
        let pack = new_pack(self.key, &footer, size);
        let recorded = self
            .packs
            .commit_untrusted_pack(self.key, pack, self.limits);
        recorded.await?;
        let result = rebuilt(&manifest);
        let cached = self.cached(&result, manifest.exit_code).await?;
        Ok((result, cached))
    }

    /// The length of the object, if it is there and under the cap.
    async fn length(&self, object: &ObjectKey) -> Step<u64> {
        let size = match self.packs.store.len(object).await {
            Ok(Some(size)) => size,
            Ok(None) => return Err(RejectReason::Missing.into()),
            Err(error) => return Err(unavailable(&format!("the length of {object}: {error}"))),
        };
        if size > self.limits.max_pack_bytes {
            return Err(RejectReason::TooLarge.into());
        }
        if size < TRAILER_LEN as u64 {
            return Err(RejectReason::Malformed.into());
        }
        Ok(size)
    }

    /// The footer of the object, parsed and checked as a task pack's, and the entry of its
    /// manifest.
    async fn footer(&self, object: &ObjectKey, size: u64) -> Step<(Footer, IndexEntry)> {
        let trailer = TRAILER_LEN as u64;
        let tail = self.read(object, size - trailer, trailer).await?;
        let wanted = Footer::len_from_trailer(&tail).map_err(|_| RejectReason::Malformed)?;
        if wanted > footer_len(self.limits.max_blobs as u64) {
            return Err(RejectReason::TooLarge.into());
        }
        if wanted > size {
            return Err(RejectReason::Malformed.into());
        }
        let bytes = self.read(object, size - wanted, wanted).await?;
        let footer = Footer::parse(&bytes, size).map_err(|_| RejectReason::Malformed)?;
        let entries = footer.entries();
        if entries
            .iter()
            .any(|entry| entry.digest.size_bytes > self.limits.max_blob_bytes)
        {
            return Err(RejectReason::TooLarge.into());
        }
        let shape = check_shape(&footer, size);
        let entry = *shape.map_err(|shape| Stop::from(shaped(&shape)))?;
        Ok((footer, entry))
    }

    /// The manifest: the record at offset 0, read after its size was checked, verified against
    /// its own entry, parsed, and required to be for this lease and action.
    async fn manifest(&self, object: &ObjectKey, entry: &IndexEntry) -> Step<TaskManifest> {
        let range = entry.range();
        let record = self
            .read(object, range.start, range.end - range.start)
            .await?;
        entry
            .verify(&record)
            .map_err(|_| RejectReason::HashMismatch)?;
        let manifest = TaskManifest::parse(&record).map_err(|_| RejectReason::BadManifest)?;
        // This ingest reads one pack and no large object: a manifest that says more would have its
        // other objects silently dropped.
        if manifest.version() != MANIFEST_VERSION {
            return Err(RejectReason::BadManifest.into());
        }
        if manifest.lease != self.lease || manifest.action != *self.action {
            return Err(RejectReason::WrongKey.into());
        }
        Ok(manifest)
    }

    /// Checks that every file of every output tree is a record of the pack, reading each tree
    /// from the pack itself: its record is verified against its footer entry (length, CRC and
    /// SHA-256), and decoded (it is at most the cap of a record). This is before the commit, so a
    /// pack with a hollow or damaged tree indexes nothing.
    async fn trees_held(
        &self,
        object: &ObjectKey,
        manifest: &TaskManifest,
        footer: &Footer,
        held: &BTreeSet<Digest>,
    ) -> Step<()> {
        let trees: BTreeSet<Digest> = manifest
            .output_directories
            .iter()
            .map(|dir| dir.tree)
            .filter(|tree| tree.size_bytes != 0)
            .collect();
        let mut files = Closure::new();
        for entry in footer
            .entries()
            .iter()
            .filter(|entry| trees.contains(&entry.digest))
        {
            let range = entry.range();
            let record = self
                .read(object, range.start, range.end - range.start)
                .await?;
            entry
                .verify(&record)
                .map_err(|_| RejectReason::HashMismatch)?;
            closure::add_tree(&mut files, &entry.digest, record)
                .map_err(|_| RejectReason::BadManifest)?;
        }
        if files.iter().any(|digest| !held.contains(digest)) {
            return Err(RejectReason::Incomplete.into());
        }
        Ok(())
    }

    /// Writes `result` to the action cache if the action exited 0 and may be cached; whether it
    /// did.
    async fn cached(&self, result: &ActionResult, exit_code: i32) -> Step<bool> {
        if exit_code != 0 {
            return Ok(false);
        }
        let blob = self.backend.read_blob(self.action).await;
        let blob = blob.map_err(|error| unavailable(&format!("the Action: {error}")))?;
        // An Action that does not decode is not cached: the frontend cannot say it may be.
        let cacheable = reapi::Action::decode(blob).is_ok_and(|action| !action.do_not_cache);
        if !cacheable {
            return Ok(false);
        }
        let prepared = self.backend.prepare_action_result(result).await;
        let record = prepared.map_err(|error| unavailable(&format!("the result: {error}")))?;
        let committed = self
            .backend
            .commit_action_record(*self.action, record)
            .await;
        committed.map_err(|error| unavailable(&format!("the action cache: {error}")))?;
        Ok(true)
    }

    /// The bytes `[offset, offset + len)` of the object.
    async fn read(&self, object: &ObjectKey, offset: u64, len: u64) -> Step<bytes::Bytes> {
        let range = ByteRange::new(offset, len).ok_or(RejectReason::Malformed)?;
        Ok(self.packs.fetch(object, range).await?)
    }
}

fn unavailable(message: &str) -> Stop {
    Stop::Unavailable(StoreError(message.to_owned()))
}

/// The reason a footer is not a task pack's.
const fn shaped(shape: &ShapeError) -> RejectReason {
    match shape {
        ShapeError::TooLarge { .. }
        | ShapeError::TooManyRecords { .. }
        | ShapeError::RecordTooLarge { .. }
        | ShapeError::ManifestTooLarge { .. } => RejectReason::TooLarge,
        ShapeError::NoRecords
        | ShapeError::ZeroSizeRecord { .. }
        | ShapeError::NotContiguous { .. } => RejectReason::Malformed,
    }
}

/// The manifest of the pack for [`Packs::commit_untrusted_pack`], as the footer says. Nothing
/// the footer says is believed: the commit hashes every record and checks the layout again.
fn new_pack(key: &IssuedPackKey, footer: &Footer, size: u64) -> NewPack {
    let blob = |entry: &IndexEntry| PackBlob {
        digest: BlobDigest {
            hash: entry.digest.hash,
            size: entry.digest.size_bytes,
        },
        offset: entry.offset,
    };
    NewPack {
        key: key.key().to_owned(),
        writer: key.build().to_owned(),
        size,
        blobs: footer.entries().iter().map(blob).collect(),
    }
}

/// The result the manifest describes, sorted by path as REAPI asks. It carries the manifest's
/// digests, which the pack's records have been hashed to, and no execution metadata.
fn rebuilt(manifest: &TaskManifest) -> ActionResult {
    let wire = digest_to_proto;
    let mut result = ActionResult {
        output_files: manifest
            .output_files
            .iter()
            .map(|file| reapi::OutputFile {
                path: file.path.clone(),
                digest: Some(wire(&file.digest)),
                is_executable: file.is_executable,
                ..reapi::OutputFile::default()
            })
            .collect(),
        output_directories: manifest
            .output_directories
            .iter()
            .map(|dir| reapi::OutputDirectory {
                path: dir.path.clone(),
                tree_digest: Some(wire(&dir.tree)),
                ..reapi::OutputDirectory::default()
            })
            .collect(),
        output_symlinks: manifest
            .output_symlinks
            .iter()
            .map(|link| reapi::OutputSymlink {
                path: link.path.clone(),
                target: link.target.clone(),
                ..reapi::OutputSymlink::default()
            })
            .collect(),
        stdout_digest: manifest.stdout.as_ref().map(wire),
        stderr_digest: manifest.stderr.as_ref().map(wire),
        exit_code: manifest.exit_code,
        ..ActionResult::default()
    };
    result.output_files.sort_by(|a, b| a.path.cmp(&b.path));
    result
        .output_directories
        .sort_by(|a, b| a.path.cmp(&b.path));
    result.output_symlinks.sort_by(|a, b| a.path.cmp(&b.path));
    result
}
