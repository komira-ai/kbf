//! The umask is per process, so the test that changes it runs alone in this binary.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use kbf_testkit::dir::TestDir;
use kbf_vmm::offline_put::{Env, put, remove_path};
use kbf_vmm::system::OfflineFile;

fn mode_of(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).expect("stat").mode() & 0o7777
}

/// Catches a file or a created directory whose mode follows the process umask instead of
/// being set exactly on the open descriptor.
#[test]
fn modes_are_exact_whatever_the_umask() {
    let root = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "offline-put-umask");
    let env = Env {
        remove: &remove_path,
        owner: None,
    };
    let f = OfflineFile {
        path: "a/b/f".to_owned(),
        contents: b"x".to_vec(),
        mode: 0o755,
    };
    // SAFETY: umask only changes this process's mask; this binary has one test.
    unsafe { libc::umask(0o277) };
    put(root.path(), &f, &env).expect("put");
    assert_eq!(mode_of(&root.path().join("a/b/f")), 0o755);
    assert_eq!(mode_of(&root.path().join("a")), 0o755);
    assert_eq!(mode_of(&root.path().join("a/b")), 0o755);
}
