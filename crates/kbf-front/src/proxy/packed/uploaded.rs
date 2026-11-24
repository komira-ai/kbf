//! The Directories clients upload into packs, learnt as they are stored, so that the Execute
//! that names them reads none of them back (`known_dirs` has the rules).
//!
//! Each write into packs ([`Packs::put_in`]: BatchUpdateBlobs, and a ByteStream Write small
//! enough to pack) gives the blobs that may be Directories to [`KnownDirs::uploaded`] once the
//! packs holding them are recorded, before the upload is answered: a client sends the Execute
//! that names them after that answer, and its walk finds them.

use std::sync::Arc;

use bytes::Bytes;
use kbf_types::Digest;

use super::store::Packs;
use crate::known_dirs::{KnownDirs, may_be_directory};

/// Of `blobs`, the ones that may be Directories ([`may_be_directory`]), for
/// [`Packs::learn_uploaded`] once they are stored.
pub(super) fn maybe_directories(blobs: &[(Digest, Bytes)]) -> Vec<(Digest, Bytes)> {
    blobs
        .iter()
        .filter(|(digest, bytes)| may_be_directory(digest, bytes))
        .cloned()
        .collect()
}

impl Packs {
    /// What the input walks over these packs know of Directories by content, the ones clients
    /// upload through them included.
    pub(crate) fn known_dirs(&self) -> &Arc<KnownDirs> {
        &self.known
    }

    /// `uploaded` are stored and recorded: the ones that are Directories not known yet are put
    /// in the walks' caches.
    pub(super) fn learn_uploaded(&self, uploaded: Vec<(Digest, Bytes)>) {
        let learnt = uploaded
            .iter()
            .filter(|(digest, bytes)| self.known.uploaded(digest, bytes))
            .count();
        self.metrics.uploaded_directories.add(learnt as u64);
    }
}
