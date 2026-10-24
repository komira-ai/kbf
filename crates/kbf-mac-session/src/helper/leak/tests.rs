use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use super::*;
use crate::autologin::CommandOutput;
use crate::helper::Outcome;
use crate::helper::tests::{Rig, log, me, rig};
use crate::proto::Request;

const REBOOT: &str = "/bin/launchctl reboot system";
const DUMPBTM: &str = "/usr/bin/sfltool dumpbtm";

/// `sfltool dumpbtm` with one entry per identifier in `ids`, all under uid 0.
fn dumpbtm(ids: &[&str]) -> String {
    let mut text = "========================\n Records for UID 0 : 0000\n========================\n\n Items:\n\n".to_owned();
    for (n, id) in ids.iter().enumerate() {
        text.push_str(&format!(
            " #{}:\n                 UUID: {n}\n           Identifier: {id}\n                 Type: legacy daemon (0x10010)\n           Generation: {n}\n",
            n + 1
        ));
    }
    text
}

fn answer(rig: &Rig, command: &str, success: bool, stdout: &str, stderr: &str) {
    rig.host.state().answers.insert(
        command.to_owned(),
        CommandOutput {
            success,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        },
    );
}

/// Writes `text` to `path`, mode 0600 whatever the umask (the helper trusts no file
/// its group or others may write).
fn write_private(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Opens the baseline window as the apply in boot `boot-1` would.
fn open_window(rig: &Rig) {
    write_private(&rig.dir.join("baseline-window"), "boot-1\n");
}

/// A Mac with something in every class but auto-login (off at rest): a launch agent,
/// a launch daemon, a login items file (and launchd's `disabled` overrides, which
/// the login items class leaves out), a background task, a foreign user record in the
/// lease range.
fn furnished(name: &str) -> Rig {
    let rig = rig(name, 4);
    for folder in ["LaunchAgents", "LaunchDaemons", "launchd"] {
        std::fs::create_dir(rig.dir.join(folder)).unwrap();
    }
    std::fs::write(rig.dir.join("LaunchAgents/com.apple.a.plist"), "a").unwrap();
    std::fs::write(rig.dir.join("LaunchDaemons/com.apple.d.plist"), "d").unwrap();
    std::fs::write(rig.dir.join("launchd/loginitems.501.plist"), "l").unwrap();
    std::fs::write(rig.dir.join("launchd/disabled.501.plist"), "o").unwrap();
    answer(&rig, DUMPBTM, true, &dumpbtm(&["16.com.apple.btm"]), "");
    rig.host.state().foreign.insert(me() + 3);
    rig
}

/// A furnished Mac whose baseline is recorded.
fn baselined(name: &str) -> Rig {
    let rig = furnished(name);
    open_window(&rig);
    rig.helper.baseline().unwrap();
    log(&rig);
    rig
}

fn classes(pairs: &[(&str, usize)]) -> Vec<(String, usize)> {
    pairs.iter().map(|&(c, n)| (c.to_owned(), n)).collect()
}

/// Catches: a scan of an untouched Mac that is not clean (an item that changes by
/// itself, such as a BTM entry's UUID or generation, or a baseline written in another
/// form than it is read), or a baseline that leaves a class out of its counts.
#[test]
fn a_clean_tree_gives_an_empty_diff() {
    let rig = furnished("leak-clean");
    open_window(&rig);
    let counts = rig.helper.baseline().unwrap();
    assert_eq!(
        counts,
        classes(&[
            ("auto-login", 0),
            ("background-tasks", 1),
            ("launch-agents", 1),
            ("launch-daemons", 1),
            ("lease-users", 1),
            ("login-items", 1),
        ])
    );
    // The entry's UUID and generation change; nothing installed did.
    let moved = dumpbtm(&["16.com.apple.btm"]).replace("UUID: 0", "UUID: 9");
    answer(
        &rig,
        DUMPBTM,
        true,
        &moved.replace("Generation: 0", "Generation: 4"),
        "",
    );
    let report = rig.helper.scan().unwrap();
    assert_eq!(report, ScanReport::default());
    assert!(report.clean());
    // An empty Mac too: no folder, no entry.
    let empty = crate::helper::tests::rig("leak-empty", 2);
    open_window(&empty);
    empty.helper.baseline().unwrap();
    assert!(empty.helper.scan().unwrap().clean());
}

/// One planted leak per item class: what it plants, and the start of the line the scan
/// must add.
type Plant = fn(&Rig);

const PLANTS: [(&str, Plant, &str); 10] = [
    (
        "a launch agent",
        |rig| std::fs::write(rig.dir.join("LaunchAgents/evil.plist"), "x").unwrap(),
        "launch-agents \"evil.plist\" file ",
    ),
    (
        "a launch daemon",
        |rig| std::fs::write(rig.dir.join("LaunchDaemons/evil.plist"), "x").unwrap(),
        "launch-daemons \"evil.plist\" file ",
    ),
    (
        "a login items file",
        |rig| std::fs::write(rig.dir.join("launchd/loginitems.0.plist"), "x").unwrap(),
        "login-items \"loginitems.0.plist\" file ",
    ),
    (
        "a background task",
        |rig| {
            let two = dumpbtm(&["16.com.apple.btm", "8.com.evil"]);
            answer(rig, DUMPBTM, true, &two, "");
        },
        "background-tasks uid=0 id=\"8.com.evil\" type=\"legacy daemon (0x10010)\"",
    ),
    (
        "a user in the lease range",
        |rig| {
            rig.host.state().foreign.insert(me() + 1);
        },
        "lease-users uid ",
    ),
    (
        "auto-login",
        |rig| {
            let domain = rig.dir.join("loginwindow").display().to_string();
            let read = format!("/usr/bin/defaults read {domain} autoLoginUser");
            answer(rig, &read, true, "kbf-lease-9-9\n", "");
        },
        "auto-login user \"kbf-lease-9-9\"",
    ),
    (
        "a kcpassword",
        |rig| std::fs::write(rig.dir.join("kcpassword"), "x").unwrap(),
        "auto-login kcpassword present",
    ),
    (
        "an intent record",
        |rig| std::fs::write(rig.dir.join("session-intent"), "9.9\n").unwrap(),
        "auto-login intent 9.9",
    ),
    (
        "the VM user's intent record",
        |rig| std::fs::write(rig.dir.join("session-intent"), "vm\n").unwrap(),
        "auto-login intent vm",
    ),
    (
        "a launch daemon's new contents",
        |rig| std::fs::write(rig.dir.join("LaunchDaemons/com.apple.d.plist"), "e").unwrap(),
        "launch-daemons \"com.apple.d.plist\" file ",
    ),
];

/// Catches: a scan that ignores an item class (one plant per class, the leak scan's
/// planted-leak rule in fleet-updates.md 10.2), a daemon whose contents changed under
/// the same name passed as clean, or a plant that is found but leaves the scan clean.
#[test]
fn every_planted_leak_is_reported_as_a_diff() {
    for (n, (what, plant, line)) in PLANTS.iter().enumerate() {
        let rig = baselined(&format!("leak-plant-{n}"));
        plant(&rig);
        let report = rig.helper.scan().unwrap();
        assert!(!report.clean(), "{what}: {report:?}");
        assert!(
            report.diff.added.iter().any(|item| item.starts_with(line)),
            "{what}: no {line:?} in {report:#?}"
        );
        assert!(report.errors.is_empty(), "{what}: {report:?}");
    }
}

/// Catches: an item gone since the baseline (a system daemon a lease deleted) passed
/// as clean, a link followed instead of recorded, or a directory, a socket or a
/// `disabled` override taken for a file; and a mode or owner change missed.
#[test]
fn removals_links_and_odd_entries_are_items() {
    let rig = baselined("leak-kinds");
    std::fs::remove_file(rig.dir.join("LaunchAgents/com.apple.a.plist")).unwrap();
    let daemons = rig.dir.join("LaunchDaemons");
    std::os::unix::fs::symlink("/etc/passwd", daemons.join("link.plist")).unwrap();
    std::fs::create_dir(daemons.join("sub")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(daemons.join("sock")).unwrap();
    std::fs::write(rig.dir.join("launchd/disabled.0.plist"), "x").unwrap();
    let report = rig.helper.scan().unwrap();
    assert_eq!(report.diff.removed.len(), 1, "{report:#?}");
    assert!(report.diff.removed[0].starts_with("launch-agents \"com.apple.a.plist\" file "));
    let added = &report.diff.added;
    assert_eq!(added.len(), 3, "{report:#?}");
    assert!(added[0].starts_with("launch-daemons \"link.plist\" link "));
    assert!(added[0].ends_with(" -> \"/etc/passwd\""), "{added:?}");
    assert!(added[1].starts_with("launch-daemons \"sock\" other "));
    assert!(added[2].starts_with("launch-daemons \"sub\" dir "));
    // Mode 0600 instead of the file's 0644: the line changes.
    let rig = baselined("leak-mode");
    let file = rig.dir.join("LaunchDaemons/com.apple.d.plist");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let report = rig.helper.scan().unwrap();
    assert_eq!((report.diff.added.len(), report.diff.removed.len()), (1, 1));
    assert!(report.diff.added[0].contains(" 600 sha256:"), "{report:?}");
}

/// Catches: a file hashed only in part (the read loop stopping after its first 64 KiB
/// buffer, or after any read short of the end), so that a launch daemon over 64 KiB
/// whose tail a lease rewrote scans clean; and a hash that is not the SHA-256 of the
/// whole file. The file spans three full reads and a short fourth; each change sits
/// in a later read than the first.
#[test]
fn a_change_past_the_first_read_of_a_large_file_is_seen() {
    use sha2::Digest as _;
    const READ: usize = 64 * 1024;
    let size = 3 * READ + 7;
    let rig = furnished("leak-large");
    let file = rig.dir.join("LaunchDaemons/big.plist");
    std::fs::write(&file, vec![b'a'; size]).unwrap();
    open_window(&rig);
    rig.helper.baseline().unwrap();
    for (n, at) in [READ, 2 * READ, size - 1].into_iter().enumerate() {
        let mut body = vec![b'a'; size];
        body[at] = b'b';
        std::fs::write(&file, &body).unwrap();
        let report = rig.helper.scan().unwrap();
        assert!(!report.clean(), "byte {at}: {report:?}");
        let sha = hex::encode(sha2::Sha256::digest(&body));
        assert_eq!(report.diff.added.len(), 1, "byte {at}: {report:#?}");
        let line = &report.diff.added[0];
        assert!(
            line.starts_with("launch-daemons \"big.plist\" file "),
            "{line}"
        );
        assert!(
            line.ends_with(&format!("sha256:{sha}")),
            "byte {at}: {line}"
        );
        assert_eq!(report.diff.removed.len(), 1, "byte {at}: {report:#?}");
        // Back to the baselined contents before the next change.
        std::fs::write(&file, vec![b'a'; size]).unwrap();
        assert!(rig.helper.scan().unwrap().clean(), "restore {n}");
    }
}

/// Catches: an item that cannot be read taken as an empty class (a leak hidden by a
/// failing `sfltool`, an unreadable folder or a broken directory lookup would pass),
/// or a baseline recorded with a class missing.
#[test]
fn an_unreadable_item_makes_the_scan_dirty_and_the_baseline_refused() {
    let breaks: [(&str, Plant, &str); 7] = [
        (
            "a folder that is a file",
            |rig| {
                std::fs::remove_dir_all(rig.dir.join("LaunchAgents")).unwrap();
                std::fs::write(rig.dir.join("LaunchAgents"), "").unwrap();
            },
            "launch-agents ",
        ),
        (
            "sfltool fails",
            |rig| answer(rig, DUMPBTM, false, "", "no access"),
            "dumpbtm failed: no access",
        ),
        (
            "sfltool prints no record",
            |rig| answer(rig, DUMPBTM, true, "what\n", ""),
            "no \"Records for UID\" header",
        ),
        (
            "the directory lookup fails",
            |rig| {
                rig.host.state().fail.insert("uid_taken");
            },
            "lease-users: looking up uid ",
        ),
        (
            "defaults fails for another reason",
            |rig| {
                let domain = rig.dir.join("loginwindow").display().to_string();
                let read = format!("/usr/bin/defaults read {domain} autoLoginUser");
                answer(rig, &read, false, "", "busy");
            },
            "failed: busy",
        ),
        (
            "the intent record is broken",
            |rig| std::fs::write(rig.dir.join("session-intent"), "garbage").unwrap(),
            "auto-login: ",
        ),
        (
            "no program runs",
            |rig| {
                rig.host.state().fail.insert("command");
            },
            "running /usr/bin/sfltool: command failed",
        ),
    ];
    for (n, (what, broken, says)) in breaks.iter().enumerate() {
        let rig = baselined(&format!("leak-broken-{n}"));
        broken(&rig);
        let report = rig.helper.scan().unwrap();
        assert!(!report.clean(), "{what}: {report:?}");
        assert!(
            report.errors.iter().any(|error| error.contains(says)),
            "{what}: no {says:?} in {report:#?}"
        );
        let error = rig.helper.baseline().unwrap_err();
        assert!(error.contains("would be incomplete"), "{what}: {error}");
    }
}

/// Catches: a file the helper cannot open (a mode 000 plist planted by an admin
/// lease) skipped rather than reported, or a `/etc/kcpassword` that cannot be looked
/// at taken as absent. Root opens any file, so this needs another user.
#[test]
fn an_unreadable_file_is_an_error() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let rig = baselined("leak-mode000");
    let file = rig.dir.join("LaunchDaemons/locked.plist");
    std::fs::write(&file, "x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    let report = rig.helper.scan().unwrap();
    assert!(
        report.errors[0].contains("locked.plist: Permission denied"),
        "{report:?}"
    );
    // The state directory stops being searchable once the scan has read its baseline
    // (as `defaults read` runs, just before kcpassword is looked at).
    let dir = rig.dir.clone();
    rig.host.state().on_command = Some(Box::new(move |command: &str| {
        if command.starts_with("/usr/bin/defaults read") {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }));
    let report = rig.helper.scan().unwrap();
    std::fs::set_permissions(rig.dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("kcpassword: Permission denied")),
        "{report:?}"
    );
}

/// Catches: a scan with no baseline taken as clean (everything would be "added", or
/// nothing compared), or a baseline file that is not the helper's own, or not one,
/// trusted.
#[test]
fn a_scan_needs_a_recorded_baseline_of_its_own() {
    let rig = furnished("leak-nobase");
    let error = rig.helper.scan().unwrap_err();
    assert!(error.contains("no baseline was recorded"), "{error}");
    let path = rig.dir.join("scan-baseline");
    write_private(&path, "something else\n");
    let error = rig.helper.scan().unwrap_err();
    assert!(error.contains("not a baseline"), "{error}");
    write_private(&path, "kbf-scan-baseline-v1\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o620)).unwrap();
    let error = rig.helper.scan().unwrap_err();
    assert!(error.contains("writable by its group or others"), "{error}");
}

/// Catches: a baseline recorded outside its window: with no apply or erase behind it,
/// in the very boot of the apply (before the reboot that settles it), while a lease
/// user exists, or when this boot cannot be told; and a malformed window taken as open.
#[test]
fn baseline_outside_its_window_is_refused() {
    let rig = furnished("leak-window");
    let error = rig.helper.baseline().unwrap_err();
    assert!(error.contains("window is closed"), "{error}");
    let window = rig.dir.join("baseline-window");
    write_private(&window, "boot-2\n");
    let error = rig.helper.baseline().unwrap_err();
    assert!(error.contains("opened in this boot (boot-2)"), "{error}");
    for bad in ["", "\n", "boot-1", "boot-1\nboot-0\n"] {
        write_private(&window, bad);
        let error = rig.helper.baseline().unwrap_err();
        assert!(error.contains("does not name one boot"), "{bad:?}: {error}");
    }
    open_window(&rig);
    rig.host.state().fail.insert("boot_session");
    let error = rig.helper.baseline().unwrap_err();
    assert!(error.contains("this boot's identifier"), "{error}");
    rig.host.state().fail.clear();
    rig.helper.user_create("1.1", None).unwrap();
    // The user exists and the window is opened again (an apply while it lived).
    open_window(&rig);
    let error = rig.helper.baseline().unwrap_err();
    assert!(error.contains("lease 1.1's user exists"), "{error}");
    assert!(!rig.dir.join("scan-baseline").exists());
    rig.helper.kill_uid("1.1").unwrap();
    rig.helper.user_delete("1.1").unwrap();
    assert!(rig.helper.baseline().is_ok());
}

/// Catches: a baseline accepted after a lease user existed this boot (the lease's
/// leaks would become the baseline): the first `user-create` must close the window,
/// and a create that cannot close it must not create.
#[test]
fn the_first_lease_user_closes_the_window() {
    let rig = furnished("leak-first");
    open_window(&rig);
    rig.helper.user_create("1.1", None).unwrap();
    assert!(!rig.dir.join("baseline-window").exists());
    rig.helper.kill_uid("1.1").unwrap();
    rig.helper.user_delete("1.1").unwrap();
    let error = rig.helper.baseline().unwrap_err();
    assert!(error.contains("window is closed"), "{error}");
    // A window that cannot be removed (a directory with something in it).
    std::fs::create_dir_all(rig.dir.join("baseline-window/x")).unwrap();
    let error = rig.helper.user_create("1.2", None).unwrap_err();
    assert!(error.contains("closing "), "{error}");
    assert!(rig.host.state().users.is_empty());
    std::fs::remove_dir_all(rig.dir.join("baseline-window")).unwrap();
    assert_eq!(rig.helper.user_create("1.2", None), Ok(me() + 1));
}

/// Catches: a baseline that cannot be written reported as recorded.
#[test]
fn a_baseline_that_cannot_be_written_is_refused() {
    let rig = furnished("leak-unwritable");
    open_window(&rig);
    std::fs::create_dir_all(rig.dir.join("scan-baseline.new/x")).unwrap();
    let error = rig.helper.baseline().unwrap_err();
    assert!(error.contains("writing "), "{error}");
}

/// Catches: `reboot-dirty` accepted without a dirty scan of the helper's own (the
/// daemon could reboot a node at will), on a stale verdict (a later clean or refused
/// scan, a new baseline, or a lease user created since), or twice on one verdict; and
/// a failed reboot reported as done.
#[test]
fn reboot_dirty_needs_the_helpers_own_dirty_scan() {
    // A function, not a closure, so a failure names the line of the step it checks.
    #[track_caller]
    fn refused(rig: &Rig) {
        let error = rig.helper.reboot_dirty().unwrap_err();
        assert!(error.contains("did not come back dirty"), "{error}");
        assert!(log(rig).iter().all(|call| !call.contains(REBOOT)));
    }
    let rig = baselined("leak-reboot");
    refused(&rig);
    rig.helper.scan().unwrap();
    refused(&rig);
    let plant = rig.dir.join("LaunchDaemons/evil.plist");
    std::fs::write(&plant, "x").unwrap();
    assert!(!rig.helper.scan().unwrap().clean());
    rig.helper.reboot_dirty().unwrap();
    assert!(
        log(&rig)
            .iter()
            .any(|call| call.starts_with(&format!("command {REBOOT}")))
    );
    refused(&rig);
    // Dirty, then clean again: stale.
    rig.helper.scan().unwrap();
    std::fs::remove_file(&plant).unwrap();
    rig.helper.scan().unwrap();
    refused(&rig);
    // Dirty, then a lease user: stale (the reboot would cut the new lease).
    std::fs::write(&plant, "x").unwrap();
    rig.helper.scan().unwrap();
    rig.helper.user_create("1.1", None).unwrap();
    refused(&rig);
    // Dirty, then a scan refused for want of a baseline.
    rig.helper.scan().unwrap();
    std::fs::remove_file(rig.dir.join("scan-baseline")).unwrap();
    assert!(rig.helper.scan().is_err());
    refused(&rig);
    // Dirty, then a new baseline (which takes the plant in).
    rig.helper.kill_uid("1.1").unwrap();
    rig.helper.user_delete("1.1").unwrap();
    open_window(&rig);
    rig.helper.baseline().unwrap();
    std::fs::write(rig.dir.join("LaunchDaemons/evil2.plist"), "x").unwrap();
    rig.helper.scan().unwrap();
    open_window(&rig);
    rig.helper.baseline().unwrap();
    refused(&rig);
    // The reboot itself fails, or launchctl cannot run.
    std::fs::write(rig.dir.join("LaunchDaemons/evil3.plist"), "x").unwrap();
    rig.helper.scan().unwrap();
    answer(&rig, REBOOT, false, "", "not permitted");
    let error = rig.helper.reboot_dirty().unwrap_err();
    assert!(
        error.contains("reboot system failed: not permitted"),
        "{error}"
    );
    rig.helper.scan().unwrap();
    rig.host.state().fail.insert("command");
    let error = rig.helper.reboot_dirty().unwrap_err();
    assert!(error.contains("running /bin/launchctl"), "{error}");
}

/// Catches: the scan's verbs routed to the wrong method, or a reply larger than a
/// frame (every list is cut to SCAN_LINES lines of SCAN_LINE_BYTES, the rest
/// counted).
#[test]
fn the_scan_verbs_are_routed_and_their_replies_capped() {
    let rig = furnished("leak-handle");
    let reply = |request| match rig.helper.handle(request, Vec::new()) {
        Outcome::Reply(reply) => reply,
        Outcome::Running(child) => panic!("a process started: {child:?}"),
    };
    open_window(&rig);
    assert!(matches!(reply(Request::Baseline), Reply::Baselined { .. }));
    let daemons = rig.dir.join("LaunchDaemons");
    for n in 0..SCAN_LINES + 2 {
        std::fs::write(daemons.join(format!("{n:02}{}", "x".repeat(200))), "x").unwrap();
    }
    let Reply::Scanned {
        clean,
        added,
        removed,
        errors,
    } = reply(Request::Scan)
    else {
        panic!("not a scan reply");
    };
    assert!(!clean);
    assert_eq!(added.len(), SCAN_LINES + 1);
    assert_eq!(added[SCAN_LINES], "and 2 more");
    assert!(added.iter().all(|line| line.len() <= SCAN_LINE_BYTES));
    assert!(added[0].len() == SCAN_LINE_BYTES, "{}", added[0]);
    assert!(removed.is_empty() && errors.is_empty());
    assert_eq!(reply(Request::RebootDirty), Reply::Restarting);
    // A cut never splits a character.
    let line = format!("{}é", "a".repeat(SCAN_LINE_BYTES - 1));
    assert_eq!(capped(vec![line])[0].len(), SCAN_LINE_BYTES - 1);
}

/// The folders are what `ScanFolder::macos` names: the scan of a real Mac looks at
/// the launch agent and daemon folders and launchd's login items.
#[test]
fn the_macos_folders_are_the_minimal_set() {
    let folders = crate::scan::ScanFolder::macos();
    let paths: Vec<&Path> = folders.iter().map(|f| f.path.as_path()).collect();
    assert_eq!(
        paths,
        [
            Path::new("/Library/LaunchAgents"),
            Path::new("/Library/LaunchDaemons"),
            Path::new("/private/var/db/com.apple.xpc.launchd"),
        ]
    );
    assert_eq!(folders[2].prefix, Some("loginitems."));
}
