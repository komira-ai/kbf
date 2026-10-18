//! The service loop's re-exec decision, with the program's file played by a flag and, once,
//! by a real file.

use std::fs;

use super::*;

/// Catches the real watch calling a rewritten, renamed-over or removed file unchanged: a
/// file replaced by a rename (the package manager's way), or removed, is replaced; one
/// not touched is not.
///
/// Mutants planted: only the size compared; only the mtime compared.
#[test]
fn the_file_identity_sees_a_rename_over_and_a_removal() {
    let dir = kbf_host::testkit::scratch("serve-identity");
    let path = dir.join("kbf-installer");
    fs::write(&path, b"old code").expect("write");
    let watch = FileIdentity::of(&path).expect("identity");
    assert!(!watch.replaced());
    let new = dir.join("kbf-installer.new");
    fs::write(&new, b"old code").expect("same bytes");
    fs::rename(&new, &path).expect("rename over");
    assert!(watch.replaced(), "a new file at the path, same size");
    let watch = FileIdentity::of(&path).expect("identity");
    fs::remove_file(&path).expect("remove");
    assert!(watch.replaced());
    assert_eq!(PASS_INTERVAL, Duration::from_secs(5));
}
