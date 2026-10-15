//! `clonefile` (macOS): a copy-on-write copy of an entry at a lease's name.

use std::io;
use std::path::Path;

use rustix::fs::{CWD, CloneFlags, fclonefileat};

/// Clones the file at `from` to the new name `to`: APFS shares the data blocks until
/// either file is written, and the clone is its own inode (so it has its own mode and
/// owner and a write to it never reaches `from`). Fails with `EEXIST` when `to` exists,
/// `ENOENT` when `from` or `to`'s directory is gone, and `EXDEV` or `ENOTSUP` when the
/// two are not on one APFS volume.
pub(super) fn clone_file(from: &Path, to: &Path) -> io::Result<()> {
    let source = std::fs::File::open(from)?;
    fclonefileat(&source, CWD, to, CloneFlags::empty())?;
    Ok(())
}
