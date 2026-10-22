use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;

use super::{Trust, TrustError, MAX_FILE_LEN};

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

fn file(dir: &Path, name: &str, mode: u32, text: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn me(dir: &Path) -> Trust {
    Trust::owned_by(fs::metadata(dir).unwrap().uid())
}

/// A file of the trusted user that only it can write is read; 0644 and 0600 are both fine.
#[test]
fn a_trusted_file_is_read() {
    let dir = scratch("trust-ok");
    let trust = me(&dir);
    for mode in [0o644, 0o600, 0o444] {
        let path = file(&dir, "f", mode, "hello");
        assert_eq!(trust.read(&path).unwrap(), "hello", "mode {mode:o}");
        trust.check_file(&path).unwrap();
        trust.check_meta(&fs::metadata(&path).unwrap()).unwrap();
    }
}

/// Catches a check that forgets the owner: a file of another user is refused even at 0644.
#[test]
fn another_owner_is_refused() {
    let dir = scratch("trust-owner");
    let path = file(&dir, "f", 0o644, "x");
    let stranger = Trust::owned_by(fs::metadata(&dir).unwrap().uid() + 1);
    assert!(matches!(stranger.read(&path), Err(TrustError::NotOwned)));
    assert!(matches!(stranger.check_file(&path), Err(TrustError::NotOwned)));
    assert!(!stranger.owns(&fs::metadata(&path).unwrap()));
    assert!(me(&dir).owns(&fs::metadata(&path).unwrap()));
}

/// Catches a check that forgets either write bit: group-writable and world-writable are
/// each refused on their own.
#[test]
fn group_or_world_writable_is_refused() {
    let dir = scratch("trust-mode");
    let trust = me(&dir);
    for mode in [0o664, 0o646, 0o666, 0o620, 0o602, 0o777] {
        let path = file(&dir, "f", mode, "x");
        assert!(matches!(trust.read(&path), Err(TrustError::Writable)), "mode {mode:o}");
        assert!(matches!(trust.check_file(&path), Err(TrustError::Writable)));
    }
}

/// Catches a read that follows a link: the file is opened `O_NOFOLLOW`. A directory and a
/// missing file are errors too; `check_file` follows a link to a trusted target.
#[test]
fn links_directories_and_missing_files() {
    let dir = scratch("trust-kind");
    let trust = me(&dir);
    let real = file(&dir, "real", 0o644, "x");
    let link = dir.join("link");
    symlink(&real, &link).unwrap();
    assert!(matches!(trust.read(&link), Err(TrustError::Io(_))));
    trust.check_file(&link).unwrap();
    assert!(matches!(trust.read(&dir), Err(TrustError::NotRegular) | Err(TrustError::Io(_))));
    assert!(matches!(trust.check_file(&dir), Err(TrustError::NotRegular)));
    assert!(matches!(trust.read(&dir.join("missing")), Err(TrustError::Io(_))));
    assert!(matches!(trust.check_file(&dir.join("missing")), Err(TrustError::Io(_))));
}

/// Catches an unbounded read: a file over the cap is refused, one at the cap is read; text
/// that is not UTF-8 is an error and not a replacement.
#[test]
fn size_cap_and_encoding() {
    let dir = scratch("trust-size");
    let trust = me(&dir);
    let at_cap = file(&dir, "cap", 0o644, &"a".repeat(MAX_FILE_LEN as usize));
    assert_eq!(trust.read(&at_cap).unwrap().len() as u64, MAX_FILE_LEN);
    let over = file(&dir, "over", 0o644, &"a".repeat(MAX_FILE_LEN as usize + 1));
    assert!(matches!(trust.read(&over), Err(TrustError::TooLarge)));
    let bytes = dir.join("bytes");
    fs::write(&bytes, [0xff, 0xfe]).unwrap();
    fs::set_permissions(&bytes, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(trust.read(&bytes), Err(TrustError::Io(_))));
}

#[test]
fn root_is_uid_zero_and_errors_read_aloud() {
    assert_eq!(Trust::root().uid, 0);
    for (error, word) in [
        (TrustError::NotRegular, "regular"),
        (TrustError::NotOwned, "owned"),
        (TrustError::Writable, "writable"),
        (TrustError::TooLarge, "larger"),
        (TrustError::Io(std::io::Error::other("boom")), "boom"),
    ] {
        assert!(error.to_string().contains(word), "{error}");
    }
}
