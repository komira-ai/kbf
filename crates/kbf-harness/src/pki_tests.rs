//! The certificates the farm issues: that each is a row's, and that keys are private from the
//! moment they exist.

use std::os::unix::fs::PermissionsExt as _;

use super::*;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

#[test]
fn keys_are_private_to_this_user_and_so_are_the_directories_that_hold_them() {
    // Catches: private keys of a farm readable by the other users of a shared runner (a file
    // written with the default 0644 in a 0755 directory). Mutants: `write_private` without
    // `.mode(0o600)`; `private_dir` without `.mode(0o700)`.
    let root = crate::scratch("pki-modes");
    let etc = root.join("fresh/etc");
    private_dir(&etc);
    let pki = Pki::new();
    pki.write_ca(&etc);
    pki.issue(&etc, "storage", Row::Storage, &["127.0.0.1"]);
    pki.issue_reapi(&etc, &["localhost"]);
    assert_eq!(mode(&etc), 0o700);
    assert_eq!(
        mode(&root.join("fresh")),
        0o700,
        "the directories above it too"
    );
    for key in ["storage.key", "reapi.key"] {
        assert_eq!(mode(&etc.join(key)), 0o600, "{key}");
    }
    let key = std::fs::read_to_string(etc.join("storage.key")).expect("key");
    assert!(key.contains("PRIVATE KEY"));
}

#[test]
fn a_private_file_replaces_one_that_was_there() {
    // Catches: a rewrite that appends to (or keeps the mode of) an earlier file, so that a
    // token written twice is two tokens, or a widened one stays wide.
    let root = crate::scratch("pki-rewrite");
    let path = root.join("token");
    write_private(&path, b"first, and a longer text\n");
    write_private(&path, b"second\n");
    assert_eq!(std::fs::read_to_string(&path).expect("file"), "second\n");
    assert_eq!(mode(&path), 0o600);
}

#[test]
#[should_panic(expected = "write")]
fn a_private_file_that_cannot_be_written_panics_naming_it() {
    // Catches: a token or key that silently was not written (the process then starts without
    // it, or reads a stale one).
    write_private(Path::new("/nonexistent-dir/token"), b"x");
}

#[test]
#[should_panic(expected = "mkdir")]
fn a_private_directory_that_cannot_be_made_panics_naming_it() {
    // Catches: a directory that silently was not made.
    private_dir(Path::new("/proc/no-such-dir/etc"));
}
