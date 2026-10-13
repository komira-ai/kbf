//! The checks an entry passes before a lease uses it
//! (`docs/design/worker-cache.md#63-integrity`): a stat check on every hit, and a
//! re-hash on its first use after each start (lazy verify).
//!
//! The stat check catches accidents (a mode or owner changed by hand, a truncated
//! file), not a writer with the daemon's uid, who can restore every field: entries are
//! kept out of every action's reach by construction (the container driver's overlay
//! and unmapped owner), not by this check.

use std::io::Read as _;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, FileType};
use sha2::{Digest as _, Sha256};

use super::layout::Key;

/// What is at a key's final name.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Found {
    /// Nothing.
    Absent,
    /// The key's entry, with this many links: it passed the stat check and hashes to
    /// the key.
    Good(u64),
    /// Something that is not the key's entry.
    Bad,
}

/// The stat check: `path` is a regular file of `uid`'s with the key's mode and size.
/// Its link count if it passes, what was found if not.
pub(super) fn stat_check(path: &Path, key: &Key, uid: u32) -> Result<u64, Found> {
    let stat = match rustix::fs::statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Err(Found::Absent),
        Err(_) => return Err(Found::Bad),
    };
    let regular = FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile;
    let fits = regular && stat.st_uid == uid && stat.st_mode & 0o7777 == key.mode();
    if fits && u64::try_from(stat.st_size) == Ok(key.size) {
        Ok(stat.st_nlink)
    } else {
        Err(Found::Bad)
    }
}

/// The stat check, then the re-hash.
pub(super) fn inspect(path: &Path, key: &Key, uid: u32) -> Found {
    match stat_check(path, key, uid) {
        Ok(nlink) if hash_file(path).is_ok_and(|hash| hash == key.hash) => Found::Good(nlink),
        Ok(_) => Found::Bad,
        Err(found) => found,
    }
}

/// The SHA-256 of the file at `path`, read a MiB at a time.
fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..read]);
    }
}
