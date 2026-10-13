//! The adopt pass (`docs/design/worker-cache.md#69-the-index`): files on disk that the
//! index does not list are added to it, unverified; entries whose file is gone are
//! dropped. It never deletes a file and never waits on a lease.
//!
//! The keys fills left unindexed (an outside file at the rename) come first, then
//! every `blobs/<h0h1>/` directory in order, at most one a [`CacheConfig::pass_pace`]
//! unless free space is under the floor with nothing left to evict. For each directory
//! it lists the names and compares them with the index; then, per key it would change,
//! it takes the key's slot, skipping a key someone holds, and re-checks the index
//! under it, since the listing is already stale:
//!
//! - a regular file of the key's size the index does not list is added, its last use
//!   its `st_ctime` (set by the rename and by each link), its bytes counted;
//! - an entry with no listed file is dropped only if `fstatat` says `ENOENT`;
//! - a name that is not a key, or a file that is not a regular file of the key's size,
//!   is left alone and counted in `foreign_files`; its bytes are never in the total.
//!
//! [`CacheConfig::pass_pace`]: super::CacheConfig::pass_pace

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt as _;

use rustix::fs::{AtFlags, CWD, FileType};

use super::layout::Key;
use super::{Inner, Point, bump};

/// What one pass changed.
#[derive(Default)]
struct Tally {
    added: u64,
    dropped: u64,
    foreign: u64,
}

impl Inner {
    /// One adopt pass; whether it completed. A pass cut short by a stop sets no
    /// counter of a whole pass.
    pub(super) fn pass(&self) -> bool {
        let mut tally = Tally::default();
        let recorded = std::mem::take(&mut self.state().unindexed);
        for key in recorded {
            self.reconcile(key, &mut tally);
        }
        for prefix in 0..=u8::MAX {
            if self.stopping() {
                return false;
            }
            self.pass_directory(prefix, &mut tally);
            let pressure = self.state().pressure;
            if !pressure && self.wait_stopped(self.config.pass_pace) {
                return false;
            }
        }
        let Tally {
            added,
            dropped,
            foreign,
        } = tally;
        let c = &self.counters;
        c.foreign
            .store(foreign, std::sync::atomic::Ordering::Relaxed);
        c.drift
            .store(added + dropped, std::sync::atomic::Ordering::Relaxed);
        tracing::info!("cache: pass done: +{added} B, -{dropped} B, {foreign} foreign files");
        true
    }

    fn pass_directory(&self, prefix: u8, tally: &mut Tally) {
        let dir = self.blobs.join(hex::encode([prefix]));
        let mut on_disk = BTreeSet::new();
        // A directory that cannot be read lists nothing, so its entries stay as they are.
        let Ok(entries) = std::fs::read_dir(&dir) else {
            tracing::warn!("cache: cannot list {}", dir.display());
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let key = std::str::from_utf8(name.as_bytes())
                .ok()
                .and_then(Key::parse);
            match key.filter(|key| key.hash[0] == prefix) {
                Some(key) => _ = on_disk.insert(key),
                None => {
                    let (name, dir) = (name.to_string_lossy(), dir.display());
                    tracing::warn!("cache: {name} in {dir} is not an entry");
                    tally.foreign += 1;
                }
            }
        }
        self.config.hooks.at(Point::Listed(prefix));
        let indexed: BTreeSet<Key> = self.state().index.under(prefix).into_iter().collect();
        for key in on_disk.symmetric_difference(&indexed) {
            self.reconcile(*key, tally);
        }
    }

    /// Brings `key`'s entry in line with its file, under the key's slot, by what the
    /// index says now (the listing is stale): a regular file of the key's size the
    /// index does not list is added, and an entry whose file is gone (`ENOENT`) is
    /// dropped. A key someone holds is left to them.
    fn reconcile(&self, key: Key, tally: &mut Tally) {
        self.config.hooks.at(Point::Compared(&key));
        let mut state = self.state();
        if state.slots.contains_key(&key) {
            return;
        }
        let indexed = state.index.get(&key).is_some();
        let slot = self.take(&mut state, key);
        drop(state);
        let stat = rustix::fs::statat(CWD, self.path_of(&key), AtFlags::SYMLINK_NOFOLLOW);
        match (indexed, stat) {
            (false, Ok(stat))
                if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                    && u64::try_from(stat.st_size) == Ok(key.size) =>
            {
                let last_use = u64::try_from(stat.st_ctime).unwrap_or(0);
                self.state().index.insert(key, last_use, false);
                bump(&self.counters.adopted);
                tally.added += key.size;
            }
            (false, Ok(_)) => tally.foreign += 1,
            (true, Err(rustix::io::Errno::NOENT)) => {
                self.state().index.remove(&key);
                bump(&self.counters.dropped);
                tally.dropped += key.size;
            }
            // Indexed with its file, or neither.
            _ => {}
        }
        drop(slot);
    }
}
