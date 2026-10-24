//! The `/var/folders` entry's clean as the pool user: who each call runs as and in what
//! order (a fake [`Run`]), and the entry mode on a real directory (the test's own uid
//! stands for the pool user).

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use kbf_testkit::dir::TestDir;

use super::*;

const PROGRAM: &str = "/opt/kbf/kbf-daemon";
const HOME: &str = "/pool/home-3";
const FOLDERS: &str = "/private/var/folders/xx/yyyy/0";

fn me() -> AsUid<'static> {
    AsUid {
        program: Path::new(PROGRAM),
        uid: 61003,
        gid: 20,
    }
}

/// Everything a step did, in order: `root <path>` for a part done as root, and
/// `<program> <args>` for each call of the real runner.
type Log = RefCell<Vec<String>>;

fn words(args: &[OsString]) -> String {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    args.join(" ")
}

/// A runner that logs each call and fails for a call that has `fail` among its arguments.
fn runner<'a>(
    log: &'a Log,
    fail: &'a [&'a str],
) -> impl FnMut(&Path, &[OsString]) -> Result<String, String> + 'a {
    move |program, args| {
        let line = format!("{} {}", program.display(), words(args));
        log.borrow_mut().push(line.clone());
        if let Some(word) = fail.iter().find(|w| line.contains(**w)) {
            return Err(format!("{word} failed"));
        }
        Ok("lines\nof\noutput".to_owned())
    }
}

fn as_root<'a>(log: &'a Log) -> impl FnMut(&Path) -> Vec<String> + 'a {
    move |path| {
        log.borrow_mut().push(format!("root {}", path.display()));
        Vec::new()
    }
}

fn launched(tool: &str, args: &str) -> String {
    format!("{PROGRAM} __as-user 61003 20 -- {tool} {args}")
}

/// The call of the entry mode `mode`: it drops to the uid itself.
fn mode_call(mode: &str) -> String {
    format!("{PROGRAM} __entry {mode} 61003 20 {FOLDERS}")
}

#[test]
fn the_flags_step_clears_the_home_as_root_and_the_entry_as_the_uid_in_that_order() {
    let log = Log::default();
    flags_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut as_root(&log),
        &mut runner(&log, &[]),
    )
    .expect("clean");
    let acl = acl_args(Path::new(FOLDERS), 61003);
    assert_eq!(
        *log.borrow(),
        vec![
            format!("root {HOME}"),
            launched(FIND, &words(&acl)),
            mode_call("clear"),
            launched(FIND, &words(&acl)),
        ]
    );
}

#[test]
fn root_is_never_given_the_entry_to_clear_or_to_empty() {
    let log = Log::default();
    let mut roots = Vec::new();
    let mut record = |path: &Path| {
        roots.push(path.to_owned());
        Vec::new()
    };
    let run = &mut runner(&log, &[]);
    flags_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut record,
        run,
    )
    .expect("flags");
    contents_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut record,
        run,
    )
    .expect("contents");
    assert_eq!(roots, vec![PathBuf::from(HOME), PathBuf::from(HOME)]);
}

#[test]
fn every_call_on_the_entry_is_made_as_the_uid() {
    let log = Log::default();
    let run = &mut runner(&log, &[]);
    let dir = (Path::new(HOME), Path::new(FOLDERS));
    flags_step(&me(), dir, &mut as_root(&Log::default()), run).expect("flags");
    contents_step(&me(), dir, &mut as_root(&Log::default()), run).expect("contents");
    left(&me(), Path::new(FOLDERS), run).expect("left");
    let calls = log.borrow();
    assert_eq!(calls.len(), 5);
    // The daemon's modes get the ids and drop inside their own process (the pool user
    // need not be able to execute the binary); the system tool is run by `as-user`.
    for call in calls.iter() {
        let own = call.starts_with(&format!("{PROGRAM} __entry "))
            && call.contains(&format!(" 61003 20 {FOLDERS}"));
        let tool = call.starts_with(&format!("{PROGRAM} __as-user 61003 20 -- {FIND} "));
        assert!(own || tool, "{call}");
    }
}

#[test]
fn the_acls_are_removed_before_and_after_the_sweep_and_only_the_last_removal_is_judged() {
    let log = Log::default();
    let acl = words(&acl_args(Path::new(FOLDERS), 61003));
    let first = format!("{PROGRAM} __as-user 61003 20 -- {FIND} ");
    // Every call fails but the sweep, which is the second: the first removal is let fail.
    let calls = RefCell::new(0);
    let mut run = |program: &Path, args: &[OsString]| {
        let line = format!("{} {}", program.display(), words(args));
        log.borrow_mut().push(line.clone());
        *calls.borrow_mut() += 1;
        if line.starts_with(&first) && *calls.borrow() == 1 {
            return Err("early removal failed".to_owned());
        }
        Ok(String::new())
    };
    clear_flags(&me(), Path::new(FOLDERS), &mut run).expect("only the last removal is judged");
    assert_eq!(
        *log.borrow(),
        vec![
            launched(FIND, &acl),
            mode_call("clear"),
            launched(FIND, &acl),
        ]
    );
}

#[test]
fn the_contents_step_empties_the_home_as_root_then_the_entry_as_the_uid() {
    let log = Log::default();
    contents_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut as_root(&log),
        &mut runner(&log, &[]),
    )
    .expect("clean");
    assert_eq!(
        *log.borrow(),
        vec![format!("root {HOME}"), mode_call("empty"),]
    );
}

#[test]
fn a_uid_side_failure_of_the_flags_step_fails_it_naming_the_entry() {
    for fail in ["__entry clear", "-exec"] {
        let log = Log::default();
        let why = flags_step(
            &me(),
            (Path::new(HOME), Path::new(FOLDERS)),
            &mut as_root(&log),
            &mut runner(&log, &[fail]),
        )
        .expect_err("fails");
        assert!(why.starts_with(&format!("{FOLDERS}: ")), "{why}");
        assert!(why.contains(&format!("{fail} failed")), "{why}");
        // The other call is made anyway: both failures are shown.
        assert_eq!(log.borrow().len(), 4, "{fail}");
    }
}

#[test]
fn both_uid_side_failures_of_the_flags_step_are_reported() {
    let log = Log::default();
    let why = flags_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut as_root(&log),
        &mut runner(&log, &["__entry clear", "-exec"]),
    )
    .expect_err("fails");
    assert!(why.contains("__entry clear failed; -exec failed"), "{why}");
}

#[test]
fn a_failure_of_the_home_as_root_fails_the_step_and_the_entry_is_still_cleaned() {
    let log = Log::default();
    let why = flags_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut |_| vec!["home: locked".to_owned()],
        &mut runner(&log, &[]),
    )
    .expect_err("fails");
    assert_eq!(why, "home: locked");
    assert_eq!(log.borrow().len(), 3);
}

#[test]
fn a_uid_side_failure_of_the_contents_step_fails_it_naming_the_entry() {
    let log = Log::default();
    let why = contents_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut as_root(&log),
        &mut runner(&log, &["__entry empty"]),
    )
    .expect_err("fails");
    assert_eq!(why, format!("{FOLDERS}: __entry empty failed"));
}

#[test]
fn a_failure_of_the_home_and_of_the_entry_are_both_in_the_contents_failure() {
    let log = Log::default();
    let why = contents_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut |_| vec!["home: busy".to_owned()],
        &mut runner(&log, &["__entry empty"]),
    )
    .expect_err("fails");
    assert!(why.starts_with("home: busy; "), "{why}");
    assert!(why.contains(FOLDERS), "{why}");
}

#[test]
fn the_look_reports_each_line_the_uid_printed() {
    let log = Log::default();
    let lines = left(&me(), Path::new(FOLDERS), &mut runner(&log, &[])).expect("looked");
    assert_eq!(lines, vec!["lines", "of", "output"]);
    assert_eq!(*log.borrow(), vec![mode_call("look")]);
}

#[test]
fn a_look_that_could_not_be_made_is_an_error_naming_the_entry() {
    let log = Log::default();
    let why = left(
        &me(),
        Path::new(FOLDERS),
        &mut runner(&log, &["__entry look"]),
    )
    .expect_err("fails");
    assert_eq!(why, format!("{FOLDERS}: __entry look failed"));
}

#[test]
fn acls_are_removed_only_from_the_uids_entries_and_never_through_a_link() {
    let args = words(&acl_args(Path::new(FOLDERS), 61003));
    assert_eq!(
        args,
        format!(
            "-x {FOLDERS} -user 61003 ( ( -type d -exec /bin/chmod -N {{}} ; ) -o ( ! -type l -exec /bin/chmod -N {{}} + ) )"
        )
    );
}

#[test]
fn the_launch_is_the_as_user_mode_then_the_tool() {
    let args = me().launch(OsStr::new("/usr/bin/true"), &["a".into(), "b".into()]);
    assert_eq!(words(&args), "__as-user 61003 20 -- /usr/bin/true a b");
}

// The entry mode, on a real directory.

fn tree(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-driver-native-entry-{name}"),
    )
}

fn me_now() -> u32 {
    rustix::process::geteuid().as_raw().max(1)
}

/// The entry mode with `words` after argv0, dropping through `drop_to`; its exit code
/// (`None` when the arguments do not select it), stdout and stderr.
fn run_mode(
    words: &[&str],
    drop_to: &mut dyn FnMut(u32, u32) -> Result<(), String>,
) -> (Option<i32>, String, String) {
    let mut args: Vec<OsString> = vec!["kbf-daemon".into()];
    args.extend(words.iter().map(OsString::from));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dispatch(&args, drop_to, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).expect("utf8"),
        String::from_utf8(err).expect("utf8"),
    )
}

/// `mode` on `dir` as the test's own uid and gid, which the fake drop accepts.
fn call(mode: &str, dir: &Path) -> (Option<i32>, String, String) {
    let (uid, gid) = (me_now().to_string(), my_gid().to_string());
    run_mode(
        &[ARG, mode, &uid, &gid, &dir.to_string_lossy()],
        &mut |_, _| Ok(()),
    )
}

fn my_gid() -> u32 {
    rustix::process::getegid().as_raw().max(1)
}

fn entry_with_files(dir: &TestDir) -> PathBuf {
    let entry = dir.path().join("entry");
    for folder in ["0", "C", "T", "X"] {
        fs::create_dir_all(entry.join(folder).join("deep")).expect("folder");
        fs::write(entry.join(folder).join("deep").join("file"), "x").expect("file");
        fs::write(entry.join(folder).join("top"), "x").expect("file");
    }
    fs::write(entry.join("stray"), "x").expect("stray");
    entry
}

#[test]
fn other_arguments_are_not_the_entry_mode() {
    let no_drop = &mut |_, _| panic!("dropped");
    assert_eq!(run_mode(&[], no_drop).0, None);
    assert_eq!(run_mode(&["--help"], no_drop).0, None);
    assert_eq!(run_mode(&["__as-user"], no_drop).0, None);
}

#[test]
fn the_entry_mode_refuses_bad_arguments_before_it_drops_anything() {
    let no_drop = &mut |_, _| panic!("dropped");
    for bad in [
        &[ARG][..],
        &[ARG, EMPTY, "5", "5"],
        &[ARG, EMPTY, "5", "5", "/x", "/y"],
    ] {
        let (code, _, err) = run_mode(bad, no_drop);
        assert_eq!(
            (code, err.starts_with("usage: ")),
            (Some(2), true),
            "{bad:?}"
        );
    }
    for ids in [
        ("0", "20"),
        ("501", "0"),
        ("x", "20"),
        ("501", "-1"),
        ("", ""),
    ] {
        let (code, _, err) = run_mode(&[ARG, EMPTY, ids.0, ids.1, "/x"], no_drop);
        assert_eq!(
            (code, err.contains("non-root ids")),
            (Some(2), true),
            "{ids:?}"
        );
    }
    // An id that is not text is refused like one that is not a number.
    use std::os::unix::ffi::OsStringExt as _;
    let args: Vec<OsString> = vec![
        "kbf-daemon".into(),
        ARG.into(),
        EMPTY.into(),
        OsString::from_vec(vec![0xff, 0xfe]),
        "20".into(),
        "/x".into(),
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(dispatch(&args, no_drop, &mut out, &mut err), Some(2));
    assert!(String::from_utf8_lossy(&err).contains("non-root ids"));
    let (code, _, err) = run_mode(&[ARG, "bogus", "501", "20", "/x"], no_drop);
    assert_eq!((code, err.contains("no mode")), (Some(2), true));
}

#[test]
fn the_entry_mode_drops_to_the_given_ids_and_stops_when_the_drop_fails() {
    let dir = tree("drop");
    let entry = entry_with_files(&dir);
    let mut dropped = Vec::new();
    let (uid, gid) = (me_now().to_string(), my_gid().to_string());
    let words = [ARG, LOOK, &uid, &gid, &*entry.to_string_lossy()];
    let (code, _, _) = run_mode(&words, &mut |u, g| {
        dropped.push((u, g));
        Ok(())
    });
    assert_eq!((code, dropped), (Some(0), vec![(me_now(), my_gid())]));
    // A failed drop is 126, and nothing was done to the entry.
    let words = [ARG, EMPTY, &uid, &gid, &*entry.to_string_lossy()];
    let (code, _, err) = run_mode(&words, &mut |_, _| Err("setuid: denied".to_owned()));
    assert_eq!(
        (code, err.as_str()),
        (Some(126), "__entry: setuid: denied\n")
    );
    assert!(entry.join("stray").exists());
}

#[test]
fn the_empty_mode_keeps_the_four_folders_empty_and_removes_the_rest() {
    let dir = tree("empty");
    let entry = entry_with_files(&dir);
    let (code, _, err) = call(EMPTY, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    let mut left: Vec<String> = fs::read_dir(&entry)
        .expect("entry")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["0", "C", "T", "X"]);
    for folder in ["0", "C", "T", "X"] {
        assert_eq!(fs::read_dir(entry.join(folder)).expect("folder").count(), 0);
    }
}

#[test]
fn the_empty_mode_fails_with_the_path_when_something_cannot_be_removed() {
    let dir = tree("empty-fails");
    // The removal of a file in a read-only directory fails, but not for root.
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root removes from a read-only directory");
        return;
    }
    let entry = entry_with_files(&dir);
    let locked = entry.join("T").join("deep");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod");
    let (code, _, err) = call(EMPTY, &entry);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains(&*locked.to_string_lossy()), "{err}");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod");
}

#[test]
fn the_clear_mode_makes_the_uids_entries_usable() {
    let dir = tree("clear");
    let entry = entry_with_files(&dir);
    let below = entry.join("C").join("deep");
    fs::set_permissions(&below, fs::Permissions::from_mode(0o000)).expect("chmod");
    let (code, _, err) = call(CLEAR, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    let mode = fs::metadata(&below).expect("meta").permissions().mode();
    assert_eq!(mode & 0o700, 0o700);
}

#[test]
fn the_look_mode_prints_what_is_left_and_nothing_when_clean() {
    let dir = tree("look");
    let entry = entry_with_files(&dir);
    let (code, out, err) = call(LOOK, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    let shown = |name: &str| out.lines().filter(|l| l.contains(name)).count();
    assert_eq!(shown("stray"), 1, "{out}");
    for folder in ["0", "C", "T", "X"] {
        assert_eq!(shown(&format!("{folder}: [")), 1, "{folder}: {out}");
    }
    call(EMPTY, &entry);
    let (code, out, _) = call(LOOK, &entry);
    assert_eq!((code, out.as_str()), (Some(0), ""));
}

#[test]
fn the_look_mode_fails_when_it_cannot_list_the_entry_or_one_of_its_folders() {
    let dir = tree("look-fails");
    let missing = dir.path().join("missing");
    let (code, out, err) = call(LOOK, &missing);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains(&*missing.to_string_lossy()), "{err}");
    // A folder that is a file cannot be listed: seen as a failure, never as empty.
    let entry = dir.path().join("entry");
    fs::create_dir_all(&entry).expect("entry");
    fs::write(entry.join("T"), "x").expect("file");
    let (code, _, err) = call(LOOK, &entry);
    assert_eq!(code, Some(1));
    assert!(err.contains("entry/T"), "{err}");
    // A link in the place of a folder is reported, never followed into its target.
    fs::remove_file(entry.join("T")).expect("rm");
    let target = dir.path().join("elsewhere");
    fs::create_dir_all(&target).expect("target");
    std::os::unix::fs::symlink(&target, entry.join("T")).expect("link");
    let (code, out, err) = call(LOOK, &entry);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains("entry/T: not a directory"), "{err}");
}
