//! The crash report rule, on a real directory tree under a temporary directory that
//! stands for `/`. Tests run unprivileged, so every file they make is the test's own:
//! "the slot's uid" is the test's uid to see what is removed by ownership, and any other
//! uid to see what the header alone removes.

use std::fs;
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

/// A pool that owns nothing under the test's directory.
const FAR: RangeInclusive<u32> = 4_000_000_000..=4_000_000_001;

fn scope(uid: u32) -> Scope {
    Scope { uid, pool: FAR }
}

const IPS: &str = "{\"app_name\":\"kbf\",\"bug_type\":\"309\"}\n{\"pid\":912,\"userID\":771,\"procName\":\"clang\"}\n";
const TEXT: &str = "Process:         clang [912]\nPath:            /usr/bin/clang\nUser ID:         771\nDate/Time:       now\n";

fn tree(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-driver-native-crash-{name}"),
    )
}

/// A descriptor on the directory that stands for `/`.
fn root(dir: &Path) -> OwnedFd {
    rustix::fs::openat(CWD, dir, DIRECTORY, Mode::empty()).unwrap()
}

/// Sweeps `dirs` (relative to `dir`) with the whole budget.
fn sweep_in(dir: &Path, dirs: &[&str], scope: &Scope) -> Vec<String> {
    let dirs: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
    each(&root(dir), &dirs, scope, READ_BUDGET, &mut Remove)
}

fn found_in(dir: &Path, dirs: &[&str], scope: &Scope) -> Vec<String> {
    let dirs: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
    let mut seen = Vec::new();
    let errors = each(&root(dir), &dirs, scope, READ_BUDGET, &mut Note(&mut seen));
    assert_eq!(errors, Vec::<String>::new());
    seen.sort();
    seen
}

fn names_of(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches a header rule that reads the wrong number: the whole `userID` value must equal
/// the uid (a prefix, a suffix, a leading zero and a different key do not name it), in
/// the header object and in the body object, and a text report names no one (its lines
/// can be forged by a newline in a process path).
#[test]
fn a_report_names_the_uid_by_its_user_id_value() {
    assert!(names_uid(IPS.as_bytes(), 771));
    assert!(names_uid(b"{\"userID\" : 771}", 771));
    assert!(names_uid(b"{\"userID\":771,\"x\":1}", 771));
    // In the header line of an `.ips` report, with a body that is not valid.
    assert!(names_uid(b"{\"userID\":771}\n{\"pid\":", 771));
    for other in [77, 7710, 1771, 0] {
        assert!(!names_uid(IPS.as_bytes(), other), "{other}");
    }
    assert!(!names_uid(b"{\"userID\":0771}", 771), "a leading zero");
    assert!(!names_uid(b"{\"uidx\":771,\"userID\":\"771\"}", 771));
    assert!(!names_uid(b"{\"userID\" 771}", 771));
    assert!(
        !names_uid(TEXT.as_bytes(), 771),
        "a text report names no one"
    );
    assert!(!names_uid(b"Process: x\nPath: /a\nUser ID: 771\n", 771));
    assert!(!names_uid(b"\xff\xfe", 771));
    assert!(!names_uid(b"", 771));
}

/// Catches a report owned by someone else that merely quotes the uid being removed as
/// root: a number in the body is not the crashed process's user. The uid in a nested
/// object or array of an `.ips` body, in a string, or in a third JSON value is not named;
/// the top-level key is.
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
}

/// Catches a list of crash folders that misses one: the system folder and its `Retired`,
/// `/cores`, root's folder, and for each user of `/Users` its `DiagnosticReports` and
/// that folder's `Retired` (`/cores`, the per-user folders and `Retired` are each
/// asserted).
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
/// uid, a text report removed because it names the uid, one that removes a folder, one
/// that follows a link, and one that stops at a folder that is not there.
#[test]
fn the_sweep_removes_what_the_uid_owns_or_the_ips_header_names_and_nothing_else() {
    let dir = tree("sweep");
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
    let dirs = ["reports", "reports/Retired", "cores", "absent"];
    // 771 is not the test's uid: only the `.ips` headers name it. The core file is the test's.
    assert_eq!(
        found_in(&dir, &dirs, &scope(771)),
        ["reports/Retired/old-771.ips", "reports/about-771.ips"]
    );
    assert_eq!(sweep_in(&dir, &dirs, &scope(771)), Vec::<String>::new());
    assert_eq!(found_in(&dir, &dirs, &scope(771)), Vec::<String>::new());
    assert_eq!(
        names_of(&reports),
        ["Retired", "about-771.crash", "about-772.ips", "link.ips"]
    );
    assert_eq!(names_of(&reports.join("Retired")), Vec::<String>::new());
    assert_eq!(names_of(&cores), ["core.912"]);
    assert!(outside.exists(), "the link was followed");
    // As the owner, everything but the folder goes, links included, the target never.
    assert_eq!(sweep_in(&dir, &dirs, &scope(me())), Vec::<String>::new());
    assert_eq!(names_of(&reports), ["Retired"]);
    assert_eq!(names_of(&cores), Vec::<String>::new());
    assert!(outside.exists(), "the link's target was removed");
    assert!(sweep_in(&dir, &dirs, &scope(other())).is_empty());
}

/// Catches a root that follows a link on the way to a crash folder (a component opened
/// without `O_NOFOLLOW`): a component that is a link, a final
/// folder that is a link, and a link planted as the folder's name all leave the file they
/// point to where it is.
#[test]
fn a_link_on_the_way_to_a_folder_is_not_followed() {
    let dir = tree("links");
    let target = dir.join("target");
    fs::create_dir_all(target.join("Logs")).unwrap();
    fs::write(target.join("Logs/victim.ips"), IPS).unwrap();
    fs::write(target.join("direct.ips"), IPS).unwrap();
    symlink(&target, dir.join("component")).unwrap();
    fs::create_dir(dir.join("real")).unwrap();
    symlink(&target, dir.join("real/final")).unwrap();
    let dirs = ["component/Logs", "real/final"];
    assert_eq!(found_in(&dir, &dirs, &scope(771)), Vec::<String>::new());
    assert_eq!(sweep_in(&dir, &dirs, &scope(771)), Vec::<String>::new());
    assert_eq!(sweep_in(&dir, &dirs, &scope(me())), Vec::<String>::new());
    assert!(target.join("Logs/victim.ips").exists());
    assert!(target.join("direct.ips").exists());
}

/// Catches a root that walks a folder a pool user controls (the pool user's home on a
/// volume that is, or is under, `/Users`): a directory on the path owned by a pool uid
/// makes the folder skipped, the same folder is walked when no pool user owns it.
#[test]
fn a_folder_with_a_pool_users_directory_on_its_path_is_skipped() {
    let dir = tree("pool");
    fs::create_dir_all(dir.join("home/Library/Logs")).unwrap();
    fs::write(dir.join("home/Library/Logs/hers.ips"), IPS).unwrap();
    let mine = Scope {
        uid: 771,
        pool: me()..=me(),
    };
    let dirs = ["home/Library/Logs"];
    assert_eq!(found_in(&dir, &dirs, &mine), Vec::<String>::new());
    assert_eq!(sweep_in(&dir, &dirs, &mine), Vec::<String>::new());
    assert!(dir.join("home/Library/Logs/hers.ips").exists());
    assert_eq!(sweep_in(&dir, &dirs, &scope(771)), Vec::<String>::new());
    assert!(!dir.join("home/Library/Logs/hers.ips").exists());
}

/// Catches a text report removed because its `User ID:` line names the uid (forged by a
/// newline in a path), and a text report the uid owns kept: a text report is the uid's by
/// owner alone.
#[test]
fn a_text_report_is_the_uids_by_owner_alone() {
    let dir = tree("text");
    fs::create_dir(dir.join("reports")).unwrap();
    let forged = "Process: x [1]\nPath: /a\nUser ID: 771\nUser ID: 771\n";
    fs::write(dir.join("reports/forged.crash"), forged).unwrap();
    assert_eq!(
        sweep_in(&dir, &["reports"], &scope(771)),
        Vec::<String>::new()
    );
    assert!(dir.join("reports/forged.crash").exists());
    assert_eq!(
        sweep_in(&dir, &["reports"], &scope(me())),
        Vec::<String>::new()
    );
    assert!(!dir.join("reports/forged.crash").exists());
}

/// Catches an entry opened or removed that is not a regular file (the regular-file
/// test): a FIFO that holds a report naming the
/// uid (a writer keeps it readable) and a link to a report are neither read nor removed.
#[test]
fn a_fifo_and_a_link_in_a_folder_are_neither_opened_nor_removed() {
    use std::io::Write as _;
    let dir = tree("fifo");
    fs::create_dir(dir.join("reports")).unwrap();
    let fifo = dir.join("reports/pipe.ips");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success(), "mkfifo");
    // Read and write on one descriptor: opening a FIFO that way does not block, and what
    // is written stays readable.
    let mut writer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .unwrap();
    writer.write_all(IPS.as_bytes()).unwrap();
    fs::write(dir.join("reports/real.ips"), IPS).unwrap();
    symlink(dir.join("reports/real.ips"), dir.join("reports/link.ips")).unwrap();
    assert_eq!(
        sweep_in(&dir, &["reports"], &scope(771)),
        Vec::<String>::new()
    );
    assert_eq!(names_of(&dir.join("reports")), ["link.ips", "pipe.ips"]);
    drop(writer);
}

/// Catches an unreadable folder of another's failing every slot's clean: a folder that
/// cannot be opened is skipped, not an error; a failure inside an opened folder of the
/// daemon's own is one (the clean answers for it), and one inside a folder another user
/// owns is only logged.
#[test]
fn a_folder_that_cannot_be_opened_is_skipped_and_one_that_cannot_be_cleaned_is_an_error() {
    use std::os::unix::fs::PermissionsExt as _;
    assert!(answers_for(0, 5, 9));
    assert!(answers_for(5, 5, 9));
    assert!(answers_for(9, 5, 7 + 2));
    assert!(!answers_for(6, 5, 9));
    let mut errors = Vec::new();
    note_failure(&mut errors, false, Path::new("/x"), "other's".to_owned());
    assert_eq!(errors, Vec::<String>::new());
    note_failure(&mut errors, true, Path::new("/x"), "ours".to_owned());
    assert_eq!(errors, ["ours"]);
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = tree("errors");
    let locked = dir.join("locked");
    let sealed = dir.join("sealed");
    fs::create_dir_all(&locked).unwrap();
    fs::create_dir_all(&sealed).unwrap();
    fs::write(locked.join("a.ips"), IPS).unwrap();
    fs::write(sealed.join("b.ips"), IPS).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
    let errors = sweep_in(&dir, &["locked", "sealed"], &scope(771));
    let seen = found_in(&dir, &["sealed"], &scope(771));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("locked/a.ips"), "{errors:?}");
    assert_eq!(
        seen,
        Vec::<String>::new(),
        "an unopenable folder holds nothing to see"
    );
    assert!(locked.join("a.ips").exists());
}

/// Catches a read with no cap (a folder of large files that start with `{` stalls the
/// clean): with a budget for two of the three small reports, two are read and removed and
/// the third is left unread, and a report larger than the budget is not read at all.
#[test]
fn what_is_read_of_reports_is_capped_by_a_budget() {
    let dir = tree("budget");
    fs::create_dir(dir.join("reports")).unwrap();
    let small = format!("{IPS}{}", " ".repeat(10 * 1024));
    for name in ["a.ips", "b.ips", "c.ips"] {
        fs::write(dir.join("reports").join(name), &small).unwrap();
    }
    let dirs = vec![PathBuf::from("reports")];
    let budget = HEAD + small.len() + 4 * 1024;
    let errors = each(&root(&dir), &dirs, &scope(771), budget, &mut Remove);
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(names_of(&dir.join("reports")).len(), 1, "two of three read");
    // A decoy of 5 MiB that starts with `{` is read for at most the budget and names no one.
    let decoy = format!("{{\"pad\":\"{}", "x".repeat(5 * 1024 * 1024));
    fs::write(dir.join("reports/decoy.ips"), decoy).unwrap();
    let errors = each(&root(&dir), &dirs, &scope(771), 64 * 1024, &mut Remove);
    assert_eq!(errors, Vec::<String>::new());
    assert!(dir.join("reports/decoy.ips").exists());
}

/// Catches a long `.ips` report read by its head alone (a body longer than the first 16 KiB
/// puts `userID` past it) and a report read whole without a bound: a 100 KB body with the
/// key last is attributed, and a report that quotes the uid only in a nested array is not.
#[test]
fn a_long_ips_report_is_read_whole() {
    let dir = tree("long");
    fs::create_dir(dir.join("r")).unwrap();
    let pad = "x".repeat(100 * 1024);
    let long = |inner: &str| format!("{{\"app_name\":\"k\"}}\n{{\"pad\":\"{pad}\",{inner}}}\n");
    fs::write(dir.join("r/hers.ips"), long("\"userID\":771")).unwrap();
    fs::write(dir.join("r/nested.ips"), long("\"t\":[{\"userID\":771}]")).unwrap();
    assert_eq!(sweep_in(&dir, &["r"], &scope(771)), Vec::<String>::new());
    assert_eq!(names_of(&dir.join("r")), ["nested.ips"]);
}

/// Catches a report that vanishes between the listing and the removal (the crash reporter
/// rotating its folder) read as a failure, and a report that cannot be opened read as
/// naming the uid: the first is no error, the second is left alone and is no error.
#[test]
fn a_vanished_report_is_no_error_and_an_unreadable_one_is_not_the_uids() {
    struct Gone;
    impl Act for Gone {
        fn on(&mut self, _: &OwnedFd, _: &CString, _: &Path) -> std::io::Result<()> {
            Err(std::io::ErrorKind::NotFound.into())
        }
    }
    let dir = tree("vanish");
    fs::create_dir(dir.join("r")).unwrap();
    fs::write(dir.join("r/a.ips"), IPS).unwrap();
    let dirs = vec![PathBuf::from("r")];
    let gone = each(&root(&dir), &dirs, &scope(me()), READ_BUDGET, &mut Gone);
    assert_eq!(gone, Vec::<String>::new());
    if rustix::process::geteuid().is_root() {
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(dir.join("r/a.ips"), fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(sweep_in(&dir, &["r"], &scope(771)), Vec::<String>::new());
    assert!(dir.join("r/a.ips").exists());
}

/// Catches a walk that fails on a path that is not there or whose root cannot be opened:
/// an absolute path that does not exist holds nothing.
#[test]
fn an_absolute_folder_that_is_not_there_holds_nothing() {
    let scope = scope(771);
    let dirs = [PathBuf::from("/nonexistent-kbf-crash/Library")];
    assert_eq!(sweep(&dirs, &scope), Vec::<String>::new());
    assert_eq!(found(&dirs, &scope), Vec::<String>::new());
    assert!(
        open_folder(
            &root(Path::new("/")),
            Path::new("../x"),
            &scope,
            &real_owner
        )
        .is_none()
    );
}

/// Catches a folder that cannot be listed passing as clean: a descriptor that is not a
/// directory (what a folder swapped for a file would give) is an error of the clean's own
/// folder, and nothing of another's.
#[test]
fn a_folder_that_cannot_be_listed_is_an_error_of_the_cleans_own() {
    let dir = tree("unlistable");
    fs::write(dir.join("file"), "x").unwrap();
    let file = rustix::fs::openat(CWD, dir.join("file"), FILE, Mode::empty()).unwrap();
    let mut errors = Vec::new();
    walk(
        &file,
        Path::new("file"),
        &scope(me()),
        (&mut Budget(READ_BUDGET), &mut Remove),
        &mut errors,
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].starts_with("file: "), "{errors:?}");
}

/// Catches a pool-ownership check made on the last component of a path only (a pool
/// user's directory higher up would then not stop the walk): a directory that is a pool
/// user's anywhere on the way, the first component or a middle one, makes the folder
/// skipped; with none, it is walked.
#[test]
fn a_pool_users_directory_on_any_component_of_the_path_skips_the_folder() {
    let dir = tree("pool-any");
    fs::create_dir_all(dir.join("first/middle/last")).unwrap();
    fs::write(dir.join("first/middle/last/hers.ips"), IPS).unwrap();
    let dirs = vec![PathBuf::from("first/middle/last")];
    let pool_uid = 7_000_000_u32;
    let scope = Scope {
        uid: 771,
        pool: pool_uid..=pool_uid + 1,
    };
    for pooled in ["first", "middle"] {
        let owner =
            move |_: &OwnedFd, name: &OsStr| Some(if name == pooled { pool_uid } else { me() });
        let errors = each_owned_by(&root(&dir), &dirs, &scope, READ_BUDGET, &mut Remove, &owner);
        assert_eq!(errors, Vec::<String>::new());
        assert!(dir.join("first/middle/last/hers.ips").exists(), "{pooled}");
    }
    let nobody = |_: &OwnedFd, _: &OsStr| Some(me());
    each_owned_by(
        &root(&dir),
        &dirs,
        &scope,
        READ_BUDGET,
        &mut Remove,
        &nobody,
    );
    assert!(!dir.join("first/middle/last/hers.ips").exists());
}
