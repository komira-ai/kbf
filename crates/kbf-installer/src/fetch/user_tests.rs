use super::*;

/// Catches the user's name drifting from the contract's: `kbf-fetch`, and `_kbf-fetch` on macOS.
#[test]
fn the_fetch_user_is_named_per_platform() {
    assert_eq!(name(Platform::Linux), "kbf-fetch");
    assert_eq!(name(Platform::MacOs), "_kbf-fetch");
}

/// Catches a lookup that would run untrusted bytes as root (`root` has uid 0), one that
/// takes a missing user for anything, and one that cannot say what went wrong with a
/// name the C library cannot take.
#[test]
fn only_an_existing_unprivileged_user_is_found() {
    assert_eq!(lookup("root"), Err(UserFault::Root("root".to_owned())));
    assert_eq!(
        lookup("kbf-no-such-user-xyz"),
        Err(UserFault::Missing("kbf-no-such-user-xyz".to_owned()))
    );
    let err = lookup("bad\0name").expect_err("a NUL");
    assert!(matches!(err, UserFault::Lookup { .. }), "{err}");
}

/// Catches a lookup that returns ids that are not the account's: `nobody` is 65534 on
/// Linux (every Linux host the tests run on has the account).
#[cfg(target_os = "linux")]
#[test]
fn a_user_is_found_with_its_ids() {
    let ids = lookup("nobody").expect("the user nobody exists");
    assert_eq!(
        ids,
        RunAs {
            uid: 65534,
            gid: 65534
        }
    );
}

/// Catches a C library's "not found" taken for a failure, or a real failure (a buffer too
/// small, `ERANGE`) taken for "no such user".
#[test]
fn the_librarys_answers_are_told_apart() {
    assert_eq!(answer(0, true), Answer::Found);
    for rc in [0, libc::ENOENT, libc::ESRCH, libc::EBADF, libc::EPERM] {
        assert_eq!(answer(rc, false), Answer::NoSuchUser, "{rc}");
    }
    assert_eq!(answer(libc::ERANGE, false), Answer::Error(libc::ERANGE));
    assert_eq!(answer(libc::EIO, true), Answer::Error(libc::EIO));
}

/// Catches a root account let through because only one of its ids is root's: a user in
/// root's group is as good as root for what the child could read.
#[test]
fn an_account_in_roots_group_is_root_enough() {
    assert!(runs_as_root(0, 0));
    assert!(runs_as_root(0, 5));
    assert!(runs_as_root(5, 0));
    assert!(!runs_as_root(5, 5));
}

/// Catches a failing lookup (here a buffer too small for the entry, `ERANGE`) taken for
/// a missing user: it is the lookup's own error, with the system's words.
#[test]
fn a_lookup_that_fails_says_so() {
    let err = lookup_with("root", 1).expect_err("one byte is too small");
    assert!(
        matches!(&err, UserFault::Lookup { name, .. } if name == "root"),
        "{err}"
    );
}
