//! The reserved-path deny list: what no declaration may read, write or keep its record in.

use update_rights::reserved::{
    HOOKS_ROOT, Kind, RESERVED, Violation, WRITE_FORBIDDEN, WRITE_ROOTS, check, is_within,
};

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
        ("/var", "/var/cache/apt"),
        ("/var/lib", "/var/lib/dpkg"),
        ("/etc/systemd", "/etc/systemd/system"),
        ("/run", "/run/systemd/generator"),
        ("/run/systemd", "/run/systemd/generator"),
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
        "/srv/y",
        "/tmp/x",
    ] {
        for kind in [Kind::Read, Kind::State] {
            assert_eq!(check(kind, path), Ok(()), "{path}");
        }
    }
    for path in [
        "/var/lib/tierstore",
        "/var/cache/x",
        "/run/x/y",
        "/opt/x/y/z",
    ] {
        assert_eq!(check(Kind::Write, path), Ok(()), "{path}");
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

/// Catches a reserved directory dropped from the list: the list is pinned entry by entry, so
/// deleting `/run/updater`, `/etc/polkit-1` or `/usr/lib/systemd/system` is a red test and not a
/// silent widening. Adding one means adding it here.
#[test]
fn the_list_is_exactly_this() {
    assert_eq!(
        RESERVED,
        [
            "/",
            "/bin",
            "/boot",
            "/etc",
            "/etc/polkit-1",
            "/etc/sudoers.d",
            "/etc/systemd/system",
            "/etc/updater.d",
            "/lib",
            "/lib/systemd/system",
            "/lib64",
            "/root",
            "/run/systemd/generator",
            "/run/systemd/generator.early",
            "/run/systemd/generator.late",
            "/run/systemd/system",
            "/run/systemd/system.control",
            "/run/systemd/transient",
            "/run/updater",
            "/sbin",
            "/usr",
            "/usr/lib/systemd/system",
            "/usr/share/polkit-1",
            "/var/cache/apt",
            "/var/lib/dpkg",
            "/var/lib/polkit-1",
            "/var/lib/updater",
            "/var/spool/cron",
        ]
    );
    assert_eq!(WRITE_FORBIDDEN, ["/dev", "/proc", "/sys"]);
    assert_eq!(WRITE_ROOTS, ["/var/lib", "/var/cache", "/run", "/opt"]);
}

/// Catches the unit directories that systemd reads at run time (transient units, generators,
/// the runtime overrides of `systemctl set-property`) and the polkit state being writable or
/// nameable: each is refused for every kind, and a file inside one only for reading.
#[test]
fn the_runtime_unit_and_polkit_directories_are_reserved() {
    for dir in [
        "/run/systemd/transient",
        "/run/systemd/generator",
        "/run/systemd/generator.early",
        "/run/systemd/generator.late",
        "/run/systemd/system.control",
        "/var/lib/polkit-1",
    ] {
        for kind in KINDS {
            assert_eq!(
                check(kind, dir),
                Err(Violation::Names(
                    RESERVED.iter().find(|r| **r == dir).unwrap()
                ))
            );
        }
        let inside = format!("{dir}/x.conf");
        assert_eq!(check(Kind::Read, &inside), Ok(()), "{inside}");
        for kind in [Kind::Write, Kind::State] {
            assert!(
                matches!(check(kind, &inside), Err(Violation::Inside(_))),
                "{inside}"
            );
        }
    }
}

/// Catches `/proc`, `/sys` and `/dev` being writable by a declaration: nothing there is a file
/// to write (a hook that wants a kernel setting is not a hook), and each may still be read.
#[test]
fn the_kernel_and_device_trees_are_never_written() {
    for dir in WRITE_FORBIDDEN {
        for inside in [dir.to_owned(), format!("{dir}/x"), format!("{dir}/a/b")] {
            for kind in [Kind::Write, Kind::State] {
                assert_eq!(
                    check(kind, &inside),
                    Err(Violation::Inside(dir)),
                    "{inside}"
                );
            }
            assert_eq!(check(Kind::Read, &inside), Ok(()), "{inside}");
        }
    }
    for near in ["/devices", "/processes", "/system", "/dev-x", "/sysfs"] {
        assert_eq!(check(Kind::Write, near), Ok(()), "{near}");
    }
}

/// Catches a write path outside the places a hook is given to write: only one component under
/// `/var/lib`, `/var/cache`, `/run` or `/opt`, and anything below that, may be named; the roots
/// themselves, `/srv`, `/home`, `/tmp`, `/var/log`, `/var/spool` and the like may not. State
/// and read entries are not held to the list.
#[test]
fn a_write_path_must_be_below_one_of_the_write_roots() {
    for path in [
        "/var/lib/x",
        "/var/lib/x/a/b",
        "/var/cache/x",
        "/var/cache/x/y",
        "/run/x",
        "/run/x/y",
        "/opt/x",
        "/opt/x/y/z",
        "/var/lib/updater/hooks/x",
    ] {
        assert_eq!(check(Kind::Write, path), Ok(()), "{path}");
    }
    for path in [
        "/var/lib",
        "/var/cache",
        "/run",
        "/opt",
        "/srv/x",
        "/home/x",
        "/tmp/x",
        "/var/log/x",
        "/var/spool/x",
        "/var/mail/x",
        "/varx/lib/y",
        "/opt2/x",
        "/run2/x",
    ] {
        assert_eq!(
            check(Kind::Write, path),
            Err(Violation::NotAllowed),
            "{path}"
        );
    }
    for path in ["/srv/x", "/home/x", "/tmp/x", "/var/log/x"] {
        assert_eq!(check(Kind::State, path), Ok(()), "{path}");
        assert_eq!(check(Kind::Read, path), Ok(()), "{path}");
    }
}

/// Catches the names the allow-list does not cover: the package database, the apt cache and the
/// cron spool sit inside or beside the allowed roots and are refused by the deny list; the system
/// directories of a host without a merged `/usr` are named and refused for every kind.
#[test]
fn the_deny_list_still_catches_names_inside_the_write_roots() {
    for (path, inside) in [
        ("/var/lib/dpkg/info/x", "/var/lib/dpkg"),
        ("/var/lib/polkit-1/x", "/var/lib/polkit-1"),
        ("/var/cache/apt/archives/x", "/var/cache/apt"),
        ("/var/spool/cron/crontabs/x", "/var/spool/cron"),
        ("/bin/x", "/bin"),
        ("/sbin/x", "/sbin"),
        ("/lib/x", "/lib"),
        ("/lib64/x", "/lib64"),
    ] {
        assert_eq!(
            check(Kind::Write, path),
            Err(Violation::Inside(inside)),
            "{path}"
        );
        assert_eq!(check(Kind::Read, path), Ok(()), "{path}");
    }
    for dir in [
        "/var/lib/dpkg",
        "/var/cache/apt",
        "/var/spool/cron",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
    ] {
        for kind in KINDS {
            assert!(
                matches!(check(kind, dir), Err(Violation::Names(_))),
                "{dir}"
            );
        }
    }
}
