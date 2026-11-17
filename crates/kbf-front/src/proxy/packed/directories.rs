//! The Directory rows of the packs' index: [`Packs::directories`] reads the bytes of a level of
//! Directories with one call of the index per [`MAX_BATCH`] digests, and
//! [`Packs::record_directories`] records the ones a walk had to read from their blobs, so that
//! the next walk of the same tree, on this frontend or another over the same database, reads
//! no Directory from the object store. The rules of the rows are `kbf_pack_index::directory`'s.
//!
//! An index that keeps no Directory rows ([`PackIndexError::Unsupported`]) answers every digest
//! as having none and records nothing: the walk reads each Directory from its blob, as it did
//! before the rows existed.

use bytes::Bytes;
use kbf_pack_index::directory::{MAX_DIRECTORY_BYTES, NewDirectory};
use kbf_pack_index::{BlobDigest, MAX_BATCH, PackIndexError};
use kbf_types::Digest;
use tonic::Status;

use super::store::{Packs, offload, to_index};
use std::sync::Arc;

impl Packs {
    /// For each of `asked`, in order, the bytes of the Directory recorded under it, or `None`
    /// when it has no row (or the index keeps no Directory rows). Nothing is read from a pack.
    ///
    /// # Errors
    /// UNAVAILABLE when the index fails.
    pub async fn directories(&self, asked: &[Digest]) -> Result<Vec<Option<Bytes>>, Status> {
        let mut found = Vec::with_capacity(asked.len());
        for chunk in asked.chunks(MAX_BATCH) {
            let wanted: Vec<BlobDigest> = chunk.iter().map(to_index).collect();
            let index = Arc::clone(&self.index);
            match offload(move || index.directories(&wanted)).await {
                Ok(bodies) => found.extend(bodies.into_iter().map(|b| b.map(Bytes::from))),
                Err(PackIndexError::Unsupported) => return Ok(vec![None; asked.len()]),
                Err(error) => return Err(rows_failed(&error)),
            }
        }
        Ok(found)
    }

    /// Records each of `directories` that has no row, in calls of at most [`MAX_BATCH`], and
    /// returns how many rows were inserted. A Directory longer than [`MAX_DIRECTORY_BYTES`] is
    /// left out (a walk keeps reading it from its blob); the caller passes bytes it read and
    /// hashed as the digest, which the index checks again.
    ///
    /// # Errors
    /// UNAVAILABLE when the index fails, or refuses a body as not its digest; the calls before
    /// the failure stay recorded.
    pub async fn record_directories(
        &self,
        directories: Vec<(Digest, Bytes)>,
    ) -> Result<usize, Status> {
        let rows: Vec<NewDirectory> = directories
            .into_iter()
            .filter(|(_, body)| body.len() <= MAX_DIRECTORY_BYTES)
            .map(|(digest, body)| NewDirectory {
                digest: to_index(&digest),
                body: body.to_vec(),
            })
            .collect();
        let mut inserted = 0;
        for chunk in rows.chunks(MAX_BATCH) {
            let chunk = chunk.to_vec();
            let index = Arc::clone(&self.index);
            match offload(move || index.record_directories(&chunk)).await {
                Ok(n) => inserted += n,
                Err(PackIndexError::Unsupported) => return Ok(0),
                Err(error) => return Err(rows_failed(&error)),
            }
        }
        Ok(inserted)
    }
}

/// UNAVAILABLE for an index whose Directory rows could not be read or written.
fn rows_failed(error: &PackIndexError) -> Status {
    Status::unavailable(format!("pack index, directory rows: {error}"))
}
