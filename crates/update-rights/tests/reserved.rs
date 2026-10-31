//! The reserved-path deny list: what no declaration may read, write or keep its record in.

use update_rights::reserved::{HOOKS_ROOT, Kind, RESERVED, Violation, check, is_within};

const KINDS: [Kind; 3] = [Kind::Read, Kind::Write, Kind::State];

/// Catches a list whose first entry is not `/`: the rule on the inside skips it by position.
#[test]
fn the_root_comes_first() {
    assert_eq!(RESERVED[0], "/");
}

/// Catches `is_within` taking a sibling with the same prefix for a child (`/etcetera` is not
/// inside `/etc`), or the root for a path it does not contain.
#[test]
fn within_means_below_or_equal() {
    assert!(is_within("/etc", "/etc"));
    assert!(is_within("/etc/x", "/etc"));
    assert!(is_within("/etc/x/y", "/etc"));
    assert!(!is_within("/etcetera", "/etc"));
    assert!(!is_within("/et", "/etc"));
    assert!(!is_within("/", "/etc"));
    assert!(is_within("/anything", "/"));
    assert!(is_within("/", "/"));
}

/// Catches a reserved directory that an entry of any kind can name: `/etc`, `/usr`, the sudo,
/// polkit and unit directories, the updater's own.
#[test]
fn no_entry_names_a_reserved_directory() {
    for reserved in RESERVED {
        for kind in KINDS {
            assert_eq!(
                check(kind, reserved),
                Err(Violation::Names(reserved)),
                "{reserved}"
            );
        }
    }
}

/// Catches an entry that contains a reserved directory: `/var/lib` holds `/var/lib/updater`, and
/// `/etc/systemd` holds `/etc/systemd/system`.
#[test]
fn no_entry_contains_a_reserved_directory() {
    for (path, holds) in [
        ("/var", "/var/lib/updater"),
        ("/var/lib", "/var/lib/updater"),
        ("/etc/systemd", "/etc/systemd/system"),
        ("/run", "/run/systemd/system"),
        ("/run/systemd", "/run/systemd/system"),
        ("/lib", "/lib/systemd/system"),
        ("/usr/share", "/usr/share/polkit-1"),
    ] {
        for kind in KINDS {
            assert_eq!(check(kind, path), Err(Violation::Contains(holds)), "{path}");
        }
    }
}

/// Catches a single file or a directory inside a reserved directory being refused for reading:
/// the declaration itself, `/etc/tierstore` and `/usr/sbin/nft` are read-only and harmless.
#[test]
fn a_file_inside_a_reserved_directory_may_be_read() {
    for path in [
        "/etc/updater.d/tierstore.toml",
        "/etc/tierstore",
        "/usr/sbin/nft",
        "/etc/sudoers.d/x",
        "/var/lib/updater/hooks",
        "/var/lib/updater/releases/x",
    ] {
        assert_eq!(check(Kind::Read, path), Ok(()), "{path}");
    }
}

/// Catches a write or a record directory inside a reserved directory: the sudo rules, the
/// declarations, the release tree.
#[test]
fn nothing_inside_a_reserved_directory_may_be_written() {
    for (path, inside) in [
        ("/etc/updater.d/tierstore.toml", "/etc/updater.d"),
        ("/etc/sudoers.d/x", "/etc/sudoers.d"),
        ("/etc/cron.d/x", "/etc"),
        ("/usr/local/bin/x", "/usr"),
        ("/var/lib/updater/releases/x", "/var/lib/updater"),
        ("/var/lib/updater/hooks", "/var/lib/updater"),
        ("/etc/polkit-1/rules.d/x", "/etc/polkit-1"),
        ("/usr/lib/systemd/system/x.service", "/usr"),
    ] {
        for kind in [Kind::Write, Kind::State] {
            match check(kind, path) {
                Err(Violation::Inside(found)) => assert!(
                    is_within(path, found),
                    "{path} is not inside the {found} it was refused for"
                ),
                other => panic!("{path} ({inside}): {other:?}"),
            }
        }
    }
}

/// Catches the record directories of the hooks being unusable: the one place inside the
/// reserved directories where something may be written is under the hooks root, and not the
/// root itself.
#[test]
fn the_hooks_root_is_where_records_live() {
    for kind in [Kind::Write, Kind::State] {
        assert_eq!(check(kind, &format!("{HOOKS_ROOT}/tierstore")), Ok(()));
        assert_eq!(check(kind, &format!("{HOOKS_ROOT}/a/b")), Ok(()));
        assert!(matches!(check(kind, HOOKS_ROOT), Err(Violation::Inside(_))));
        assert!(matches!(
            check(kind, "/var/lib/updater/hooksx"),
            Err(Violation::Inside(_))
        ));
    }
}

/// Catches ordinary paths being refused.
#[test]
fn ordinary_paths_pass() {
    for path in [
        "/var/lib/tierstore",
        "/srv/x",
        "/opt/x/y",
        "/home/x",
        "/tmp/x",
    ] {
        for kind in KINDS {
            assert_eq!(check(kind, path), Ok(()), "{path}");
        }
    }
}

/// Every refusal says why.
#[test]
fn violations_read_as_sentences() {
    for violation in [
        Violation::Names("/etc"),
        Violation::Contains("/etc"),
        Violation::Inside("/etc"),
    ] {
        assert!(violation.to_string().contains("/etc"));
    }
}
