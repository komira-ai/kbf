//! The clean's decisions over text, with no Mac.

use super::*;

/// Catches a job told to be launchd's own by its label (a lease chooses its labels) and
/// not by where its plist is: only a path under a sealed system folder or Apple's own
/// is launchd's. A plist in a home, or one that merely contains `/System/` later in the
/// path, or a missing path, is the lease's.
#[test]
fn a_job_is_launchds_own_by_the_folder_of_its_plist() {
    let printed = "com.apple.cfprefsd.xpc.agent = {\n\tactive count = 1\n\tpath = /System/Library/LaunchAgents/com.apple.cfprefsd.xpc.agent.plist\n\tstate = running\n\n\tinherited environment = {\n\t\tpath = /ignored\n\t}\n}\n";
    assert_eq!(
        plist_path(printed),
        Some("/System/Library/LaunchAgents/com.apple.cfprefsd.xpc.agent.plist")
    );
    assert_eq!(plist_path("x = {\n\t\tpath = /deeper\n}\n"), None);
    for own in [
        "/System/Library/LaunchAgents/a.plist",
        "/Library/Apple/System/Library/LaunchAgents/b.plist",
    ] {
        assert!(is_own_plist(own), "{own}");
    }
    for lease in [
        "/Volumes/pool/kbf-pool-0/Library/LaunchAgents/com.apple.fake.plist",
        "/Library/LaunchAgents/System.plist",
        "/tmp/System/x.plist",
        "/Library/Apple",
        "",
    ] {
        assert!(!is_own_plist(lease), "{lease}");
    }
}

/// Catches a services table read wrong: a running service's pid, a stopped one's `-`
/// and 0, a label with a dot, the lines outside the table, and a domain with no table.
#[test]
fn the_services_of_a_domain_are_the_table_between_its_braces() {
    let domain = "user/700 = {\n\ttype = user\n\tservices = {\n\t\t   123      -   com.apple.a\n\t\t     0      -   kbf.b\n\t\t     -     78   kbf.c\n\t}\n\n\tunrelated = {\n\t\t9 - nope\n\t}\n}\n";
    assert_eq!(
        listed_services(domain),
        [
            (Some(123), "com.apple.a".to_owned()),
            (None, "kbf.b".to_owned()),
            (None, "kbf.c".to_owned()),
        ]
    );
    assert_eq!(listed_services("user/700 = {\n}\n"), []);
    assert_eq!(listed_services(""), []);
}

/// Catches a shared sweep without `/Library/Caches` (a raw plant there by the uid survived
/// the clean), one that still sweeps all of `/private/var/folders` (the other uids'
/// entries are listed and searched), and one that lost a place or launchd's per-uid
/// files: the plan names exactly the five places and the two named files.
#[test]
fn the_shared_plan_covers_the_shared_places_and_caches_not_the_folders() {
    let plan = shared_plan();
    assert_eq!(
        plan.owned,
        [
            Path::new("/Users").join("Shared"),
            PathBuf::from("/private/tmp"),
            PathBuf::from("/private/var/tmp"),
            PathBuf::from("/Library/Caches"),
        ]
    );
    assert_eq!(
        plan.named,
        [
            "/private/var/db/com.apple.xpc.launchd/disabled.{uid}.plist",
            "/private/var/db/com.apple.xpc.launchd/loginitems.{uid}.plist"
        ]
    );
}

/// Catches a mount list read wrong: a device with spaces in its name, options after the
/// path, a mount that is only a prefix of the root (`kbf-pool-10` is not under
/// `kbf-pool-1`), and the order: a mount inside a mount first.
#[test]
fn mounts_under_the_roots_come_off_deepest_first() {
    let mount = "\
/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)
/dev/disk4s1 on /Volumes/kbf/home/kbf-pool-1/mnt (hfs, local, nodev, nosuid, mounted by u)
/dev/disk5s1 on /Volumes/kbf/home/kbf-pool-1/mnt/inner (hfs, local, nodev)
map auto home on /Volumes/kbf/home/kbf-pool-10/mnt (autofs)
/dev/disk6s1 on /private/var/folders/xx/yyyy/T/m (apfs)
garbage line
";
    let home = Path::new("/Volumes/kbf/home/kbf-pool-1");
    let entry = Path::new("/private/var/folders/xx/yyyy");
    assert_eq!(
        mounts_under(mount, &[home, entry]),
        [
            PathBuf::from("/private/var/folders/xx/yyyy/T/m"),
            PathBuf::from("/Volumes/kbf/home/kbf-pool-1/mnt/inner"),
            PathBuf::from("/Volumes/kbf/home/kbf-pool-1/mnt"),
        ]
    );
    assert_eq!(
        mounts_under(mount, &[Path::new("/nowhere")]),
        Vec::<PathBuf>::new()
    );
    assert_eq!(mounts_under("", &[home]), Vec::<PathBuf>::new());
}

/// Catches a check stricter than the restore (a root the system gave `nodump` or `opaque`
/// restored with them and then found wrong, so every clean of that slot failed), a
/// restore that gives back a lock or `hidden`, and one that drops the other flags: the
/// restore and the check agree on the same three bits.
#[test]
fn the_restore_and_its_check_agree_on_the_flags_they_never_leave() {
    const NODUMP: u32 = 0x1;
    const UCHG: u32 = 0x2;
    const UAPPND: u32 = 0x4;
    const OPAQUE: u32 = 0x8;
    const HIDDEN: u32 = 0x8000;
    const ARCHIVED: u32 = 0x10000;
    let recorded = NODUMP | UCHG | UAPPND | OPAQUE | HIDDEN | ARCHIVED;
    assert_eq!(kept_flags(recorded), NODUMP | OPAQUE | ARCHIVED);
    for flags in [NODUMP, OPAQUE, ARCHIVED, 0, kept_flags(recorded)] {
        assert!(!unrestored(flags), "{flags:#x}");
    }
    for flags in [UCHG, UAPPND, HIDDEN, recorded] {
        assert!(unrestored(flags), "{flags:#x}");
        assert!(!unrestored(kept_flags(flags)), "{flags:#x}");
    }
}

/// A scripted uid: `scans` are what the successive scans find (then nothing).
fn run_loop(
    scans: &[&[i32]],
    expire_after: usize,
    kill: Result<(), String>,
) -> (Result<(), String>, usize, usize) {
    let (mut kills, mut pauses, mut seen) = (0, 0, 0);
    let mut script = scans.iter();
    let result = kill_until_quiet(
        &mut || {
            seen += 1;
            seen > expire_after
        },
        &mut || {
            kills += 1;
            kill.clone()
        },
        &mut || Ok(script.next().map_or_else(Vec::new, |s| s.to_vec())),
        &mut || pauses += 1,
    );
    (result, kills, pauses)
}

/// Catches a kill loop that stops at the first empty scan (a respawn after the kill
/// would be missed: two empty scans, a pause apart, are the rule), one that does not
/// restart its count when a process shows up between two empty scans: empty, then a
/// process, then empty twice, is five rounds with the quiet pair at the end.
#[test]
fn the_kill_loop_ends_on_two_empty_scans_in_a_row() {
    assert_eq!(run_loop(&[], 100, Ok(())), (Ok(()), 2, 1));
    assert_eq!(
        run_loop(&[&[5], &[], &[6], &[], &[]], 100, Ok(())),
        (Ok(()), 5, 4)
    );
    assert_eq!(run_loop(&[&[5]], 100, Ok(())), (Ok(()), 3, 2));
}

/// Catches a kill loop with no bound (a process stuck in the kernel would hang the slot),
/// one whose bound is not the scan count, one that forgets the deadline, and an error
/// that hides the pids: processes that never go fail the loop at the 100th scan, or at
/// the deadline, naming them.
#[test]
fn the_kill_loop_is_bounded_by_scans_and_by_the_deadline() {
    let stuck: Vec<&[i32]> = vec![&[7]; 200];
    let (result, kills, pauses) = run_loop(&stuck, usize::MAX, Ok(()));
    assert_eq!(result, Err("processes [7] survived SIGKILL".to_owned()));
    assert_eq!((kills, pauses), (MAX_SCANS, MAX_SCANS));
    let (result, kills, pauses) = run_loop(&stuck, 3, Ok(()));
    assert_eq!(result, Err("processes [7] survived SIGKILL".to_owned()));
    assert_eq!((kills, pauses), (4, 3));
    // Out of time while quiet once: not quiet twice.
    let (result, _, _) = run_loop(&[], 0, Ok(()));
    assert_eq!(
        result,
        Err("the uid was not quiet for two scans".to_owned())
    );
}

/// Catches a kill that fails and is ignored (the scans would then judge a live uid) and a
/// scan that fails and is read as empty: both end the loop with their error.
#[test]
fn a_failed_kill_or_scan_ends_the_loop_with_its_error() {
    let (result, kills, _) = run_loop(&[], 100, Err("setuid".to_owned()));
    assert_eq!((result, kills), (Err("setuid".to_owned()), 1));
    let result = kill_until_quiet(
        &mut || false,
        &mut || Ok(()),
        &mut || Err("table".to_owned()),
        &mut || {},
    );
    assert_eq!(result, Err("table".to_owned()));
}
