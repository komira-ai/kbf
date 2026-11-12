//! Ingest of the run a task committed into a put directory: [`ingest_put_dir`].
//!
//! The task writes its objects as files into the directory the frontend minted for the run,
//! `cas/b/<lease>/<nonce>` ([`IssuedPutDir`]), and the node's store commits each by a rename
//! that returns only once the object store holds it (docs/design/jobs-on-jobscheduler-and-tierstore.md,
//! sections 2 and 3). The ingest is [`super::ingest_set`]'s over every key the directory can hold
//! (`pack` to `pack7`, `L0` to `L7`), with a lighter proof ([`Proof::Manifest`]):
//!
//! - **What it reads.** Pack 0's length, trailer, footer and record 0 (the manifest, checked
//!   against its CRC-32C and SHA-256), the footer of each other pack the manifest names, and the
//!   length of each large object. No other record of a pack is read: neither the output trees
//!   nor the blobs.
//! - **What it adds.** The manifest's outputs must be paths the Command declares (its
//!   `output_paths`, or its `output_files` and `output_directories` when it has none): a path
//!   it does not declare is `BadManifest`. The Action and the Command are read through the
//!   backend for it.
//! - **How the packs are recorded.** Each pack's footer is recorded in the pack index as it
//!   is, written by `b/<lease>` ([`Packs::record_put_dir_pack`]), packs `1..` first and pack 0
//!   last. The index keeps the first location recorded for a digest; a later pack that holds
//!   the same digest gets no row for it.
//! - **Large objects** are as in [`super::ingest_set`]: each is streamed once through SHA-256
//!   into the CAS, since a blob over the record cap is never read from a pack location.
//!
//! What the lighter proof gives up: a record whose bytes are not its digest is recorded at its
//! offset all the same. The first read of it hashes it (as every read from a pack does) and
//! the reader drops that location, so a lying pack costs its own action's outputs and one
//! failed read, and never moves a digest that another pack recorded first (design, section 7).

use kbf_pack::TaskManifest;
use kbf_pack_index::{NewPack, Recorded};
use kbf_proto::reapi;
use kbf_types::Digest;
use prost::Message as _;

use super::task::{Ingest, Ingested, Proof, RejectReason, Step, ended, unavailable};
use super::{CommitError, IssuedPackKey, IssuedPutDir, PackKeyMint, Packs, UntrustedLimits};
use crate::backend::Backend;
use crate::proxy::packed::commit::{offload, recording_failed};
use crate::wire::digest;

/// Reads, checks and records the run a task committed into the put directory `dir` for
/// `action`, on the lease `lease`; the module docs give the steps. `dir` and `lease` come from
/// the frontend's own record of the run, never from the task. `mint` is the frontend's, and
/// `limits` bound the run.
///
/// The keys are held, refused and counted as [`super::ingest_set`]'s are.
pub async fn ingest_put_dir(
    packs: &Packs,
    backend: &Backend,
    mint: &PackKeyMint,
    lease: &str,
    dir: &IssuedPutDir,
    action: &Digest,
    limits: &UntrustedLimits,
) -> Ingested {
    let keys = dir.key_set();
    let done = Ingest {
        packs,
        backend,
        mint,
        lease,
        keys: &keys,
        action,
        limits,
        proof: Proof::Manifest,
    };
    let ingested = ended(packs, done).await;
    if matches!(ingested, Ingested::Ran { .. }) {
        packs.metrics.ingest_put_dirs.add(1);
    }
    ingested
}

impl Ingest<'_> {
    /// Checks that every output the manifest names is a path the Command declares.
    #[allow(deprecated)] // `output_files` and `output_directories`: REAPI before 2.1 sends only these.
    pub(super) async fn declared(&self, manifest: &TaskManifest) -> Step<()> {
        let bad = || RejectReason::BadManifest.into();
        let action = self.action().await?.ok_or_else(bad)?;
        let wire = action.command_digest.as_ref().ok_or_else(bad)?;
        let command = digest(Some(wire)).map_err(|_| bad())?;
        let blob = self.backend.read_blob(&command).await;
        let blob = blob.map_err(|error| unavailable(&format!("the Command: {error}")))?;
        let command = reapi::Command::decode(blob).map_err(|_| bad())?;
        let declared: Vec<&String> = if command.output_paths.is_empty() {
            let files = command.output_files.iter();
            files.chain(&command.output_directories).collect()
        } else {
            command.output_paths.iter().collect()
        };
        let paths = (manifest.output_files.iter().map(|file| &file.path))
            .chain(manifest.output_directories.iter().map(|dir| &dir.path))
            .chain(manifest.output_symlinks.iter().map(|link| &link.path));
        for path in paths {
            if !declared.contains(&path) {
                return Err(bad());
            }
        }
        Ok(())
    }
}

impl Packs {
    /// Records `pack`, the footer of a pack a build committed under `issued` in its put
    /// directory, as written by `b/<build>`, without reading a record of it: the first location
    /// recorded for a digest is kept, and a read of a record hashes it. Idempotent on the key:
    /// the same contents are answered `created: false`, others [`CommitError::Conflict`].
    ///
    /// # Errors
    /// [`CommitError`]: the index refuses the pack, or it failed.
    pub(super) async fn record_put_dir_pack(
        &self,
        issued: &IssuedPackKey,
        pack: NewPack,
    ) -> Result<Recorded, CommitError> {
        let recorded = NewPack {
            writer: format!("b/{}", issued.build()),
            ..pack
        };
        let index = std::sync::Arc::clone(&self.index);
        let done = offload(move || index.record_pack(&recorded)).await;
        let recorded = done.map_err(recording_failed)?;
        if recorded.created {
            self.metrics.put_dir_records.add(1);
        }
        Ok(recorded)
    }
}
