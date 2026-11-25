use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;

use super::{FileFlaw, MAX_PASSWORD_FILE_BYTES, read};

/// This process's effective uid: the owner of every file a test makes.
fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// The system's number for a missing file.
pub(crate) fn libc_enoent() -> i32 {
    rustix::io::Errno::NOENT.raw_os_error()
}

/// The file `name` in `dir` holding `bytes`, with `mode`.
pub(crate) fn secret(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write the file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// Catches a line ending kept in the password (the server would refuse it), a space at either
/// end trimmed away (it is part of the password), and an owner-only mode (0600 or 0400) refused.
#[test]
fn a_password_file_gives_its_line_without_its_line_ending() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-file-ok");
    for (bytes, mode, want) in [
        (&b"hunter2\n"[..], 0o600, &b"hunter2"[..]),
        (b"hunter2\r\n", 0o400, b"hunter2"),
        (b"hunter2", 0o600, b"hunter2"),
        (b" two words \n", 0o600, b" two words "),
    ] {
        let path = secret(&dir, "pw", bytes, 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        assert_eq!(
            read(&path, me()).expect("read").as_slice(),
            want,
            "{bytes:?}"
        );
        std::fs::remove_file(&path).expect("remove");
    }
}

/// Catches a file its group or others may read or write accepted (the password would be theirs
/// too), and an executable or set-id file taken as owner-only. The text names the mode.
#[test]
fn a_password_file_others_can_reach_is_refused_by_its_mode() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-file-mode");
    for mode in [0o644, 0o640, 0o660, 0o604, 0o700, 0o4600] {
        let path = secret(&dir, &format!("pw-{mode:o}"), b"hunter2\n", mode);
        let flaw = read(&path, me()).expect_err("refused");
        assert_eq!(flaw, FileFlaw::Mode(mode), "{mode:o}");
        if mode == 0o644 {
            assert_eq!(
                flaw.to_string(),
                "it has mode 0644; it must be 0600 or 0400"
            );
        }
    }
}

/// Catches a file of another user accepted (whoever owns it chooses the password), and the root
/// exception lost: root runs the installer and the boot check for a service that owns the file.
#[test]
fn a_password_file_is_owned_by_this_user_unless_this_user_is_root() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-file-owner");
    let path = secret(&dir, "pw", b"hunter2\n", 0o600);
    let other = me() + 1;
    let flaw = read(&path, other).expect_err("refused");
    assert_eq!(
        flaw,
        FileFlaw::Owner {
            owner: me(),
            euid: other
        }
    );
    assert_eq!(
        flaw.to_string(),
        format!(
            "it is owned by uid {}, not by this process's uid {other}",
            me()
        )
    );
    assert_eq!(
        read(&path, 0).expect("root reads it").as_slice(),
        b"hunter2"
    );
}

/// Catches a symbolic link followed (it may point anywhere), a directory or another file type
/// read as a password, and a missing file that is not a refusal naming the system's reason.
#[test]
fn a_password_file_that_is_a_link_a_directory_or_missing_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-file-kind");
    let target = secret(&dir, "pw", b"hunter2\n", 0o600);
    let link = dir.join("link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    assert_eq!(read(&link, me()), Err(FileFlaw::Symlink));
    assert_eq!(FileFlaw::Symlink.to_string(), "it is a symbolic link");
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).expect("mkdir");
    assert_eq!(read(&sub, me()), Err(FileFlaw::NotAFile));
    assert_eq!(FileFlaw::NotAFile.to_string(), "it is not a regular file");
    let missing = read(&dir.join("missing"), me()).expect_err("refused");
    let enoent = std::io::Error::from_raw_os_error(libc_enoent()).to_string();
    assert_eq!(missing, FileFlaw::Read(enoent.clone()));
    assert_eq!(missing.to_string(), enoent);
}

/// Catches the size limit off by one either way (a file of exactly the limit is a password; one
/// byte more is refused, whatever the line ending), and a file with no password, a second line
/// or a NUL taken as a password.
#[test]
fn a_password_file_holds_one_password_of_at_most_the_limit() {
    let dir = TestDir::under(std::env::temp_dir(), "pg-auth-file-content");
    let full = vec![b'a'; MAX_PASSWORD_FILE_BYTES];
    let path = secret(&dir, "full", &full, 0o600);
    assert_eq!(
        read(&path, me()).expect("read").len(),
        MAX_PASSWORD_FILE_BYTES
    );
    let mut over = full.clone();
    over.push(b'\n');
    let path = secret(&dir, "over", &over, 0o600);
    assert_eq!(read(&path, me()), Err(FileFlaw::TooLarge));
    assert_eq!(
        FileFlaw::TooLarge.to_string(),
        format!("it holds more than {MAX_PASSWORD_FILE_BYTES} bytes")
    );
    for (i, bytes) in [
        &b""[..],
        b"\n",
        b"\r\n",
        b"one\ntwo\n",
        b"one\rtwo",
        b"nul\0inside",
        b"one\n\n",
    ]
    .iter()
    .enumerate()
    {
        let path = secret(&dir, &format!("bad-{i}"), bytes, 0o600);
        assert_eq!(read(&path, me()), Err(FileFlaw::Content), "{bytes:?}");
    }
    assert_eq!(
        FileFlaw::Content.to_string(),
        "it must hold the password alone, on one line"
    );
}
