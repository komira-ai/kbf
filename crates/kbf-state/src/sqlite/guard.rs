//! Red-first stub: the bodies land with the implementation.

use std::io;
use std::path::{Path, PathBuf};

pub(super) fn lock_path(_db: &Path) -> PathBuf {
    todo!("av2-03: lock_path")
}

pub(super) fn network_filesystem(_magic: u32) -> Option<&'static str> {
    todo!("av2-03: network_filesystem")
}

pub(super) fn filesystem_type(_dir: &Path) -> io::Result<u32> {
    todo!("av2-03: filesystem_type")
}
