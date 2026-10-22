use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;

use super::{MAX_FILE_LEN, Trust, TrustError};

/// A scratch directory only its owner can write, whatever the umask: the walk upward from a
/// file refuses a directory others can write.
fn scratch(name: &str) -> TestDir {
    let dir = TestDir::under(std::env::temp_dir(), name);
    chmod(&dir, 0o755);
    dir
}

fn file(dir: &Path, name: &str, mode: u32, text: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn me(dir: &Path) -> Trust {
    Trust::owned_by(fs::metadata(dir).unwrap().uid(), dir)
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
    let stranger = Trust::owned_by(fs::metadata(&dir).unwrap().uid() + 1, &dir);
    assert!(matches!(stranger.read(&path), Err(TrustError::NotOwned)));
    assert!(matches!(
        stranger.check_file(&path),
        Err(TrustError::NotOwned)
    ));
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
        assert!(
            matches!(trust.read(&path), Err(TrustError::Writable)),
            "mode {mode:o}"
        );
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
    assert!(matches!(
        trust.read(&dir),
        Err(TrustError::NotRegular) | Err(TrustError::Io(_))
    ));
    assert!(matches!(
        trust.check_file(&dir),
        Err(TrustError::NotRegular)
    ));
    assert!(matches!(
        trust.read(&dir.join("missing")),
        Err(TrustError::Io(_))
    ));
    assert!(matches!(
        trust.check_file(&dir.join("missing")),
        Err(TrustError::Io(_))
    ));
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
    assert_eq!(Trust::root().anchor, Path::new("/"), "the walk goes to /");
    for (error, word) in [
        (TrustError::NotRegular, "regular"),
        (TrustError::NotOwned, "owned"),
        (TrustError::Writable, "writable"),
        (TrustError::TooLarge, "larger"),
        (TrustError::ParentNotOwned, "owned"),
        (TrustError::ParentWritable, "writable"),
        (TrustError::Io(std::io::Error::other("boom")), "boom"),
    ] {
        assert!(error.to_string().contains(word), "{error}");
    }
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Catches a check that looks at the file and not at the way to it: a directory above the
/// file that its group or others can write lets them replace the file, whatever the file's
/// own mode. Each of the parent, the grandparent and the top of the walk is made writable
/// in turn, by group alone and by others alone, and each is refused, by `read` and by
/// `check_file`.
#[test]
fn a_writable_directory_above_is_refused() {
    let dir = scratch("trust-parents");
    let trust = me(&dir);
    let middle = dir.join("a");
    let deep = middle.join("b");
    fs::create_dir_all(&deep).unwrap();
    let path = file(&deep, "f", 0o644, "x");
    for d in [&*dir, &*middle, &*deep] {
        chmod(d, 0o755);
    }
    assert_eq!(trust.read(&path).unwrap(), "x");
    trust.check_file(&path).unwrap();
    for (which, d) in [("parent", &*deep), ("grandparent", &*middle), ("top", &*dir)] {
        for mode in [0o775, 0o757, 0o770, 0o707, 0o777, 0o730, 0o703] {
            chmod(d, mode);
            assert!(
                matches!(trust.read(&path), Err(TrustError::ParentWritable)),
                "read, {which} {mode:o}"
            );
            assert!(
                matches!(trust.check_file(&path), Err(TrustError::ParentWritable)),
                "check_file, {which} {mode:o}"
            );
            chmod(d, 0o755);
        }
    }
    chmod(&deep, 0o700);
    trust.check_file(&path).unwrap();
}

/// Catches a walk that goes on past its anchor: the directories above the anchor (here
/// `/tmp`, which anyone can write) are not looked at, so a trust anchored at the fixture
/// accepts a file in it; the anchor is where the walk ends, not where it begins.
#[test]
fn the_walk_ends_at_the_anchor() {
    let dir = scratch("trust-anchor");
    chmod(&dir, 0o755);
    let path = file(&dir, "f", 0o644, "x");
    let trust = me(&dir);
    assert!(trust.check_file(&path).is_ok());
    // A walk to / meets a directory of another owner on the way (/ itself at the latest).
    let uid = fs::metadata(&*dir).unwrap().uid();
    if uid != 0 {
        let to_root = Trust::owned_by(uid, Path::new("/"));
        assert!(matches!(
            to_root.check_file(&path),
            Err(TrustError::ParentNotOwned)
        ));
        assert!(matches!(
            to_root.read(&path),
            Err(TrustError::ParentNotOwned)
        ));
    }
}

/// Catches a check that stops at a link: `check_file` follows a link, and the directories
/// above the target are the ones that matter. A link in a safe directory to a file in a
/// writable one is refused.
#[test]
fn the_way_to_a_links_target_is_checked() {
    let dir = scratch("trust-link-target");
    chmod(&dir, 0o755);
    let open = dir.join("open");
    fs::create_dir(&open).unwrap();
    let target = file(&open, "t", 0o644, "x");
    chmod(&open, 0o777);
    let safe = dir.join("safe");
    fs::create_dir(&safe).unwrap();
    chmod(&safe, 0o755);
    let link = safe.join("link");
    symlink(&target, &link).unwrap();
    let trust = me(&dir);
    assert!(matches!(
        trust.check_file(&link),
        Err(TrustError::ParentWritable)
    ));
    chmod(&open, 0o755);
    trust.check_file(&link).unwrap();
}

/// A path with `..` in it is judged by where it really leads, not by how it is spelled.
#[test]
fn dot_dot_is_resolved() {
    let dir = scratch("trust-dotdot");
    chmod(&dir, 0o755);
    let open = dir.join("open");
    let safe = dir.join("safe");
    fs::create_dir(&open).unwrap();
    fs::create_dir(&safe).unwrap();
    chmod(&safe, 0o755);
    file(&safe, "f", 0o644, "x");
    chmod(&open, 0o777);
    let trust = me(&dir);
    // safe/../open/../safe/f is safe/f; `open` is not above it.
    let spelled = dir.join("safe/../open/../safe/f");
    assert_eq!(trust.read(&spelled).unwrap(), "x");
    // open/../safe/f also; but a file inside `open` is not, however it is spelled.
    file(&open, "g", 0o644, "y");
    let spelled = dir.join("safe/../open/g");
    assert!(matches!(
        trust.read(&spelled),
        Err(TrustError::ParentWritable)
    ));
    chmod(&open, 0o755);
}
