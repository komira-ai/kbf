//! The action cache of the in-memory index: a map beside the pack maps, changed under the same
//! lock, so a put is atomic as the Postgres transaction is.

use super::{MemoryPackIndex, record_in};
use crate::action::{ActionEntry, ActionPut, ActionStore, check_result, in_key_order};
use crate::{BlobDigest, NewPack, PackIndexError, Recorded, micros_of};

impl ActionStore for MemoryPackIndex {
    fn get_action(&self, action: &BlobDigest) -> Result<Option<ActionEntry>, PackIndexError> {
        let state = self.lock();
        let found = state.actions.get(action);
        Ok(found.map(|(result, written_us)| ActionEntry {
            result: result.clone(),
            written_us: *written_us,
        }))
    }

    fn put_action(
        &self,
        action: &BlobDigest,
        result: &[u8],
        packs: &[NewPack],
    ) -> Result<ActionPut, PackIndexError> {
        check_result(result)?;
        let mut checked = Vec::with_capacity(packs.len());
        for (place, pack) in in_key_order(packs) {
            checked.push((place, pack, pack.checked()?));
        }
        let now = micros_of(self.clock.now());
        let mut guard = self.lock();
        // The packs are recorded on a copy that replaces the state only when every one was
        // taken and the row is written: a refusal half way leaves nothing behind.
        let mut next = guard.clone();
        let mut recorded = vec![None::<Recorded>; packs.len()];
        for (place, pack, blobs) in &checked {
            recorded[*place] = Some(record_in(&mut next, pack, blobs, now)?);
        }
        let replaced = next
            .actions
            .insert(*action, (result.to_vec(), now))
            .is_some();
        *guard = next;
        Ok(ActionPut {
            replaced,
            packs: recorded.into_iter().flatten().collect(),
        })
    }
}
