//! The assertion's parsers on fixtures of what the Mac's tools print, its comparison with
//! a baseline, and what a red result does to the slot book: all on every platform, with a
//! fake [`Reader`].

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::*;
use crate::slots::book::{Backoff, Book, Cleaner, Slotted};
use crate::slots::clean::Outcome;

fn lines(texts: &[&str]) -> Lines {
    texts.iter().map(|t| (*t).to_owned()).collect()
}

/// A reader that answers what a test put in it, and counts nothing it was not asked.
#[derive(Default)]
struct Fake(Mutex<BTreeMap<Probe, Result<Lines, String>>>);

impl Fake {
    /// A user with nothing wrong: every probe answers its clean lines.
    fn clean() -> Self {
        let fake = Self::default();
        for probe in ALL {
            fake.set(probe, &clean_lines(probe));
        }
        fake
    }

    fn set(&self, probe: Probe, answer: &[&str]) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(probe, Ok(lines(answer)));
    }

    fn fail(&self, probe: Probe, why: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(probe, Err(why.to_owned()));
    }
}

impl Reader for Fake {
    fn read(&self, probe: Probe) -> Result<Lines, String> {
        let answers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        answers
            .get(&probe)
            .cloned()
            .unwrap_or_else(|| Err("not answered".to_owned()))
    }
}

/// What a clean used-once user reads for `probe`.
fn clean_lines(probe: Probe) -> Vec<&'static str> {
    match probe {
        Probe::Entry => vec![". owned by 61001:20 mode 700 flags 0x0"],
        Probe::Record => vec!["UniqueID=61001", "id uid=61001(u) gid=20(staff)"],
        Probe::Services => vec![
            "com.apple.cfprefsd.xpc.agent",
            "com.apple.distnoted.xpc.agent",
        ],
        _ => Vec::new(),
    }
}

fn baseline() -> Snapshot {
    record(&Fake::clean()).expect("a clean user is a baseline")
}

const PS: &str = "\
    1     0     0     0 Ss   /sbin/launchd
  345   501   501   501 Ss   /usr/libexec/distnoted
61001 61001 61001 61001 S    /bin/sleep
61002 61001     0     0 S    /usr/bin/su
61003     0     0 61001 S    /x/saved uid
61004 61001 61001 61001 Z    /bin/dead
61005     0 61001     0 S    /x/effective only
";

/// Catches a process table read as the uid alone (a setuid-root program the uid started,
/// whose effective uid is root's, would not be found; nor one that dropped to the uid as
/// its saved uid), a zombie counted as a process (it cannot be killed and goes when its
/// parent reaps it), a garbled table taken as an empty one, and a command with spaces cut.
#[test]
fn processes_are_found_by_real_effective_or_saved_uid() {
    let procs = parse_ps(PS).expect("parsed");
    assert_eq!(procs.len(), 7);
    assert_eq!(
        uid_processes(&procs, 61001, &[]),
        lines(&[
            "61001 /bin/sleep",
            "61002 /usr/bin/su",
            "61003 /x/saved uid",
            "61005 /x/effective only"
        ])
    );
    assert!(uid_processes(&procs, 61005, &[]).is_empty());
    assert!(parse_ps("").is_err(), "an empty table is a failed read");
    assert!(parse_ps("12 x 0 0 S /bin/sh").is_err());
    assert!(parse_ps("12 0 0").is_err());
    assert!(parse_ps("12 0 0 0").is_err(), "no state");
    assert!(
        parse_ps("99999999999 0 0 0 S /x").is_err(),
        "a pid past i32"
    );
}

/// Catches launchd's own agents counted as a lease's processes (they start again on
/// demand as the uid after every kill): the pids of own services are left out, and only
/// those.
#[test]
fn launchds_own_processes_are_not_the_leases() {
    let procs = parse_ps(PS).expect("parsed");
    assert!(uid_processes(&procs, 501, &[345]).is_empty());
    assert_eq!(
        uid_processes(&procs, 501, &[]),
        lines(&["345 /usr/libexec/distnoted"])
    );
    assert_eq!(uid_processes(&procs, 61001, &[61001]).len(), 3);
}

fn job(label: &str, pid: Option<i32>, own: bool) -> Job {
    Job {
        pid,
        label: label.to_owned(),
        own,
    }
}

/// Catches a job judged by its label (a lease can name its agent `com.apple.x`): the
/// judgement is the job's `own`, which the reader takes from its plist path, so a
/// foreign job labelled like launchd's is listed and an own one is not; the pids of own
/// services are those the processes probe leaves out.
#[test]
fn a_job_is_launchds_own_by_its_plist_not_its_label() {
    let jobs = [
        job("com.apple.cfprefsd.xpc.agent", Some(77), true),
        job("com.apple.kbf-fresh", Some(88), false),
        job("kbf.fresh", None, false),
    ];
    assert_eq!(
        foreign_jobs(&jobs),
        lines(&["job com.apple.kbf-fresh (pid 88)", "job kbf.fresh"])
    );
    assert_eq!(own_pids(&jobs), vec![77]);
    assert_eq!(own_labels(&jobs), lines(&["com.apple.cfprefsd.xpc.agent"]));
}

const IPCS: &str = "\
IPC status from <running system> as of Thu Oct 22 10:00:00 UTC 2026
T     ID     KEY        MODE       OWNER    GROUP  CREATOR   CGROUP CBYTES  QNUM QBYTES LSPID LRPID STIME    RTIME    CTIME
Message Queues:
q  none 0x00000009 --rw------- alice staff alice staff 0 0 2048 0 0 no-entry no-entry 10:00:00
q  100 0x00000001 --rw------- alice staff alice staff 0 0 2048 0 0 no-entry no-entry 10:00:00
Shared Memory:
m  200 0x00000002 --rw------- root wheel kbf-s staff 4096 0 0 0 0 no-entry no-entry 10:00:00
m  201 0x00000003 --rw------- kbf-s staff root wheel 4096 0 0 0 0 no-entry no-entry 10:00:00
Semaphores:
s  300 0x00000004 --rw------- 61001 staff 61001 staff 0 0 0 0 0 no-entry no-entry 10:00:00
s  301 0x00000005 --rw------- bob staff bob staff 0 0 0 0 0 no-entry no-entry 10:00:00
";

/// Catches `ipcs` rows matched on the owner alone (an object the uid made and gave away
/// stays), on the name alone (a name the user database does not give is its uid), or the
/// header lines and section titles taken as objects.
#[test]
fn ipc_objects_are_found_by_owner_or_creator() {
    let ids = |name, uid| -> Vec<(char, String)> {
        ipc_rows(IPCS, name, uid)
            .into_iter()
            .map(|r| (r.kind, r.id))
            .collect()
    };
    assert_eq!(
        ids("kbf-s", 61001),
        vec![
            ('m', "200".to_owned()),
            ('m', "201".to_owned()),
            ('s', "300".to_owned())
        ]
    );
    assert_eq!(ids("alice", 5), vec![('q', "100".to_owned())]);
    assert!(ids("nobody", 9).is_empty());
}

const HDIUTIL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>images</key><array>
<dict><key>image-path</key><string>/private/var/pool/u/a.dmg</string><key>owner-uid</key><integer>61001</integer>
<key>system-entities</key><array>
<dict><key>dev-entry</key><string>/dev/disk4s1</string></dict>
<dict><key>dev-entry</key><string>/dev/disk4</string></dict></array></dict>
<dict><key>image-path</key><string>/private/var/folders/xx/yy/T/b.dmg</string><key>owner-uid</key><integer>0</integer>
<key>system-entities</key><array>
<dict><key>dev-entry</key><string>/dev/disk5</string><key>mount-point</key><string>/private/var/folders/xx/yy/T/vol</string></dict></array></dict>
<dict><key>image-path</key><string>/Applications/other.dmg</string><key>owner-uid</key><integer>0</integer>
<key>system-entities</key><array>
<dict><key>dev-entry</key><string>/dev/disk6</string><key>mount-point</key><string>/Volumes/other</string></dict></array></dict>
<dict><key>image-path</key><string>/Applications/third.dmg</string><key>owner-uid</key><integer>0</integer>
<key>system-entities</key><array>
<dict><key>dev-entry</key><string>/dev/disk7</string><key>mount-point</key><string>/private/var/pool/u/vol</string></dict></array></dict>
</array></dict></plist>"#;

/// Catches an attached image the uid made with no mount point (`attach -nomount` shows in
/// no `mount` line), an image or a mount under the folders the clean empties, the
/// partition taken for the image (the whole device is the shortest), and an unrelated
/// image or mount counted.
#[test]
fn mounts_and_attached_images_under_the_users_folders_are_found() {
    let images = hdiutil_images(HDIUTIL.as_bytes()).expect("a property list");
    assert_eq!(images.len(), 4);
    assert_eq!(images[0].device.as_deref(), Some("/dev/disk4"));
    assert_eq!(images[0].owner, Some(61001));
    assert_eq!(images[1].mounts, vec!["/private/var/folders/xx/yy/T/vol"]);
    let mount = "\
/dev/disk3s1 on / (apfs, sealed, local)
/dev/disk5 on /private/var/folders/xx/yy/T/vol (hfs, local)
/dev/disk6 on /Volumes/other (hfs, local)
";
    let home = Path::new("/private/var/pool/u");
    let folders = Path::new("/private/var/folders/xx/yy");
    assert_eq!(
        mount_lines(mount, &images, 61001, &[home, folders]),
        lines(&[
            "mounted /private/var/folders/xx/yy/T/vol",
            "image /dev/disk4 attached by the uid",
            "image /dev/disk5 under the user's folders",
            "image /dev/disk7 under the user's folders",
        ])
    );
    assert!(
        mount_lines(
            "/dev/d on / (apfs)\n",
            &images[2..3],
            61001,
            &[home, folders]
        )
        .is_empty()
    );
    assert!(hdiutil_images(b"not a plist").is_err());
}

/// Catches an ACL on the home read as the listing's first line (the directory itself),
/// and a listing with none read as one.
#[test]
fn acl_entries_are_the_numbered_lines_after_the_first() {
    let listing = "drwx------+ 2 u staff 64 Oct 22 10:00 /home\n 0: user:x allow list,add_file\n 1: group:y deny delete\n";
    assert_eq!(
        acl_entries(listing),
        lines(&["0: user:x allow list,add_file", "1: group:y deny delete"])
    );
    assert!(acl_entries("drwx------ 2 u staff 64 Oct 22 10:00 /home\n").is_empty());
    // Lines that look a little like an entry are not one.
    let odd = "drwx------ 2 u staff 64 Oct 22 10:00 /home\ntotal 3\nname: value\n: nothing\n 7: \n";
    assert!(acl_entries(odd).is_empty());
}

fn facts() -> Facts {
    Facts {
        uid: 61001,
        gid: 20,
        mode: 0o700,
        ..Facts::default()
    }
}

/// Catches a home that is judged on part of what the assertion promises: each of the
/// owner, the group, the mode, a flag, an ACL, an extended attribute and a file makes a
/// line of its own, and an empty home of the user's makes none.
#[test]
fn a_home_is_empty_0700_owned_and_has_no_flag_acl_or_xattr() {
    assert!(home_lines(&facts(), (61001, 20)).is_empty());
    let cases: [(Facts, &str); 7] = [
        (Facts { uid: 0, ..facts() }, "owned by 0:20, not 61001:20"),
        (
            Facts { gid: 80, ..facts() },
            "owned by 61001:80, not 61001:20",
        ),
        (
            Facts {
                mode: 0o755,
                ..facts()
            },
            "mode 755, not 700",
        ),
        (
            Facts {
                flags: 0x2,
                ..facts()
            },
            "flags 0x2",
        ),
        (
            Facts {
                acl: lines(&["0: user:x allow list"]),
                ..facts()
            },
            "acl 0: user:x allow list",
        ),
        (
            Facts {
                xattrs: lines(&["com.apple.x"]),
                ..facts()
            },
            "xattr com.apple.x",
        ),
        (
            Facts {
                entries: lines(&["a.txt"]),
                ..facts()
            },
            "entry a.txt",
        ),
    ];
    for (found, line) in cases {
        assert_eq!(home_lines(&found, (61001, 20)), lines(&[line]), "{line}");
    }
}

/// Catches an entry that is only listed (a name in the entry root that is not one of the
/// four folders stays, a folder with a file in it stays), one whose folders another user
/// owns, and a root recorded without its owner, mode and flags.
#[test]
fn the_entry_keeps_its_four_folders_empty_and_nothing_else() {
    let root = Facts {
        mode: 0o755,
        ..facts()
    };
    let kept = |name: &str| (name.to_owned(), true, facts());
    let skeleton = ["0", "C", "T", "X"];
    let clean = entry_lines(&root, &[kept("0"), kept("C"), kept("T")], &skeleton, 61001);
    assert_eq!(clean, lines(&[". owned by 61001:20 mode 755 flags 0x0"]));
    let full = (
        "T".to_owned(),
        true,
        Facts {
            entries: lines(&["leak"]),
            ..facts()
        },
    );
    let stray = ("stray".to_owned(), false, Facts::default());
    let dir_stray = ("Z".to_owned(), true, facts());
    let foreign = ("0".to_owned(), true, Facts { uid: 0, ..facts() });
    let locked = (
        "C".to_owned(),
        true,
        Facts {
            flags: 0x2,
            ..facts()
        },
    );
    let found = entry_lines(
        &root,
        &[full, stray, dir_stray, foreign, locked],
        &skeleton,
        61001,
    );
    assert_eq!(
        found,
        lines(&[
            ". owned by 61001:20 mode 755 flags 0x0",
            "T: entry leak",
            "stray: not part of the entry",
            "Z: not part of the entry",
            "0: owned by 0",
            "C: flags 0x2",
        ])
    );
}

/// Catches a record read with the wrong attribute (`RecordName` for `RealName`), a value
/// on the line after its key lost, and an attribute that vanished compared as equal:
/// missing is an empty value, which differs.
#[test]
fn the_directory_record_is_read_by_attribute() {
    let said = "NFSHomeDirectory: /private/var/pool/x\nPrimaryGroupID: 20\nnot a pair\n indented: 1\nRealName:\n The X\nRecordName: x\nUniqueID: 61001\nUserShell:\n";
    assert_eq!(
        dscl_lines(said, &["RecordName", "UniqueID", "RealName", "UserShell"]),
        lines(&[
            "RecordName=x",
            "UniqueID=61001",
            "RealName=The X",
            "UserShell="
        ])
    );
}

/// Catches a baseline taken of a user that is not clean (a leftover process would be
/// recorded as normal and never found again), a probe that could not run recorded as an
/// empty one, and a baseline that leaves out the lines a later read is compared with.
#[test]
fn a_baseline_is_recorded_only_of_a_clean_user_and_every_probe() {
    let snapshot = baseline();
    assert_eq!(snapshot.lines(Probe::Record).map(Vec::len), Some(2));
    assert!(snapshot.lines(Probe::Services).is_some());
    for probe in ALL {
        assert!(snapshot.lines(probe).is_some(), "{probe}");
    }
    let dirty = Fake::clean();
    dirty.set(Probe::Processes, &["61001 /bin/sleep"]);
    assert_eq!(
        record(&dirty).expect_err("not clean"),
        lines(&["processes: + 61001 /bin/sleep"])
    );
    let blind = Fake::clean();
    blind.fail(Probe::Ipc, "ipcs: not found");
    let wrong = record(&blind).expect_err("a probe that cannot run");
    assert_eq!(wrong.len(), 1);
    assert!(wrong[0].starts_with("ipc: cannot run"), "{wrong:?}");
}

/// Catches a comparison that tolerates nothing launchd varies (a pid, a service loaded
/// on demand, a service that was not loaded yet) and one that tolerates too much: the
/// services differ from the baseline's and the user is still clean, while a foreign job,
/// a process, an IPC object, a mount, a dirty home, a changed record, a changed entry
/// root and an owned file each make a red line of their own.
#[test]
fn the_comparison_tolerates_launchds_services_and_nothing_a_lease_can_make() {
    let base = baseline();
    let now = Fake::clean();
    now.set(Probe::Services, &["com.apple.other.agent"]);
    assert!(
        assert(&now, Some(&base), &ALL).is_empty(),
        "launchd's own services vary"
    );
    let cases: [(Probe, &[&str], &str); 8] = [
        (
            Probe::Processes,
            &["9 /bin/sleep"],
            "processes: + 9 /bin/sleep",
        ),
        (Probe::Domain, &["job kbf.x"], "launchd: + job kbf.x"),
        (Probe::Ipc, &["s 300"], "ipc: + s 300"),
        (Probe::Mounts, &["mounted /m"], "mounts: + mounted /m"),
        (Probe::Home, &["entry a"], "home: + entry a"),
        (
            Probe::Shared,
            &["/private/tmp/x"],
            "shared: + /private/tmp/x",
        ),
        (
            Probe::Schedules,
            &["/private/var/at/tabs/u"],
            "schedules: + /private/var/at/tabs/u",
        ),
        (
            Probe::Record,
            &["UniqueID=61002", "id uid=61001(u) gid=20(staff)"],
            "record: + UniqueID=61002",
        ),
    ];
    for (probe, found, line) in cases {
        let now = Fake::clean();
        now.set(probe, found);
        let red = assert(&now, Some(&base), &ALL);
        assert!(red.contains(&line.to_owned()), "{probe}: {red:?}");
    }
    let now = Fake::clean();
    now.set(Probe::Record, &["id uid=61001(u) gid=20(staff)"]);
    let red = assert(&now, Some(&base), &[Probe::Record]);
    assert_eq!(
        red,
        lines(&["record: - UniqueID=61001"]),
        "a line gone is a difference"
    );
    let now = Fake::clean();
    now.set(Probe::Entry, &[". owned by 61001:20 mode 700 flags 0x2"]);
    let red = assert(&now, Some(&base), &[Probe::Entry]);
    assert_eq!(
        red,
        lines(&[
            "entry: + . owned by 61001:20 mode 700 flags 0x2",
            "entry: - . owned by 61001:20 mode 700 flags 0x0"
        ])
    );
}

/// Catches a probe that cannot run counted as green (the most dangerous way for the
/// assertion to fail), and one that needs a baseline passing without one.
#[test]
fn a_probe_that_cannot_run_or_has_no_baseline_is_red() {
    let base = baseline();
    for probe in ALL {
        let now = Fake::clean();
        now.fail(probe, "boom");
        let red = assert(&now, Some(&base), &[probe]);
        assert_eq!(red.len(), 1, "{probe}: {red:?}");
        assert!(
            red[0].starts_with(&format!("{probe}: cannot run")),
            "{red:?}"
        );
        assert!(red[0].contains("boom"));
    }
    let red = assert(
        &Fake::clean(),
        None,
        &[Probe::Entry, Probe::Record, Probe::Ipc],
    );
    assert_eq!(
        red,
        lines(&[
            "entry: no baseline to compare with",
            "record: no baseline to compare with"
        ])
    );
    assert!(assert(&Fake::clean(), None, &[Probe::Ipc]).is_empty());
}

/// Catches a cheap subset that leaves out one of the probes a slot is handed over with
/// (processes, launchd's jobs, IPC, mounts, the home and the entry), one that runs the
/// slow walks of the shared places at every lease, and a full list that misses a probe
/// or reads one twice.
#[test]
fn the_cheap_probes_are_the_hand_over_ones_and_all_probes_are_read_once() {
    for probe in [
        Probe::Processes,
        Probe::Domain,
        Probe::Ipc,
        Probe::Mounts,
        Probe::Home,
        Probe::Entry,
    ] {
        assert!(CHEAP.contains(&probe), "{probe}");
    }
    assert_eq!(CHEAP.len(), 6);
    for probe in [Probe::Shared, Probe::Schedules, Probe::Record] {
        assert!(!CHEAP.contains(&probe));
    }
    let mut all = ALL.to_vec();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), ALL.len());
    assert_eq!(all.len(), 10);
}

/// A slot of a book whose clean runs the assertion against a world a test controls.
struct Slot(u32);

impl Slotted for Slot {
    fn uid(&self) -> u32 {
        61000 + self.0
    }
}

/// A book over two slots whose cleaner asserts `world`: red is dirty, as the Mac's
/// cleaner makes it.
fn asserted_book(world: &Arc<Fake>, base: &Arc<Snapshot>) -> Arc<Book<Slot>> {
    let (world, base) = (Arc::clone(world), Arc::clone(base));
    let cleaner: Cleaner<Slot> = Arc::new(move |_: &Slot| {
        let red = assert(world.as_ref(), Some(&base), &ALL);
        if red.is_empty() {
            Outcome::Clean
        } else {
            Outcome::Dirty {
                why: format!("assertion: {}", red.join("; ")),
            }
        }
    });
    let never = Backoff {
        first: Duration::from_secs(3600),
        max: Duration::from_secs(3600),
    };
    Book::start(vec![Slot(0), Slot(1)], cleaner, never)
}

/// Catches a red assertion that still offers the slot (a lease would run on a user that
/// holds what the last one left), a count that does not drop, a recovery that frees the
/// slot while the user is still red, and one that never asserts again: with the user red
/// both slots are dirty and the count is 0; a re-clean that finds it still red frees
/// nothing; once it is green again the next re-clean frees both.
#[test]
fn a_red_assertion_keeps_the_slot_out_until_a_clean_and_assert_passes() {
    let base = Arc::new(baseline());
    let world = Arc::new(Fake::clean());
    world.set(Probe::Processes, &["61001 /bin/sleep"]);
    let book = asserted_book(&world, &base);
    assert_eq!(book.count(), 0);
    assert_eq!(book.dirty().len(), 2);
    assert_eq!(book.retry(), 0, "still red: nothing is freed");
    assert_eq!(book.count(), 0);
    world.set(Probe::Processes, &[]);
    assert_eq!(book.retry(), 2);
    assert_eq!(book.count(), 2);
}

/// Catches a slot-ready assertion that does not set the slot aside through the book
/// (the lease would take it anyway), and one that sets aside the wrong slot or all of
/// them: the taken slot is the one dirty, the other stays offered, and a later clean
/// that asserts green frees it.
#[tokio::test]
async fn a_slot_that_fails_the_assertion_at_slot_ready_is_set_aside() {
    let base = Arc::new(baseline());
    let world = Arc::new(Fake::clean());
    let book = asserted_book(&world, &base);
    assert_eq!(book.count(), 2);
    let (index, permit) = book.take().await.expect("a slot");
    // What a lease finds at slot-ready: a mount that was not there at the clean.
    world.set(Probe::Mounts, &["mounted /private/var/folders/x/T/vol"]);
    let red = assert(world.as_ref(), Some(&base), &CHEAP);
    assert_eq!(
        red,
        lines(&["mounts: + mounted /private/var/folders/x/T/vol"])
    );
    book.set_aside(index, permit, &format!("assertion: {}", red.join("; ")));
    assert_eq!(book.count(), 1);
    assert_eq!(book.dirty().into_iter().collect::<Vec<_>>(), vec![index]);
    assert_eq!(
        book.retry(),
        0,
        "the re-clean asserts again, and it is still red"
    );
    world.set(Probe::Mounts, &[]);
    assert_eq!(book.retry(), 1);
    assert_eq!(book.count(), 2);
}
