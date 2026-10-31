//! The accounts a declaration names, read from the text of the password and group files.

mod support;

use support::Scratch;
use update_rights::{Accounts, EtcAccounts, UserIds};

const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/sh
short:x:1
badid:x:abc:5:x:/:/bin/false
badgid:x:6:xyz:x:/:/bin/false
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
tierstore:x:990:991::/nonexistent:/usr/sbin/nologin
tierstore:x:7:7::/second:/bin/false
";

const GROUP: &str = "\
root:x:0:
nogid:x:abc:tierstore
tierstore:x:991:
helpers:x:2000:alice,tierstore,bob
others:x:2001:alice
solo:x:2002:tierstore
threefields:x:2003
";

/// Catches a user found by something other than its name, a malformed row ending the search,
/// and the second of two rows of one name winning over the first.
#[test]
fn users_are_found_by_name_in_the_password_text() {
    let accounts = EtcAccounts::new(PASSWD, GROUP);
    assert_eq!(accounts.user("root"), Some(UserIds { uid: 0, gid: 0 }));
    assert_eq!(accounts.user("daemon"), Some(UserIds { uid: 1, gid: 1 }));
    assert_eq!(
        accounts.user("tierstore"),
        Some(UserIds { uid: 990, gid: 991 })
    );
    for absent in [
        "", "nobody", "x", "short", "badid", "badgid", "roo", "tierstor",
    ] {
        assert_eq!(accounts.user(absent), None, "{absent:?}");
    }
}

/// Catches a group found by something other than its name, or a group of no number.
#[test]
fn groups_are_found_by_name_in_the_group_text() {
    let accounts = EtcAccounts::new(PASSWD, GROUP);
    assert_eq!(accounts.group("root"), Some(0));
    assert_eq!(accounts.group("tierstore"), Some(991));
    assert_eq!(accounts.group("helpers"), Some(2000));
    for absent in ["", "nogid", "nobody", "helper"] {
        assert_eq!(accounts.group(absent), None, "{absent:?}");
    }
}

/// Catches membership read by substring (`tier` is not `tierstore`), a group without members
/// being listed, and the user's primary group being counted as a membership.
#[test]
fn the_groups_that_list_a_user_are_those_with_the_name_among_the_members() {
    let accounts = EtcAccounts::new(PASSWD, GROUP);
    assert_eq!(accounts.member_of("tierstore"), [2000, 2002]);
    assert_eq!(accounts.member_of("alice"), [2000, 2001]);
    assert_eq!(accounts.member_of("bob"), [2000]);
    for none in ["tier", "tierstores", "root", "carol"] {
        assert!(accounts.member_of(none).is_empty(), "{none:?}");
    }
}

/// Catches an empty host: nothing is known, and nothing panics.
#[test]
fn no_text_knows_nobody() {
    let accounts = EtcAccounts::new("", "");
    assert_eq!(accounts.user("root"), None);
    assert_eq!(accounts.group("root"), None);
    assert!(accounts.member_of("root").is_empty());
    let default = EtcAccounts::default();
    assert_eq!(default.user("root"), None);
}

/// Catches the files being read from the wrong place, or an unreadable file being an empty one.
#[test]
fn the_files_are_read_from_the_paths_given() {
    let scratch = Scratch::new("accounts");
    let passwd = scratch.file("passwd", PASSWD, 0o644);
    let group = scratch.file("group", GROUP, 0o644);
    let accounts = EtcAccounts::read(&passwd, &group).unwrap();
    assert_eq!(
        accounts.user("tierstore"),
        Some(UserIds { uid: 990, gid: 991 })
    );
    assert_eq!(accounts.group("helpers"), Some(2000));
    assert!(EtcAccounts::read(&scratch.path().join("missing"), &group).is_err());
    assert!(EtcAccounts::read(&passwd, &scratch.path().join("missing")).is_err());
}

/// Catches the host's own files not being what `system` reads: root is uid 0 on every host.
#[test]
fn the_hosts_own_files_know_root() {
    let accounts = EtcAccounts::system().unwrap();
    assert_eq!(accounts.user("root").map(|ids| ids.uid), Some(0));
}
