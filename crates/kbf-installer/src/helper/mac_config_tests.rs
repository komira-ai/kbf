//! The `kbf.toml` the Mac lease source reads ([`super`]): which files cannot be relied on, and
//! that a daemon in pool mode is read as one in daemon-user mode is.

use std::fs;
use std::os::unix::fs::symlink;

use kbf_host::exec::Entry;
use kbf_host::path::HostPath;
use kbf_host::step::{Owner, Perms};

use crate::fixtures::ran;
use crate::helper::mac_leases::tests::{KBF_TOML, MacHost, TOML, refused, reply_pids, root_owned};

/// The `[macos]` table of a root daemon that runs its actions as four lease users from uid 600
/// (`docs/design/macos-end-state.md`, section 1).
const POOL: &str = "[macos]\npool-users = 4\npool-first-uid = 600\n";

/// Catches an unreliable `kbf.toml` passing. Each of these cannot tell, naming the file or
/// the key: absent, a link or a directory where the file should be, not root's, group
/// writable, in a directory others can write, over 1 MiB, not text, not TOML, a mode that is
/// not `daemon`, no mode, a `driver` that is absent, not `["native"]` or holds more, no
/// `data-dir` or a relative one, and a file that is there for the trust check but gone for
/// the timestamp read.
///
/// Catches: the trust check dropped; the driver check dropped; the mode check dropped.
#[test]
fn a_kbf_toml_that_is_not_a_native_daemons_or_not_trusted_cannot_tell() {
    let toml_cases: Vec<(&str, String, &str)> = vec![
        (
            "mode",
            TOML.replace("mode = \"daemon\"", "mode = \"frontend\""),
            "mode is \"frontend\"",
        ),
        (
            "no mode",
            TOML.replace("mode = \"daemon\"\n", ""),
            "has no mode",
        ),
        (
            "no driver",
            TOML.replace("driver = [\"native\"]\n", ""),
            "driver is None",
        ),
        (
            "two drivers",
            TOML.replace("[\"native\"]", "[\"native\", \"native-whole-machine\"]"),
            "not [\"native\"]",
        ),
        (
            "other driver",
            TOML.replace("native", "container"),
            "not [\"native\"]",
        ),
        (
            "no data-dir",
            TOML.replace("data-dir = \"/Volumes/kbf/data\"\n", ""),
            "no absolute data-dir",
        ),
        (
            "relative",
            TOML.replace("/Volumes/kbf/data", "data"),
            "no absolute data-dir",
        ),
        (
            "not a string",
            TOML.replace("\"/Volumes/kbf/data\"", "7"),
            "no absolute data-dir",
        ),
        ("not toml", "mode = = \n".to_owned(), "is not TOML"),
        (
            "big",
            format!("{TOML}#{}\n", "x".repeat(1 << 20)),
            "is over",
        ),
    ];
    for (name, text, why) in toml_cases {
        let mut rig = MacHost::new("toml");
        rig.toml(&text);
        match rig.count() {
            Err(e) => assert!(e.contains(why), "{name}: {e}"),
            Ok(n) => panic!("{name}: answered {n}"),
        }
    }

    let file = |perms, uid, bytes: &[u8]| Entry::File {
        perms: Perms::new(perms),
        owner: Owner { uid, gid: 0 },
        bytes: bytes.to_vec(),
    };
    let entry_cases: Vec<(&str, Option<Entry>, &str)> = vec![
        ("absent", None, "is not there"),
        (
            "not utf-8",
            Some(file(0o644, 0, &[0xff, 0xfe])),
            "not UTF-8",
        ),
        (
            "a link",
            Some(Entry::Link { target: "x".into() }),
            "not a plain file",
        ),
        (
            "a directory",
            Some(Entry::Dir {
                perms: Perms::new(0o755),
                owner: root_owned(),
            }),
            "not a plain file",
        ),
        (
            "not root's",
            Some(file(0o644, 501, TOML.as_bytes())),
            "not trusted",
        ),
        (
            "group writable",
            Some(file(0o664, 0, TOML.as_bytes())),
            "not trusted",
        ),
    ];
    for (name, entry, why) in entry_cases {
        let mut rig = MacHost::new("entry");
        match entry {
            Some(entry) => {
                rig.exec
                    .entries
                    .insert(HostPath::new(KBF_TOML).expect("path"), entry);
            }
            None => {
                rig.exec
                    .entries
                    .remove(&HostPath::new(KBF_TOML).expect("path"));
            }
        }
        match rig.count() {
            Err(e) => assert!(e.contains(why), "{name}: {e}"),
            Ok(n) => panic!("{name}: answered {n}"),
        }
    }

    let mut rig = MacHost::new("entry-dir-writable");
    rig.exec.entries.insert(
        HostPath::new("/usr/local/etc").expect("path"),
        Entry::Dir {
            perms: Perms::new(0o775),
            owner: root_owned(),
        },
    );
    refused(rig.count(), "not trusted");

    let mut rig = MacHost::new("unreadable");
    rig.exec.entries.insert(
        HostPath::new("/usr/local/etc/kbf/kbf.toml").expect("path"),
        file(0o644, 0, TOML.as_bytes()),
    );
    fs::remove_file(rig.root.join("usr/local/etc/kbf/kbf.toml")).expect("remove");
    refused(rig.count(), "kbf.toml");
}

/// Catches a pool-mode Mac that takes no update order: a `kbf.toml` with the pool keys (both,
/// or either alone, beside other keys of the table) is read like one without them. An idle
/// pool host counts no lease and no note, so the release is activated; and a `[macos]` table
/// with no pool key is read the same way.
///
/// Catches: the pool keys refused ("pool mode leases are not read by this installer yet").
#[test]
fn a_pool_mode_mac_with_no_live_lease_reads_idle() {
    for table in [
        POOL.to_owned(),
        "[macos]\npool-users = 4\n".to_owned(),
        "[macos]\npool-first-uid = 600\n".to_owned(),
        "[macos]\nxcode-recheck-secs = 60\npool-users = 4\npool-first-uid = 600\n".to_owned(),
        "[macos]\nxcode-recheck-secs = 60\n".to_owned(),
    ] {
        let mut rig = MacHost::new("pool-idle");
        rig.toml(&format!("{TOML}{table}"));
        assert_eq!(rig.live(), Ok((0, None)), "{table}");
    }
}

/// Catches a live lease of a pool-mode Mac read as idle, so that the release would be
/// activated under it: the lease directory the root daemon makes for a slot user's lease
/// (`<data-dir>/leases/lease-<term>-<seq>`, the slot user's, mode 0700) counts as one lease,
/// and so does the keeper of a slot (`kbf-keeper __keeper uid <uid>`); one lease in both is
/// one, and two directories are two.
#[test]
fn a_pool_mode_mac_counts_its_lease_directories_and_its_slot_keepers() {
    let pool = |name: &str| {
        let mut rig = MacHost::new(name);
        rig.toml(&format!("{TOML}{POOL}"));
        rig
    };
    let mut rig = pool("pool-dir");
    rig.dir("lease-3-7");
    assert_eq!(rig.live(), Ok((1, None)));

    let mut rig = pool("pool-keeper");
    rig.keepers(reply_pids("412\n"));
    assert_eq!(rig.live(), Ok((1, None)));

    let mut rig = pool("pool-both");
    rig.keepers(reply_pids("412\n"));
    rig.dir("lease-3-7");
    rig.dir("lease-3-8");
    assert_eq!(rig.live(), Ok((2, None)));
}

/// Catches a pool-mode Mac whose lease records cannot be read taken as idle: a `leases/` that
/// is a link, is missing or is a file cannot tell, with the reason naming the directory, and
/// so does a keeper listing that failed. The text is the source's whole answer.
#[test]
fn a_pool_mode_mac_whose_lease_records_cannot_be_read_cannot_tell() {
    let pool = |name: &str| {
        let mut rig = MacHost::new(name);
        rig.toml(&format!("{TOML}{POOL}"));
        rig
    };
    let mut rig = pool("pool-link");
    fs::remove_dir(rig.leases()).expect("remove");
    fs::create_dir(rig.root.join("elsewhere")).expect("dir");
    symlink(rig.root.join("elsewhere"), rig.leases()).expect("link");
    assert_eq!(
        rig.count(),
        Err("/Volumes/kbf/data/leases is a link".to_owned())
    );

    let mut rig = pool("pool-file");
    fs::remove_dir(rig.leases()).expect("remove");
    fs::write(rig.leases(), "").expect("file");
    assert_eq!(
        rig.count(),
        Err("/Volumes/kbf/data/leases is not a directory".to_owned())
    );

    let mut rig = pool("pool-missing");
    fs::remove_dir(rig.leases()).expect("remove");
    refused(rig.count(), "/Volumes/kbf/data/leases: ");

    let mut rig = pool("pool-pgrep");
    rig.keepers(Some(ran(2, "", "")));
    assert_eq!(
        rig.count(),
        Err("/usr/bin/pgrep failed (status 2)".to_owned())
    );
}
