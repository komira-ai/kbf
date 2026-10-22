//! The settings file a test hands to a `kbf` child.

use std::os::unix::fs::PermissionsExt as _;

use kbf_testkit::config::{FILE, flag, flag_with};
use kbf_testkit::dir::TestDir;

const ROOT: &str = env!("CARGO_TARGET_TMPDIR");

#[test]
fn flag_names_an_empty_private_file_in_the_directory() {
    // Catches: a flag naming a file that is not there, one that holds something, or one
    // that group or other can write (kbf refuses such a file).
    let dir = TestDir::under(ROOT, "config-flag");
    let got = flag(dir.path());
    let file = dir.path().join(FILE);
    assert_eq!(got, format!("--config={}", file.display()));
    assert_eq!(std::fs::read(&file).expect("the file"), b"");
    let mode = std::fs::metadata(&file).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn flag_with_writes_the_text_and_replaces_an_earlier_file() {
    // Catches: text appended to an earlier file's instead of replacing it (a restart of a
    // test in the same directory would then read two files' keys), or lost.
    let dir = TestDir::under(ROOT, "config-flag-with");
    let _ = flag_with(dir.path(), "[a]\nb = \"long text long text\"\n");
    let got = flag_with(dir.path(), "[c]\nd = 1\n");
    let file = dir.path().join(FILE);
    assert_eq!(got, format!("--config={}", file.display()));
    assert_eq!(
        std::fs::read_to_string(&file).expect("the file"),
        "[c]\nd = 1\n"
    );
}
