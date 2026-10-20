//! `raise::replace`: the floors file is replaced whole.

use std::io;
use std::path::Path;

use kbf_coverage::raise::{replace, replace_with};

/// Catches: a write that leaves part of a file, or a temporary file left behind
/// after a failed rename.
#[test]
fn replace_is_whole_and_leaves_no_temporary_file() {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("replace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("coverage-floors");
    std::fs::write(&path, "old").expect("write");
    replace(&path, "new").expect("replace");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "new");
    let err = replace_with(&path, "newer", |_, _| Err(io::Error::other("no")));
    assert!(err.is_err());
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "new");
    assert!(!dir.join("coverage-floors.raise-tmp").exists());
    // A directory that does not exist: the write fails, with nothing to clean up.
    assert!(replace(&dir.join("no/such/file"), "x").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
