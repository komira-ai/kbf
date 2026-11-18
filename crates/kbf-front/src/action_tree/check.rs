//! The objects of an action tree checked against the store, for a tree written after the store
//! lost an object the frontend's state said it had ([`TreeWriter::write_checked`]).
//!
//! The pack index says where each blob is, and nothing on the usual path asks the store whether
//! that pack is still there: a store is general-purpose, and its objects can vanish (a store
//! swapped for an empty one, a bucket emptied). A job that read a lost manifest says so, and its
//! next tree is checked: every pack the tree names is asked for its length once, at most
//! [`HEADS_IN_FLIGHT`] at once, and a pack the store does not have, or has shorter than a blob
//! in it ends, is forgotten by the pack index ([`BlobSource::forget_lost`]). Its blobs are then
//! missing, which FindMissingBlobs answers too, so the client uploads them again; the tree is
//! refused naming them ([`NoTree::Lost`]), and both memos are dropped whole: the place memo may
//! place other blobs in the packs forgotten, and a remembered manifest may name them as a
//! file's source, which neither memo can be asked about.

use std::collections::{BTreeMap, BTreeSet};

use futures::stream::{self, StreamExt as _};
use kbf_types::Digest;

use super::place::{HEADS_IN_FLIGHT, Place};
use super::{NoTree, TreeWriter};

impl TreeWriter {
    /// Checks every pack `places` names against the store, and has the index forget each the
    /// store lacks.
    ///
    /// # Errors
    /// [`NoTree::Lost`] naming every blob of the packs forgotten; [`NoTree::Lookup`] when the
    /// store or the index could not answer.
    pub(super) async fn check_objects(
        &self,
        places: &BTreeMap<Digest, Place>,
    ) -> Result<(), NoTree> {
        // For each pack, the end of the furthest blob in it the tree names.
        let mut ends: BTreeMap<String, u64> = BTreeMap::new();
        for (digest, place) in places {
            if let Place::Packed { key, offset } = place {
                let end = offset.saturating_add(digest.size_bytes);
                let furthest = ends.entry(key.clone()).or_default();
                *furthest = (*furthest).max(end);
            }
        }
        let source = &self.source;
        let answers: Vec<(String, _)> = stream::iter(ends)
            .map(|(key, end)| async move {
                let answer = source.forget_lost(&key, end).await;
                (key, answer)
            })
            .buffer_unordered(HEADS_IN_FLIGHT)
            .collect()
            .await;
        let mut forgotten: BTreeSet<String> = BTreeSet::new();
        for (key, answer) in answers {
            let gone = answer.map_err(|e| NoTree::Lookup { why: e.to_string() })?;
            if gone {
                forgotten.insert(key);
            }
        }
        let lost: Vec<Digest> = (places.iter())
            .filter(
                |(_, place)| matches!(place, Place::Packed { key, .. } if forgotten.contains(key)),
            )
            .map(|(digest, _)| *digest)
            .collect();
        if lost.is_empty() {
            return Ok(());
        }
        self.remembered().clear();
        self.lock().clear();
        tracing::warn!(
            packs = forgotten.len(),
            blobs = lost.len(),
            "an action tree's check found packs the store lost: the index forgot them"
        );
        Err(NoTree::Lost { blobs: lost })
    }
}
