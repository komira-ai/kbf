//! The floor's one writer, on a scratch host. What each test proves is named on it, with the
//! defect it catches.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::testkit::host_root;

/// The floor file of `host`.
fn file(host: &Layout) -> PathBuf {
    host.join(&host.seq_floor())
}

/// What the floor file holds.
fn held(host: &Layout) -> String {
    fs::read_to_string(file(host)).expect("a file")
}

/// A raise makes the file (one decimal number, a line end, root's alone), a raise to a number
/// that is not above the floor leaves it as it is, and a higher one replaces it; each answers
/// with the floor after it.
///
/// Catches a floor written at the number given whatever stands (a floor that goes down), a
/// file others can read or write, and a floor not made when there was none.
///
/// Catches: the comparison dropped (the file is written whatever its number: red on
/// the content after the lower raise); the mode 0644 (red on the mode).
#[test]
fn a_raise_makes_the_floor_and_never_lowers_it() {
    let (host, _) = host_root("floor-raise");
    assert_eq!(raise(&host, 40).expect("made"), 40);
    assert_eq!(held(&host), "40\n");
    let meta = fs::metadata(file(&host)).expect("meta");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    for lower in [39, 40] {
        assert_eq!(raise(&host, lower).expect("kept"), 40, "{lower}");
        assert_eq!(held(&host), "40\n", "{lower}");
    }
    assert_eq!(raise(&host, 41).expect("raised"), 41);
    assert_eq!(held(&host), "41\n");
}

/// A floor that is there and is not one number is not replaced: the raise refuses, saying so,
/// whether the thing is a directory or text, and the file is as it was.
///
/// Catches a floor that cannot be read taken for no floor and written over, which would lower
/// it (to the number given) behind the operator's back.
///
/// Catches: the read error mapped to `None`.
#[test]
fn a_floor_that_cannot_be_read_is_left_alone() {
    let (host, _) = host_root("floor-unreadable");
    fs::create_dir(file(&host)).expect("a directory where the file goes");
    let err = raise(&host, 40).expect_err("unreadable");
    assert!(matches!(err, FloorError::Unreadable(_)), "{err:?}");
    assert!(err.to_string().contains("cannot be read"), "{err}");
    assert!(file(&host).is_dir());

    let (host, _) = host_root("floor-text");
    fs::write(file(&host), "forty\n").expect("text");
    let err = raise(&host, 40).expect_err("unreadable");
    assert!(matches!(err, FloorError::Unreadable(_)), "{err:?}");
    assert!(err.to_string().contains("is not one number"), "{err}");
    assert_eq!(held(&host), "forty\n");
}

/// A raise that cannot write its new file says so and leaves no floor behind, and one that
/// cannot take the lock says that (the lock's directory is a file here).
///
/// Catches an error swallowed (a raise that reports a floor it did not write: the confirmed
/// release would then be orderable again as a seed).
///
/// Catches: the write error ignored; the lock error ignored.
#[test]
fn a_raise_that_cannot_write_or_lock_says_so() {
    let (host, _) = host_root("floor-write");
    // The temporary name is taken by a directory, which `remove_file` cannot clear.
    fs::create_dir(file(&host).with_file_name(".seq_floor.tmp")).expect("in the way");
    let got = raise(&host, 40);
    assert!(matches!(&got, Err(FloorError::Write { .. })), "{got:?}");
    assert!(!file(&host).exists());

    let (host, _) = host_root("floor-lock");
    let dir = host.join(&host.lock());
    let dir = dir.parent().expect("a lock has a directory").to_owned();
    fs::remove_dir_all(&dir).expect("the lock directory");
    fs::write(&dir, "a file where the directory goes").expect("a file");
    let got = raise(&host, 40);
    assert!(matches!(&got, Err(FloorError::Lock(_))), "{got:?}");
    assert!(!file(&host).exists());
}
