//! Which file is a pin: what [`PinFile::look`] makes of every kind of thing at the name of the
//! hold file, and where the agent looks for it.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use super::pin::{LINUX_PIN, Look, MAC_PIN, PinFile, ROOT};
use super::testing::COMMIT_B;
use crate::testutil::scratch;
use crate::wire::{Commit, PinReport};

/// The uid the test's own files have: the owner a pin must have in these tests.
pub(super) fn me(dir: &Path) -> u32 {
    fs::metadata(dir).unwrap().uid()
}

/// A hold file at `path` as the helper writes it: `text`, mode 0644 whatever the umask.
pub(super) fn put(path: &Path, text: impl AsRef<[u8]>) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

pub(super) fn hold_text(commit: &str) -> String {
    format!("{{\"commit\":\"{commit}\",\"version\":null,\"at_unix\":5,\"by\":\"activate\"}}")
}

fn pinned_to(commit: &str) -> Look {
    Look::Pinned {
        report: PinReport {
            commit: Commit::parse(commit),
        },
        problem: None,
    }
}

fn unknown(problem: String) -> Look {
    Look::Pinned {
        report: PinReport { commit: None },
        problem: Some(problem),
    }
}

/// A scratch directory, the hold file's path in it, the reader that trusts the test's own
/// account, and the path as messages show it.
fn rig(name: &str) -> (PathBuf, PinFile, String) {
    let dir = scratch(name);
    let path = dir.join("pin.json");
    let pin = PinFile::new(&path, me(&dir));
    let shown = path.display().to_string();
    (path, pin, shown)
}

/// The owner's regular file is a pin with its commit; another account's file, or one its group
/// or others may write, is no pin, and says why.
///
/// Catches: a pin taken from a file the agent's own account wrote; a pin whose commit is lost.
#[test]
fn only_the_owners_file_that_no_one_else_may_write_is_a_pin() {
    let (path, pin, shown) = rig("pin-owner");
    let uid = me(path.parent().unwrap());
    assert_eq!(pin.look(), Look::Absent);
    assert_eq!(pin.look().line(), "not pinned: following the coordinator");
    put(&path, hold_text(COMMIT_B));
    assert_eq!(pin.look(), pinned_to(COMMIT_B));
    assert_eq!(
        pin.look().line(),
        format!("pinned to {COMMIT_B}: no order for kbf runs")
    );
    assert_eq!(
        PinFile::new(&path, uid + 1).look(),
        Look::NotAPin(format!("{shown} is owned by uid {uid}, not {}", uid + 1))
    );
    for mode in [0o664, 0o646] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let look = pin.look();
        assert_eq!(
            look,
            Look::NotAPin(format!("{shown} is writable by its group or by others")),
            "{mode:o}"
        );
        assert_eq!(
            look.line(),
            format!("the hold file is not a pin: {shown} is writable by its group or by others")
        );
    }
}

/// What root's file holds: a commit padded to exactly the 4096 bytes read is a pin with the
/// commit; one byte more, nothing, a missing, null, mistyped or malformed commit, text that is
/// not JSON and bytes that are not UTF-8 are each a pin with no commit, saying why.
///
/// Catches: a file cut at the limit read as whole; a root's file whose commit cannot be read
/// taken for "not pinned".
#[test]
fn roots_file_is_a_pin_whatever_it_holds_and_its_commit_only_when_whole() {
    let (path, pin, shown) = rig("pin-content");
    let exact = format!("{}{}", hold_text(COMMIT_B), " ".repeat(4096));
    put(&path, &exact[..4096]);
    assert_eq!(pin.look(), pinned_to(COMMIT_B));
    put(&path, &exact[..4097]);
    assert_eq!(pin.look(), unknown(format!("{shown}: it is too long")));
    put(&path, "{\"commit\":\"BBBB\"}");
    let look = pin.look();
    assert_eq!(
        look,
        unknown(format!("{shown}: its commit is not 40 hex digits"))
    );
    assert_eq!(
        look.line(),
        format!(
            "pinned, the commit unknown ({shown}: its commit is not 40 hex digits): no order for \
             kbf runs"
        )
    );
    for (text, says) in [
        (&b""[..], "EOF while parsing a value at line 1 column 0"),
        (b"{}", "missing field `commit` at line 1 column 2"),
        (
            b"{\"commit\":null}",
            "invalid type: null, expected a string at line 1 column 14",
        ),
        (
            b"{\"commit\":7}",
            "invalid type: integer `7`, expected a string at line 1 column 11",
        ),
        (b"not json", "expected ident at line 1 column 2"),
        (
            b"{\"commit\":\"\xff\"}",
            "invalid unicode code point at line 1 column 13",
        ),
    ] {
        put(&path, text);
        assert_eq!(pin.look(), unknown(format!("{shown}: {says}")), "{text:?}");
    }
    assert_eq!(
        Look::Pinned {
            report: PinReport { commit: None },
            problem: None
        }
        .line(),
        "pinned: no order for kbf runs"
    );
}

/// A link (to a good hold file), a directory, a socket and a pipe at the name are no pin; the
/// pipe is not waited on.
///
/// Catches: a pin read through a link the agent's account made; a look that hangs on a pipe.
#[test]
fn a_link_a_directory_a_socket_or_a_pipe_is_no_pin() {
    let (path, pin, shown) = rig("pin-kinds");
    let elsewhere = path.with_file_name("elsewhere");
    put(&elsewhere, hold_text(COMMIT_B));
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let look = pin.look();
    assert_eq!(look, Look::NotAPin(format!("{shown} is a link")));
    assert_eq!(
        look.line(),
        format!("the hold file is not a pin: {shown} is a link")
    );
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(pin.look(), Look::NotAPin(format!("{shown} is a directory")));
    fs::remove_dir(&path).unwrap();
    let socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
    assert_eq!(
        pin.look(),
        Look::NotAPin(format!("{shown} is not a regular file"))
    );
    drop(socket);
    fs::remove_file(&path).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    assert_eq!(
        pin.look(),
        Look::NotAPin(format!("{shown} is not a regular file"))
    );
}

/// A name that cannot be looked at is a pin with no commit: the hold file may be there. A
/// missing directory is no hold file. A file of the owner's this account may not read is a pin
/// with no commit (root reads it whatever its mode, and finds the commit).
///
/// Catches: an I/O error taken for "not pinned", which lets an order run over a pin.
#[test]
fn a_name_that_cannot_be_looked_at_holds_the_host() {
    let (path, pin, shown) = rig("pin-errors");
    let dir = path.parent().unwrap();
    put(&dir.join("a-file"), "x");
    let under_a_file = dir.join("a-file/pin.json");
    assert_eq!(
        PinFile::new(&under_a_file, me(dir)).look(),
        unknown(format!(
            "{}: Not a directory (os error 20)",
            under_a_file.display()
        ))
    );
    assert_eq!(
        PinFile::new(dir.join("missing/pin.json"), me(dir)).look(),
        Look::Absent
    );

    put(&path, hold_text(COMMIT_B));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let expected = if fs::File::open(&path).is_ok() {
        pinned_to(COMMIT_B)
    } else {
        unknown(format!("{shown}: Permission denied (os error 13)"))
    };
    assert_eq!(pin.look(), expected);
}

/// The agent looks where the helper writes: the same fixed paths, root's.
///
/// Catches: the agent and the helper disagreeing on where the pin is.
#[test]
fn the_hold_file_is_the_helpers() {
    use kbf_host_install::contract::local::{LINUX_HOLD, MAC_HOLD};
    assert_eq!((LINUX_PIN, MAC_PIN), (LINUX_HOLD, MAC_HOLD));
    assert_eq!(PinFile::of_platform("linux"), PinFile::new(LINUX_PIN, ROOT));
    assert_eq!(PinFile::of_platform("macos"), PinFile::new(MAC_PIN, ROOT));
}
