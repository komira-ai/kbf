//! Ingest of the run a task wrote: [`ingest_set`] reads the objects the task put under the keys the
//! frontend minted for its lease, trusts nothing in them, and rebuilds the action's result from what
//! it verified. [`ingest`] is the run of one pack and no large object (the keys of a task that was
//! given one key): the set of that key.
//!
//! The task reports nothing but an exit code, so the frontend derives everything from the objects
//! themselves, in this order, each step refusing before the next reads more:
//!
//! 1. **The keys.** Every key of the [`IssuedKeySet`] must be one minted for this lease and action
//!    ([`PackKeyMint::verify`], a constant-time MAC check) and be in the place its slot names (pack
//!    keys by `0..`, large keys by `L0..`, at most a run's slots of each): a key that did not come
//!    from the frontend, or is another's, or is out of place, is refused without a request to the
//!    store.
//! 2. **Pack 0.** The length ([`kbf_objstore::ObjectStore::len`], a HEAD for a store over the
//!    network: none is `Missing`, one over the cap `TooLarge`), the trailer, and the footer it says
//!    is there (at most as many records as the limits allow), parsed with its CRC.
//!    [`kbf_pack::check_shape`] applies the rules of a task pack: records back to back from offset
//!    0 to the footer, none of size 0, none over a cap, and the record at offset 0, the manifest, at
//!    most [`MAX_UNTRUSTED_MANIFEST_BYTES`] (refused before it is read).
//! 3. **The manifest.** The record at offset 0 is read, checked against its CRC and digest and
//!    parsed, with large objects up to [`UntrustedLimits::max_large_bytes`]. It must be for this
//!    lease and this action, name no more packs than the set has keys for and no large slot the set
//!    has no key for (`BadManifest`).
//! 4. **The other packs.** Packs `1..packs` are read one at a time, footer only, by the same rules
//!    without the manifest bound ([`kbf_pack::check_records`]: the first record of a pack that holds
//!    no manifest may be of any size up to the cap of a record). A pack that is not there is
//!    `Incomplete`. This keeps the verified record lists, never a pack's bytes.
//! 5. **The large objects.** Each is asked for its length: absent is `Incomplete`, a length that is
//!    not the digest's size is `HashMismatch` (those bytes are not that blob), and no byte of it
//!    has been read.
//! 6. **Where every digest is.** A digest is in one place: one record of one pack or one large
//!    object. A digest in two places is `Malformed`; a digest the manifest names (or a tree names)
//!    that is in none is `Incomplete`.
//! 7. **The trees.** Each output directory's `Tree` is read from the pack that holds it (its
//!    record is at the offset and size of its footer entry), checked against its CRC and SHA-256,
//!    decoded (a record is at most the cap of a record), and every file it names must be in a place
//!    as well. A tree that is a large object cannot be read here and is `Incomplete`. This is
//!    before anything is stored: a hollow or damaged tree leaves nothing indexed.
//! 8. **The large objects, stored.** Each is streamed from the object store through SHA-256 into
//!    the CAS, a window at a time and never held whole ([`super::large`]): a mismatch is
//!    `HashMismatch` and nothing is kept under that digest, a store that failed is `Unavailable`.
//! 9. **Every byte of the packs.** Each footer is turned into the pack's manifest for
//!    [`Packs::commit_untrusted_pack`], which hashes every record and records the pack where it
//!    lies. Nothing a manifest says is believed beyond this: it names digests, the pack's own
//!    hashing says the records are those digests. The packs are recorded last pack first, pack 0
//!    last, so a run that fails midway leaves no row for the pack that holds the manifest.
//! 10. **The result** is rebuilt from the manifest's now-verified digests, sorted by path, with no
//!     execution metadata (the task's timings are advisory and not carried). It is written to the
//!     action cache only if the action exited 0 and its Action does not say `do_not_cache`, by the
//!     same two calls a daemon's result takes ([`Backend::prepare_action_result`] and
//!     [`Backend::commit_action_record`]); a failed action is reported and not cached. It is written
//!     after every pack is recorded and every large object is in the CAS, so a reader never finds a
//!     result whose blobs cannot be found.
//!
//! A refusal at steps 1 to 7 stores and indexes nothing. A refusal at step 8 or 9 can leave what was
//! stored before it: large objects in the CAS, each under the digest its own bytes hash to, and
//! packs recorded whose records were hashed. Those are valid blobs nobody names, and a re-run finds
//! them. A failure of the action cache at step 10 leaves them with no action-cache entry, which is
//! harmless for the same reason.
//!
//! What is not decided here:
//!
//! - **The age of the run's wall.** [`PackKeyMint::verify`] says the minute a key was minted in,
//!   and `ingest` refuses a key as old as [`kbf_pack::MIN_GRACE`] (`Expired`), the age from which
//!   the janitor of `cas/b/` may delete it; a younger key is the executor's to judge against the
//!   run's wall time.
//! - Whether the paths are the Command's declared outputs (the executor knows the Command).
//! - A replaced object (the store's create-only write is the guard).
//!
//! The `backend` must be built over the same [`Packs`] as `packs`
//! (`Backend::remote_over(node, Some(packs))`): the Action it reads to decide on caching may be a
//! blob a frontend keeps in a pack, which a backend over other packs, or none, does not find (the
//! ingest is then `Unavailable`). The trees do not need it: they are read from the task's packs.
//! The wiring of the executor builds the backend so.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use kbf_meta::Closure;
use kbf_objstore::{ByteRange, ObjectKey};
use kbf_pack::{
    MAX_LARGE, MAX_PACKS, MIN_GRACE, ShapeError, Slot, TaskManifest, TaskPackKey, check_records,
    check_shape,
};
use kbf_pack_index::{BlobDigest, NewPack, PackBlob};
use kbf_proto::reapi::{self, ActionResult};
use kbf_segments::layout::{TRAILER_LEN, footer_len};
use kbf_segments::{Footer, IndexEntry};
use kbf_types::Digest;
use prost::Message as _;

use super::large::Large;
use super::metadata::claimed;
use super::{CommitError, IssuedKeySet, IssuedPackKey, PackKeyMint, Packs, UntrustedLimits};
use crate::backend::Backend;
use crate::closure;
use crate::wire::digest_to_proto;

/// Why a run was refused. Also the label of the count of refusals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// No object under the key of pack 0.
    Missing,
    /// A key is not the one issued for the lease and the action (or is not in the place its slot
    /// names), or the manifest is for another lease or action, or a key was recorded before with
    /// another pack.
    WrongKey,
    /// A pack, its record count, a record or the manifest record is over its cap.
    TooLarge,
    /// A footer or trailer is damaged, or the records are out of order, overlap, leave a gap, or
    /// include one of no bytes, or a digest is in two places.
    Malformed,
    /// A record's bytes do not hash to its digest, or a large object is not the blob its digest
    /// names (its length is not the digest's size, or its bytes do not hash to it).
    HashMismatch,
    /// The record at offset 0 is not a valid manifest, or it names more packs or other large
    /// slots than the keys issued, or a tree is not a REAPI `Tree`.
    BadManifest,
    /// A pack beyond the first, or a large object, is not in the store, or a digest the manifest
    /// or a tree names is in no pack and no large object.
    Incomplete,
    /// A key is as old as the longest it can be in use ([`kbf_pack::MIN_GRACE`]): the janitor
    /// of `cas/b/` may delete it, so the run is not ingested, and the action is run again under
    /// a new key set.
    Expired,
    /// The janitor of `cas/b/` has claimed a key of the run for its delete: the run is not
    /// ingested, and the action is run again under a new key set.
    Claimed,
}

impl RejectReason {
    /// Every reason, in the order of the variants (the order of the counters).
    pub const ALL: [Self; 9] = [
        Self::Missing,
        Self::WrongKey,
        Self::TooLarge,
        Self::Malformed,
        Self::HashMismatch,
        Self::BadManifest,
        Self::Incomplete,
        Self::Expired,
        Self::Claimed,
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
            Self::Expired => "expired",
            Self::Claimed => "claimed",
        }
    }
}

/// The object store, the pack index, the CAS or the action cache could not answer. Nothing says the
/// run is bad: the same ingest may succeed later, or the lease is run again under a new key set.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StoreError(pub String);

/// What [`ingest`] and [`ingest_set`] made of a run.
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
    /// The run is not acceptable; the action is run again on a new lease with a new key set.
    Reject(RejectReason),
    /// The object store, the index, the CAS or the action cache could not answer.
    Unavailable(StoreError),
}

/// Where an ingest stopped.
#[derive(Clone)]
pub(super) enum Stop {
    Reject(RejectReason),
    Unavailable(StoreError),
}

impl Stop {
    /// The same stop, except that an object that is not there is `Incomplete`: for an object beyond
    /// the first, which the manifest of pack 0 says must be.
    pub(super) fn absent_is_incomplete(self) -> Self {
        match self {
            Self::Reject(RejectReason::Missing) => RejectReason::Incomplete.into(),
            other => other,
        }
    }
}

pub(super) type Step<T> = Result<T, Stop>;

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
/// `lease`: [`ingest_set`] of the set of that one key, a run of one pack and no large object.
/// `key` and `lease` come from the frontend's own lease row, never from the task. `mint` is the
/// frontend's, and `limits` bound the pack.
pub async fn ingest(
    packs: &Packs,
    backend: &Backend,
    mint: &PackKeyMint,
    lease: &str,
    key: &IssuedPackKey,
    action: &Digest,
    limits: &UntrustedLimits,
) -> Ingested {
    let keys = IssuedKeySet::single(key.clone());
    ingest_set(packs, backend, mint, lease, &keys, action, limits).await
}

/// Reads, verifies and commits the run a task wrote under the keys of `keys` for `action`, on the
/// lease `lease`; the module docs give the steps. `keys` and `lease` come from the frontend's own
/// lease row, never from the task. `mint` is the frontend's, and `limits` bound the run.
///
/// Every key of `keys` is held in [`Packs::verifying_keys`] from the start of the call until it
/// returns or is dropped, so the janitor of `cas/b/` never deletes a pack or a large object this
/// ingest is reading or copying, however old its key says it is. A key the janitor has claimed
/// for its delete is refused as `Claimed`, and one as old as [`kbf_pack::MIN_GRACE`] as `Expired`.
///
/// Every ending is counted: an ingest (`kbf_pack_ingests_total`, with the packs of the run in
/// `kbf_pack_ingest_packs_total` and the bytes of its large objects in
/// `kbf_pack_ingest_large_bytes_total`), a refusal by reason (`kbf_pack_ingest_rejects_total`) or a
/// store that could not answer (`kbf_pack_ingest_unavailable_total`).
pub async fn ingest_set(
    packs: &Packs,
    backend: &Backend,
    mint: &PackKeyMint,
    lease: &str,
    keys: &IssuedKeySet,
    action: &Digest,
    limits: &UntrustedLimits,
) -> Ingested {
    let done = Ingest {
        packs,
        backend,
        mint,
        lease,
        keys,
        action,
        limits,
    };
    let metrics = &packs.metrics;
    match done.run().await {
        Ok(ran) => {
            metrics.ingests.add(1);
            metrics.ingest_packs.add(ran.packs);
            metrics.ingest_large_bytes.add(ran.large_bytes);
            Ingested::Ran {
                result: Box::new(ran.result),
                cached: ran.cached,
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

/// The inputs of one ingest.
struct Ingest<'a> {
    packs: &'a Packs,
    backend: &'a Backend,
    mint: &'a PackKeyMint,
    lease: &'a str,
    keys: &'a IssuedKeySet,
    action: &'a Digest,
    limits: &'a UntrustedLimits,
}

/// What an ingest made of a run that ran.
struct Ran {
    result: ActionResult,
    cached: bool,
    packs: u64,
    large_bytes: u64,
}

/// One pack of the run as far as its footer shows: its key, its length and its records.
struct Laid<'k> {
    key: &'k IssuedPackKey,
    size: u64,
    footer: Footer,
}

/// Where a digest the run holds is.
#[derive(Clone, Copy)]
enum Place {
    /// A record of the pack at this index of the run, at this entry.
    Pack { at: usize, entry: IndexEntry },
    /// A large object.
    Large,
}

impl Ingest<'_> {
    async fn run(&self) -> Step<Ran> {
        // Every key of the set is held from here to the end of the run, or its being dropped, and
        // a key the janitor has claimed for its delete refuses the run before anything is read:
        // the janitor never deletes what this ingest is reading or copying, and this ingest never
        // records a pack whose object the janitor is deleting (`verifying`).
        let names = self.keys.packs().iter().chain(self.keys.large());
        let names = names.map(|key| key.key().to_owned()).collect();
        let _held = self
            .packs
            .verifying
            .hold(names)
            .map_err(|_| Stop::from(RejectReason::Claimed))?;
        let first = self.verified()?;
        let size = self.length(&first.object).await?;
        let (footer, entry) = self.footer(&first.object, size, true).await?;
        let manifest = self.manifest(&first.object, &entry).await?;
        self.issued(&manifest)?;
        let mut laid = vec![Laid {
            key: first,
            size,
            footer,
        }];
        for at in 1..usize::from(manifest.packs) {
            laid.push(self.second(&self.keys.packs()[at]).await?);
        }
        let larges: Vec<Large<'_>> = manifest
            .large
            .iter()
            .map(|entry| {
                let key = &self.keys.large()[usize::from(entry.slot)];
                Large::new(self.packs, key, entry.digest)
            })
            .collect();
        for large in &larges {
            large.check_length().await?;
        }
        let places = places(&laid, &manifest)?;
        if manifest
            .output_digests()
            .any(|digest| !places.contains_key(&digest))
        {
            return Err(RejectReason::Incomplete.into());
        }
        self.trees_held(&laid, &manifest, &places).await?;
        for large in &larges {
            large.store(self.backend).await?;
        }
        for pack in laid.iter().rev() {
            let new = new_pack(pack.key, &pack.footer, pack.size);
            let recorded = self.packs.commit_untrusted_pack(pack.key, new, self.limits);
            recorded.await?;
        }
        let result = rebuilt(&manifest, self.packs.now());
        let cached = self.cached(&result, manifest.exit_code).await?;
        let large_bytes = manifest.large.iter().map(|large| large.size()).sum();
        Ok(Ran {
            result,
            cached,
            packs: laid.len() as u64,
            large_bytes,
        })
    }

    /// Checks every key of the set: for this lease, minted by the frontend for this action, and in
    /// the place its slot names. Nothing is asked of the store. The key of pack 0.
    fn verified(&self) -> Step<&IssuedPackKey> {
        let wrong_key = || Stop::from(RejectReason::WrongKey);
        let (packs, large) = (self.keys.packs(), self.keys.large());
        let Some(first) = packs.first() else {
            return Err(wrong_key());
        };
        if packs.len() > usize::from(MAX_PACKS) {
            return Err(wrong_key());
        }
        if large.len() > usize::from(MAX_LARGE) {
            return Err(wrong_key());
        }
        let packs = packs.iter().zip((0..).map(Slot::Pack));
        let large = large.iter().zip((0..).map(Slot::Large));
        for (key, slot) in packs.chain(large) {
            if key.build() != self.lease {
                return Err(wrong_key());
            }
            if slot_of(key) != Some(slot) {
                return Err(wrong_key());
            }
            let minted = self
                .mint
                .verify(key, self.action)
                .map_err(|_| wrong_key())?;
            // A key as old as the longest it can be in use is never committed: the janitor may
            // delete it from that age on.
            let age = self.packs.now().duration_since(minted);
            if age.unwrap_or(Duration::ZERO) >= MIN_GRACE {
                return Err(RejectReason::Expired.into());
            }
        }
        Ok(first)
    }

    /// Checks that the manifest names no more packs than the set has keys for and no large slot
    /// the set has no key for.
    fn issued(&self, manifest: &TaskManifest) -> Step<()> {
        if usize::from(manifest.packs) > self.keys.packs().len() {
            return Err(RejectReason::BadManifest.into());
        }
        let large = self.keys.large().len();
        if manifest
            .large
            .iter()
            .any(|entry| usize::from(entry.slot) >= large)
        {
            return Err(RejectReason::BadManifest.into());
        }
        Ok(())
    }

    /// A pack beyond the first, read as far as its footer: an object that is not there is
    /// `Incomplete`.
    async fn second<'k>(&self, key: &'k IssuedPackKey) -> Step<Laid<'k>> {
        let laid = async {
            let size = self.length(&key.object).await?;
            let (footer, _) = self.footer(&key.object, size, false).await?;
            Ok::<_, Stop>(Laid { key, size, footer })
        };
        laid.await.map_err(Stop::absent_is_incomplete)
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

    /// The footer of the object, parsed and checked as a task pack's, and the entry at offset 0.
    /// Pack 0 (`manifest`) holds the manifest there, which is at most the cap of one.
    async fn footer(
        &self,
        object: &ObjectKey,
        size: u64,
        manifest: bool,
    ) -> Step<(Footer, IndexEntry)> {
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
        let shape = if manifest {
            check_shape(&footer, size)
        } else {
            check_records(&footer, size)
        };
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
        let large_max = self.limits.max_large_bytes;
        let manifest =
            TaskManifest::parse_with(&record, large_max).map_err(|_| RejectReason::BadManifest)?;
        if manifest.lease != self.lease || manifest.action != *self.action {
            return Err(RejectReason::WrongKey.into());
        }
        Ok(manifest)
    }

    /// Checks that every file of every output tree is in a place of the run, reading each tree
    /// from the pack that holds it: its record is verified against its footer entry (length, CRC
    /// and SHA-256), and decoded (it is at most the cap of a record). This is before anything is
    /// stored, so a run with a hollow or damaged tree stores and indexes nothing. A tree that is a
    /// large object is not read and is `Incomplete`.
    async fn trees_held(
        &self,
        laid: &[Laid<'_>],
        manifest: &TaskManifest,
        places: &BTreeMap<Digest, Place>,
    ) -> Step<()> {
        let trees: BTreeSet<Digest> = manifest
            .output_directories
            .iter()
            .map(|dir| dir.tree)
            .filter(|tree| tree.size_bytes != 0)
            .collect();
        let mut files = Closure::new();
        for tree in &trees {
            let Some(&Place::Pack { at, entry }) = places.get(tree) else {
                return Err(RejectReason::Incomplete.into());
            };
            let range = entry.range();
            let object = &laid[at].key.object;
            let record = self
                .read(object, range.start, range.end - range.start)
                .await?;
            entry
                .verify(&record)
                .map_err(|_| RejectReason::HashMismatch)?;
            closure::add_tree(&mut files, tree, record).map_err(|_| RejectReason::BadManifest)?;
        }
        if files.iter().any(|digest| !places.contains_key(digest)) {
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

/// The slot a key names, if it is a task pack key.
fn slot_of(key: &IssuedPackKey) -> Option<Slot> {
    TaskPackKey::parse(key.key()).ok().map(|key| key.slot())
}

/// Where each digest of the run is: a record of a pack, or a large object. A digest in two places
/// (two records of two packs, or a record and a large object) is `Malformed`.
fn places(laid: &[Laid<'_>], manifest: &TaskManifest) -> Step<BTreeMap<Digest, Place>> {
    let mut places = BTreeMap::new();
    let records = laid.iter().enumerate().flat_map(|(at, pack)| {
        let entries = pack.footer.entries().iter();
        entries.map(move |entry| (entry.digest, Place::Pack { at, entry: *entry }))
    });
    let larges = manifest
        .large
        .iter()
        .map(|large| (large.digest, Place::Large));
    for (digest, place) in records.chain(larges) {
        if places.insert(digest, place).is_some() {
            return Err(RejectReason::Malformed.into());
        }
    }
    Ok(places)
}

pub(super) fn unavailable(message: &str) -> Stop {
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
/// digests, which the pack's records have been hashed to, and the task's claimed timing as
/// execution metadata ([`claimed`]), anchored at `end`.
fn rebuilt(manifest: &TaskManifest, end: SystemTime) -> ActionResult {
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
        execution_metadata: claimed(&manifest.timing, end),
        ..ActionResult::default()
    };
    result.output_files.sort_by(|a, b| a.path.cmp(&b.path));
    result
        .output_directories
        .sort_by(|a, b| a.path.cmp(&b.path));
    result.output_symlinks.sort_by(|a, b| a.path.cmp(&b.path));
    result
}
