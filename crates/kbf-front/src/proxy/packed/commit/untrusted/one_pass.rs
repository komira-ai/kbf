//! Ingest of a one-pass object (`kbf_pack::one_pass`, docs/design/one-pass-task-object.md): the
//! one object a put-directory task writes with `--object-format one-pass`, in place of packs and
//! large objects. It is an uncompressed ZIP64 archive. [`Ingest::run`] hands it here when the end
//! of pack 0 is the format's end records.
//!
//! What it reads: the last 138 bytes (the end records), then once from the manifest member to
//! the end (the manifest, the file index and the central directory), and no record. In order,
//! each step refusing before the next reads:
//!
//! 1. the end records' offsets must describe the object's length, and the tail one range holds
//!    must be within [`MAX_UNTRUSTED_MANIFEST_BYTES`] and [`UntrustedLimits::max_object_entries`]
//!    records (`Malformed`, `TooLarge`), before it is read;
//! 2. the members' CRC-32s, that every entry is stored (an entry of any other method is
//!    `Malformed`), has its SHA-256 and lies inside the records, and none overlaps (`Malformed`);
//! 3. the manifest, for this lease and action, naming one pack and no large object
//!    (`BadManifest`, `WrongKey`), and only paths the Command declares ([`Ingest::declared`]);
//! 4. every digest the manifest names (the empty one aside) is a record (`Incomplete`);
//! 5. the object is recorded in the pack index with a row for every record, a blob over
//!    [`Packs::max_blob_bytes`] included: the row says where it is, and a read of one, from a
//!    frontend that serves large blobs from the index, is a ranged read of this object;
//! 6. a frontend whose reads of a large blob go to storage nodes has each such blob copied to
//!    them by a ranged read of the object ([`Large::at`]); where the index serves it, the
//!    copy finds the row and reads nothing;
//! 7. the result, written to the action cache last, as in [`Ingest::run`].
//!
//! The proof is [`Proof::Manifest`]'s: a record that is not its digest is dropped by the first
//! read that hashes it.

use kbf_pack::TaskManifest;
use kbf_pack::one_pass::{Entry, Tail, Trailer};
use kbf_types::Digest;
use std::collections::BTreeMap;

use kbf_pack_index::{BlobDigest, NewPack, PackBlob};

use super::IssuedPackKey;
use super::large::Large;
use super::task::{Ingest, Ran, RejectReason, Step, Stop, rebuilt};
use crate::output_index::{DerivedTree, OutputIndex};

/// The length of the end of a one-pass object, which is how many bytes of the end of pack 0
/// [`Ingest::run`] reads to tell the formats apart.
pub(super) const TAIL_LEN: usize = kbf_pack::one_pass::TAIL_LEN;

/// Whether `end`, the last bytes of an object, is the end of a one-pass object.
pub(super) fn is_one_pass(end: &[u8]) -> bool {
    kbf_pack::one_pass::looks_like(end)
}

impl Ingest<'_> {
    /// Ingests the one-pass object `first` of `size` bytes, whose last bytes are `end`.
    pub(super) async fn one_pass(&self, first: &IssuedPackKey, size: u64, end: &[u8]) -> Step<Ran> {
        let malformed = |_| RejectReason::Malformed;
        let trailer = Trailer::parse(end, size).map_err(malformed)?;
        if trailer.manifest_len > self.limits.max_object_manifest_bytes {
            return Err(RejectReason::TooLarge.into());
        }
        let max_entries = u64::try_from(self.limits.max_object_entries).unwrap_or(u64::MAX);
        if trailer.entry_count > max_entries {
            return Err(RejectReason::TooLarge.into());
        }
        // What one range may hold: the manifest, and for each record at most this many bytes of
        // its central directory entry and its line of the file index.
        let len = trailer.tail_len();
        let per_entry = self.limits.object_bytes_per_entry;
        let most = (self.limits.max_object_manifest_bytes)
            .saturating_add(max_entries.saturating_mul(per_entry));
        if len > most {
            return Err(RejectReason::TooLarge.into());
        }
        let bytes = self.read(&first.object, size - len, len).await?;
        let tail = Tail::parse(&trailer, &bytes).map_err(malformed)?;

        let manifest = TaskManifest::parse_with(&tail.manifest, self.limits.max_large_bytes)
            .map_err(|_| RejectReason::BadManifest)?;
        let ours = [
            manifest.lease == self.lease,
            manifest.action == *self.action,
        ];
        if !ours.iter().all(|ok| *ok) {
            return Err(RejectReason::WrongKey.into());
        }
        self.issued(&manifest)?;
        if manifest.packs != 1 || !manifest.large.is_empty() {
            return Err(RejectReason::BadManifest.into());
        }
        self.declared(&manifest).await?;
        let held = |digest: &kbf_types::Digest| {
            tail.entries
                .binary_search_by(|entry| entry.digest.cmp(digest))
                .is_ok()
        };
        // `output_digests` leaves out the empty blob, which no object holds.
        if (manifest.output_digests()).any(|digest| !held(&digest)) {
            return Err(RejectReason::Incomplete.into());
        }
        let (index, trees) = agreed(&manifest, &tail.files)?;
        if index.digests().iter().any(|digest| !held(digest)) {
            return Err(RejectReason::Incomplete.into());
        }

        self.packs
            .record_put_dir_pack(first, new_pack(first, &tail.entries, size))
            .await?;
        let cap = self.packs.max_blob_bytes();
        let mut copied = 0;
        for entry in tail.entries.iter().filter(|e| e.digest.size_bytes > cap) {
            Large::at(self.packs, first, entry.offset, entry.digest)
                .store(self.backend)
                .await?;
            copied += entry.digest.size_bytes;
        }
        if let Some(writer) = self.publish {
            let spans: BTreeMap<Digest, u64> =
                tail.entries.iter().map(|e| (e.digest, e.offset)).collect();
            for tree in &trees {
                let key = first.key();
                let published = writer
                    .publish_output(&tree.root, &tree.directories, key, &spans)
                    .await;
                if let Err(error) = published {
                    tracing::warn!(%error, path = %tree.path, "an output directory's manifest was not published");
                }
            }
        }
        let result = rebuilt(&manifest, self.packs.now());
        let cached = self.cached(&result, manifest.exit_code).await?;
        Ok(Ran {
            result,
            cached,
            packs: 1,
            large_bytes: copied,
        })
    }
}

/// Checks the file index against the manifest, and derives the output directories' trees from it:
/// every output file and symlink of the manifest is in the index as the manifest says, and the
/// `Tree` derived for each output directory has the digest the manifest names. An object with no
/// output has no index to check.
fn agreed(manifest: &TaskManifest, bytes: &[u8]) -> Step<(OutputIndex, Vec<DerivedTree>)> {
    let bad = || Stop::from(RejectReason::BadManifest);
    let outputs = [
        manifest.output_files.is_empty(),
        manifest.output_directories.is_empty(),
        manifest.output_symlinks.is_empty(),
    ];
    if outputs.iter().all(|none| *none) {
        return Ok((OutputIndex::default(), Vec::new()));
    }
    let index = OutputIndex::parse(bytes).map_err(|_| bad())?;
    let files = manifest.output_files.iter().all(|file| {
        index.file(&file.path).map(|f| (f.digest, f.exec))
            == Some((file.digest, file.is_executable))
    });
    let links = (manifest.output_symlinks.iter())
        .all(|link| index.link(&link.path) == Some(link.target.as_str()));
    let mut trees = Vec::with_capacity(manifest.output_directories.len());
    for dir in &manifest.output_directories {
        let tree = index.tree(&dir.path).map_err(|_| bad())?;
        if tree.digest != dir.tree {
            return Err(bad());
        }
        trees.push(tree);
    }
    if !files || !links {
        return Err(bad());
    }
    Ok((index, trees))
}

/// The pack index's record of the object: a row for each entry of its index.
fn new_pack(key: &IssuedPackKey, entries: &[Entry], size: u64) -> NewPack {
    let blob = |entry: &Entry| PackBlob {
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
        blobs: entries.iter().map(blob).collect(),
    }
}
