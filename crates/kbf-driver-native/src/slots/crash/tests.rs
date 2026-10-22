//! The crash report rule, on a real directory. Tests run unprivileged, so every file
//! they make is the test's own: "the slot's uid" is the test's uid to see what is
//! removed by ownership, and any other uid to see what the header alone removes.

use std::os::unix::fs::symlink;

use kbf_testkit::dir::TestDir;

use super::*;

/// The folder of the users' homes on a Mac.
const USERS: &str = "/Users";

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn other() -> u32 {
    me() + 1
}

const IPS: &str = "{\"app_name\":\"kbf\",\"bug_type\":\"309\"}\n{\"pid\":912,\"userID\":771,\"procName\":\"clang\"}\n";
const TEXT: &str = "Process:         clang [912]\nPath:            /usr/bin/clang\nUser ID:         771\nDate/Time:       now\n";

/// Catches a header rule that reads the wrong number: the whole `userID` or `User ID`
/// value must equal the uid (a prefix, a suffix and a different key do not name it), in
/// the JSON form and the text form, with the spaces either side of the colon.
#[test]
fn a_header_names_the_uid_by_its_user_id_value() {
    assert!(names_uid(IPS.as_bytes(), 771));
    assert!(names_uid(TEXT.as_bytes(), 771));
    assert!(names_uid(b"{\"userID\" : 771}", 771));
    assert!(names_uid(b"{\"userID\":771,\"x\":1}", 771));
    // In the header line of an `.ips` report, with a body that is not valid.
    assert!(names_uid(b"{\"userID\":771}\n{\"pid\":", 771));
    for other in [77, 7710, 1771, 0] {
        assert!(!names_uid(IPS.as_bytes(), other), "{other}");
        assert!(!names_uid(TEXT.as_bytes(), other), "{other}");
    }
    assert!(!names_uid(b"{\"uidx\":771,\"userID\":\"771\"}", 771));
    assert!(!names_uid(b"{\"userID\" 771}", 771));
    assert!(!names_uid(b"   User ID: 77x", 771));
    assert!(!names_uid(b"Parent User ID: 771", 771));
    assert!(!names_uid(b"\xff\xfe", 771));
    assert!(!names_uid(b"", 771));
}

/// Catches a report owned by someone else that merely quotes the uid being removed as
/// root: a number in the body is not the crashed process's user. The uid in a nested
/// object or array of an `.ips` body, in a string, in a third JSON value, after the first
/// section of a text report, or on an indented line is not named; the header's is.
#[test]
fn a_uid_quoted_in_the_body_names_nobody() {
    let body = |inner: &str| format!("{{\"app_name\":\"x\"}}\n{{\"pid\":1,{inner}}}\n");
    assert!(!names_uid(
        body("\"threads\":[{\"userID\":771}]").as_bytes(),
        771
    ));
    assert!(!names_uid(
        body("\"note\":\"\\\"userID\\\":771\"").as_bytes(),
        771
    ));
    assert!(!names_uid(body("\"a\":{\"userID\":771}").as_bytes(), 771));
    assert!(names_uid(body("\"userID\":771").as_bytes(), 771));
    let third = "{}\n{}\n{\"userID\":771}\n";
    assert!(!names_uid(third.as_bytes(), 771));
    for section in ["Thread 0 Crashed:", "Backtrace", "Binary Images:"] {
        let text = format!("Process: x [1]\nUser ID: 12\n\n{section}\nUser ID: 771\n");
        assert!(!names_uid(text.as_bytes(), 771), "{section}");
        assert!(names_uid(text.as_bytes(), 12), "{section}");
    }
    assert!(!names_uid(b"Process: x [1]\n  User ID: 771\n", 771));
    // No `User ID` line in the header: the one after the first section is the body's.
    let late = b"Process: x [1]\n\nThread 0 Crashed:\nUser ID: 771\n";
    assert!(!names_uid(late, 771));
}

/// Catches a list of crash folders that misses one: the system folder and its `Retired`,
/// `/cores`, root's folder, and for each user of `/Users` its `DiagnosticReports` and
/// that folder's `Retired` (the mutant that drops `/cores`, or the per-user folders, or
/// `Retired`, turns this red).
#[test]
fn the_crash_folders_are_the_systems_and_every_users() {
    let dirs = crash_dirs(&["alice".to_owned(), "bob".to_owned()]);
    let shown: Vec<&str> = dirs.iter().map(|d| d.to_str().unwrap()).collect();
    let user = |name: &str, retired: &str| {
        format!("{USERS}/{name}/Library/Logs/DiagnosticReports{retired}")
    };
    assert_eq!(
        shown,
        [
            "/Library/Logs/DiagnosticReports".to_owned(),
            "/Library/Logs/DiagnosticReports/Retired".to_owned(),
            "/cores".to_owned(),
            "/private/var/root/Library/Logs/DiagnosticReports".to_owned(),
            user("alice", "/Retired"),
            user("alice", ""),
            user("bob", "/Retired"),
            user("bob", ""),
        ]
    );
    assert_eq!(crash_dirs(&[]).len(), 4);
}

/// Catches a sweep that removes only the files the uid owns (a report the console user's
/// crash reporter wrote about a pool process is owned by the console user: the header
/// rule alone removes it), one that removes by header only (a core file the uid owns has
/// no readable header), one that removes another uid's report that does not name the
/// uid, one that removes a folder, one that follows a link a user planted in a crash
/// folder, and one that stops at a folder that is not there.
#[test]
fn the_sweep_removes_what_the_uid_owns_or_the_header_names_and_nothing_else() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-driver-native-crash");
    let reports = dir.join("reports");
    let cores = dir.join("cores");
    fs::create_dir_all(reports.join("Retired")).unwrap();
    fs::create_dir(&cores).unwrap();
    fs::write(reports.join("about-771.ips"), IPS).unwrap();
    fs::write(reports.join("about-771.crash"), TEXT).unwrap();
    fs::write(reports.join("about-772.ips"), IPS.replace("771", "772")).unwrap();
    fs::write(cores.join("core.912"), [0_u8, 159, 146, 150]).unwrap();
    let outside = dir.join("outside");
    fs::write(&outside, IPS).unwrap();
    symlink(&outside, reports.join("link.ips")).unwrap();
    fs::write(reports.join("Retired/old-771.ips"), IPS).unwrap();
    // `/Users/.localized` is a file: the folder under it is not there.
    let under_a_file = dir.join("file");
    fs::write(&under_a_file, "x").unwrap();
    let dirs = [
        under_a_file.join("Library/Logs/DiagnosticReports"),
        reports.clone(),
        reports.join("Retired"),
        cores.clone(),
        dir.join("absent"),
    ];
    // 771 is not the test's uid: only the headers name it. The core file is the test's.
    let mut seen = found(&dirs, 771);
    seen.sort();
    assert_eq!(
        seen,
        [
            reports.join("Retired/old-771.ips"),
            reports.join("about-771.crash"),
            reports.join("about-771.ips")
        ]
        .map(|p| p.display().to_string()),
        "the check sees what the sweep removes"
    );
    assert_eq!(sweep(&dirs, 771), Vec::<String>::new());
    assert_eq!(found(&dirs, 771), Vec::<String>::new());
    let left = |d: &Path| {
        let mut names: Vec<String> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(left(&reports), ["Retired", "about-772.ips", "link.ips"]);
    assert_eq!(left(&reports.join("Retired")), Vec::<String>::new());
    assert_eq!(left(&cores), ["core.912"]);
    assert!(outside.exists(), "the link was followed");
    // As the owner, everything but the folder goes, links included, the target never.
    assert_eq!(sweep(&dirs, me()), Vec::<String>::new());
    assert_eq!(left(&reports), ["Retired"]);
    assert_eq!(left(&cores), Vec::<String>::new());
    assert!(outside.exists(), "the link's target was removed");
    assert!(sweep(&dirs, other()).is_empty());
}

/// Catches an entry that cannot be removed or a folder that cannot be listed passing as
/// clean: each is an error naming its path, and the rest of the folder still goes.
#[test]
fn a_report_that_cannot_be_removed_or_a_folder_that_cannot_be_read_is_an_error() {
    use std::os::unix::fs::PermissionsExt as _;
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = TestDir::under(std::env::temp_dir(), "kbf-driver-native-crash-errors");
    let locked = dir.join("locked");
    let sealed = dir.join("sealed");
    fs::create_dir_all(&locked).unwrap();
    fs::create_dir_all(&sealed).unwrap();
    fs::write(locked.join("a.ips"), IPS).unwrap();
    fs::write(sealed.join("b.ips"), IPS).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
    let errors = sweep(&[locked.clone(), sealed.clone()], 771);
    let seen = found(std::slice::from_ref(&sealed), 771);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].contains("locked/a.ips"), "{errors:?}");
    assert!(errors[1].contains("sealed"), "{errors:?}");
    assert_eq!(
        seen.len(),
        1,
        "an unreadable folder is found, not clean: {seen:?}"
    );
    assert!(locked.join("a.ips").exists());
}

/// Catches a report that vanishes between the listing and the removal (the crash reporter
/// rotating its folder) read as a failure, and a report that cannot be opened read as
/// naming the uid: the first is no error, the second is left alone and is no error.
#[test]
fn a_vanished_report_is_no_error_and_an_unreadable_one_is_not_the_uids() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-driver-native-crash-vanish");
    fs::write(dir.join("a.ips"), IPS).unwrap();
    let gone = each(std::slice::from_ref(&dir.to_path_buf()), me(), |_| {
        Err(std::io::ErrorKind::NotFound.into())
    });
    assert_eq!(gone, Vec::<String>::new());
    if rustix::process::geteuid().is_root() {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(dir.join("a.ips"), fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(
        sweep(std::slice::from_ref(&dir.to_path_buf()), 771),
        Vec::<String>::new()
    );
    assert!(dir.join("a.ips").exists());
}

/// Catches a JSON report read by its head alone (a body longer than the first 16 KiB puts
/// `userID` past it) and a report that is read whole without a bound: a 100 KB body with
/// the key last is attributed, and a report that quotes the uid only deep in a nested
/// array past the head is not.
#[test]
fn a_long_ips_report_is_read_whole() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-driver-native-crash-long");
    let pad = "x".repeat(100 * 1024);
    let long = |inner: &str| format!("{{\"app_name\":\"k\"}}\n{{\"pad\":\"{pad}\",{inner}}}\n");
    fs::write(dir.join("hers.ips"), long("\"userID\":771")).unwrap();
    fs::write(dir.join("nested.ips"), long("\"t\":[{\"userID\":771}]")).unwrap();
    let dirs = [dir.to_path_buf()];
    assert_eq!(sweep(&dirs, 771), Vec::<String>::new());
    assert!(!dir.join("hers.ips").exists());
    assert!(dir.join("nested.ips").exists());
}
