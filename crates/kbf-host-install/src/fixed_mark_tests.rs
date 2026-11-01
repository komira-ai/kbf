//! What a test of the fixed files compares: a file's inode, its times of change and of
//! modification, its mode and its bytes. A write of the same bytes, a `chmod` or a replacement by
//! a rename each changes one of them, so a file whose mark is the same was not written.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The mark of one file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    inode: u64,
    changed: (i64, i64),
    modified: (i64, i64),
    mode: u32,
    bytes: Vec<u8>,
}

/// The mark of the file at `path`.
pub(crate) fn mark(path: &Path) -> Mark {
    let meta = fs::symlink_metadata(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Mark {
        inode: meta.ino(),
        changed: (meta.ctime(), meta.ctime_nsec()),
        modified: (meta.mtime(), meta.mtime_nsec()),
        mode: meta.mode(),
        bytes: fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    }
}
