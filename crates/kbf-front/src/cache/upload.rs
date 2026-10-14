//! What the `Segments` service asks of the cache (`docs/design/write-path.md`, section
//! 4): storing segments the server built from a daemon's checked records under one
//! commit, locating blobs, and reading ranges of stored objects.

use bytes::Bytes;
use futures::future::try_join_all;
use kbf_meta::{BlobAnswer, Location, ObjectId, StoreId};
use kbf_objstore::{ByteRange, ObjectStore, ObjectStoreError};
use kbf_segments::IndexEntry;
use kbf_types::Digest;

use super::{Cache, CacheError};
use crate::meta_log::MetaLog;

/// One segment ready to store: its whole bytes (records, then a footer the server
/// built from `entries`) and the entries that footer indexes.
#[derive(Debug)]
pub(crate) struct BuiltSegment {
    pub body: Bytes,
    pub entries: Vec<IndexEntry>,
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Stores every segment of an upload (each its own object), reads each footer
    /// back, and then commits **one** [`PutBlobs`](kbf_meta::Command::PutBlobs)
    /// listing every record of every segment. Returns the digests committed, in
    /// segment then index order.
    ///
    /// Each segment is stored as a grouped segment is (`Writer::store_segment`): under
    /// a `--write-puts-in-flight` slot, its PUT retried under a new key, its footer read
    /// back; the commit is timed as a segment commit.
    ///
    /// Nothing is indexed unless every PUT and read-back succeeded: a failure leaves
    /// the objects written so far as orphans, unreferenced.
    ///
    /// # Errors
    /// The store or the metadata log failed; a footer read back without a record the
    /// server put in it ([`CacheError::Internal`]).
    pub(crate) async fn store_upload(
        &self,
        segments: Vec<BuiltSegment>,
    ) -> Result<Vec<Digest>, CacheError> {
        let placed = try_join_all(segments.into_iter().map(|s| self.put_built(s))).await?;
        let placed: Vec<(Digest, Location)> = placed.into_iter().flatten().collect();
        let digests = placed.iter().map(|(d, _)| *d).collect();
        self.writer.commit_segment(placed).await?;
        Ok(digests)
    }

    /// Writes one built segment as a new object and returns where each of its records
    /// is, as the footer read back from the store says.
    async fn put_built(
        &self,
        segment: BuiltSegment,
    ) -> Result<Vec<(Digest, Location)>, CacheError> {
        let (object, key, footer) = self
            .writer
            .store_segment(&self.group, segment.body, segment.entries.len())
            .await?;
        segment
            .entries
            .iter()
            .map(|entry| {
                let found = footer
                    .find(&entry.digest)
                    .filter(|f| f.offset == entry.offset);
                let found = found.ok_or_else(|| {
                    CacheError::Internal(format!(
                        "segment {key} came back without blob {} at {}",
                        entry.digest, entry.offset
                    ))
                })?;
                Ok((
                    entry.digest,
                    Location {
                        store: StoreId::CONFIGURED,
                        object,
                        offset: found.offset,
                    },
                ))
            })
            .collect()
    }

    /// Where each of `digests` is, in order: the object and offset of a blob present
    /// and reachable, `None` for any other (the empty blob included: it is in no
    /// object). The present ones are touched before the answer, as a read is, so a
    /// fill that reads them next finds them.
    ///
    /// # Errors
    /// The metadata log is unavailable or kept changing.
    pub(crate) async fn locate(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Option<Location>>, CacheError> {
        self.touched(|s| {
            let found: Vec<Option<Location>> = digests
                .iter()
                .map(|d| match s.blob(d) {
                    BlobAnswer::Present { location, .. } => Some(location),
                    BlobAnswer::Unavailable | BlobAnswer::Absent => None,
                })
                .collect();
            let present = digests.iter().zip(&found).filter_map(|(d, l)| l.map(|_| d));
            let touch = s.touch_for(present);
            (found, touch)
        })
        .await
    }

    /// `len` bytes of object `object` from `offset`, unchecked: the caller checks each
    /// blob it cuts from them. `None` when the store does not hold the whole range (no
    /// such object, or one too short), which the service answers NOT_FOUND.
    ///
    /// # Errors
    /// The store failed in another way, or the key could not be built.
    pub(crate) async fn read_object_range(
        &self,
        object: ObjectId,
        offset: u64,
        len: u64,
    ) -> Result<Option<Bytes>, CacheError> {
        let key = self.object_key(object)?;
        let Some(range) = ByteRange::new(offset, len) else {
            return Ok(None);
        };
        match self.writer.get_range(&key, range).await {
            Ok(bytes) if bytes.len() as u64 == len => Ok(Some(bytes)),
            Ok(_) | Err(ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. }) => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}
