//! The real host, under a scratch directory standing in for `/`.

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

mod testdir;

use host_step::{Cmd, Host, Kind, RealHost};
use testdir::TestDir;

fn scratch(name: &str) -> (TestDir, RealHost) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let host = RealHost::under(dir.path());
    (dir, host)
}

fn me(dir: &TestDir) -> (u32, u32) {
    let m = std::fs::metadata(dir.path()).unwrap();
    (m.uid(), m.gid())
}

/// Catches a write that leaves the mode to the umask or leaves a temporary file behind: the
/// mode is exact, the old content is replaced whole, and only the target remains.
#[test]
fn write_sets_the_exact_mode_and_replaces_whole() {
    let (dir, host) = scratch("write");
    let owner = me(&dir);
    host.mkdir("/etc/x", 0o750, owner).unwrap();
    host.write("/etc/x/f", 0o640, owner, b"first").unwrap();
    host.write("/etc/x/f", 0o600, owner, b"second").unwrap();
    // A temporary file an interrupted run left is cleared.
    std::fs::write(dir.path().join("etc/x/f.js-host.tmp"), b"junk").unwrap();
    host.write("/etc/x/f", 0o644, owner, b"third").unwrap();
    let meta = host.stat("/etc/x/f").unwrap().unwrap();
    assert_eq!((meta.kind, meta.mode, meta.size), (Kind::File, 0o644, 5));
    assert_eq!(host.read("/etc/x/f").unwrap().unwrap(), b"third");
    let names: Vec<_> = std::fs::read_dir(dir.path().join("etc/x"))
        .unwrap()
        .collect();
    assert_eq!(names.len(), 1);
    let dir_meta = host.stat("/etc/x").unwrap().unwrap();
    assert_eq!((dir_meta.kind, dir_meta.mode), (Kind::Dir, 0o750));
}

/// Catches a mkdir that leaves an existing directory's mode alone (a rerun must fix it) and
/// a stat that follows a link or reports a missing path as an error.
#[test]
fn mkdir_fixes_an_existing_mode_and_stat_sees_links_and_gaps() {
    let (dir, host) = scratch("mkdir");
    let owner = me(&dir);
    host.mkdir("/d", 0o777, owner).unwrap();
    host.mkdir("/d", 0o700, owner).unwrap();
    assert_eq!(
        std::fs::metadata(dir.path().join("d"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    symlink("d", dir.path().join("l")).unwrap();
    assert_eq!(host.stat("/l").unwrap().unwrap().kind, Kind::Other);
    assert_eq!(host.stat("/nothing").unwrap(), None);
    assert_eq!(host.read("/nothing").unwrap(), None);
    host.remove("/nothing").unwrap();
    host.write("/d/f", 0o600, owner, b"x").unwrap();
    host.remove("/d/f").unwrap();
    assert_eq!(host.stat("/d/f").unwrap(), None);
}

/// Catches errors that are swallowed: a write into a missing directory fails, a read of a
/// directory fails, and a path the system rejects fails rather than reading as missing.
#[test]
fn errors_other_than_not_found_are_returned() {
    let (dir, host) = scratch("errors");
    let owner = me(&dir);
    assert!(host.write("/missing/f", 0o600, owner, b"x").is_err());
    host.mkdir("/d", 0o700, owner).unwrap();
    assert!(host.read("/d").is_err());
    assert!(host.stat("/d/../..\0x").is_err());
}

/// Catches a runner that goes through a shell, passes the environment on, or reports a
/// failing exit as success; and a missing program that is not an error.
#[test]
fn run_executes_without_a_shell_or_environment() {
    let (_dir, host) = scratch("run");
    let said = host
        .run(&Cmd::new("/bin/echo").arg("a  b").arg("$HOME"))
        .unwrap();
    assert_eq!((said.code, said.stdout.as_str()), (0, "a  b $HOME\n"));
    let env = host.run(&Cmd::new("/usr/bin/env")).unwrap();
    assert_eq!(env.stdout, "");
    let failed = host.run(&Cmd::new("/bin/false")).unwrap();
    assert_eq!(failed.code, 1);
    assert!(host.run(&Cmd::new("/nonexistent/program")).is_err());
    // A signal ends the program: the code is -1. The program is this test binary running
    // only `ends_by_a_signal`, which aborts (SIGABRT) when it finds `SIGNAL_CHILD` among
    // its arguments; `--skip` takes the marker as a filter that matches no test.
    let me = std::env::current_exe().unwrap();
    let killed = host
        .run(
            &Cmd::new(me.to_str().unwrap())
                .arg("--exact")
                .arg("ends_by_a_signal")
                .arg("--ignored")
                .arg("--skip")
                .arg(SIGNAL_CHILD),
        )
        .unwrap();
    assert_eq!(killed.code, -1, "{killed:?}");
    assert_eq!(host.euid(), std::fs::metadata("/proc/self").unwrap().uid());
    assert_eq!(host.egid(), std::fs::metadata("/proc/self").unwrap().gid());
    assert_eq!(
        RealHost::default().stat("/proc").unwrap().unwrap().kind,
        Kind::Dir
    );
}

/// The argument that tells `ends_by_a_signal` it runs as the child of
/// `run_executes_without_a_shell_or_environment`.
const SIGNAL_CHILD: &str = "host-step-signal-child";

/// The child of `run_executes_without_a_shell_or_environment`: a program that a signal ends,
/// with no shell. Run any other way (`--ignored` in a whole run), it returns at once.
#[test]
#[ignore = "a helper process: run_executes_without_a_shell_or_environment runs it to see a program end by a signal"]
fn ends_by_a_signal() {
    if std::env::args().any(|a| a == SIGNAL_CHILD) {
        std::process::abort();
    }
}

fn mode_and_owner(path: &std::path::Path) -> (u32, u32, u32) {
    let m = std::fs::metadata(path).unwrap();
    (m.permissions().mode() & 0o7777, m.uid(), m.gid())
}

/// Catches a mkdir that follows a link the service user planted where a planned subdirectory
/// should be: root would chmod and chown the link's target (any directory of the host). The
/// link is refused, and its target's mode and owner are exactly what they were.
#[test]
fn mkdir_refuses_a_link_at_a_planned_subdirectory_and_changes_nothing_behind_it() {
    let (dir, host) = scratch("mkdirlink");
    let owner = me(&dir);
    let victim = dir.path().join("victim");
    std::fs::create_dir(&victim).unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let before = mode_and_owner(&victim);
    host.mkdir("/state", 0o700, owner).unwrap();
    symlink(&victim, dir.path().join("state/nanohub")).unwrap();

    let err = host.mkdir("/state/nanohub", 0o700, owner).unwrap_err();
    assert_eq!(
        err.to_string(),
        "refused: /state/nanohub: nanohub is a symbolic link or not a directory; give the real path"
    );
    // A directory to be made below the link is not made behind it.
    let err = host
        .mkdir("/state/nanohub/inner", 0o700, owner)
        .unwrap_err();
    assert!(err.to_string().contains("refused"), "{err}");
    assert!(!victim.join("inner").exists());
    assert_eq!(mode_and_owner(&victim), before);
    // A link to a file is not a directory either.
    std::fs::write(dir.path().join("afile"), b"x").unwrap();
    symlink(dir.path().join("afile"), dir.path().join("state/scep")).unwrap();
    assert!(host.mkdir("/state/scep", 0o700, owner).is_err());
    assert_eq!(std::fs::read(dir.path().join("afile")).unwrap(), b"x");
}

/// Catches a write, or a remove, that goes through a link in a directory on the way: the file
/// must not appear behind the link (a rename through it would be a write anywhere), and what
/// is behind the link must stay. A link as the file's own name is replaced, not followed.
#[test]
fn write_and_remove_refuse_a_link_on_the_way_and_replace_a_link_as_the_name() {
    let (dir, host) = scratch("writelink");
    let owner = me(&dir);
    let victim = dir.path().join("victim");
    std::fs::create_dir(&victim).unwrap();
    std::fs::write(victim.join("keep"), b"keep").unwrap();
    host.mkdir("/state", 0o700, owner).unwrap();
    symlink(&victim, dir.path().join("state/removed")).unwrap();

    let err = host
        .write("/state/removed/unit", 0o644, owner, b"unit")
        .unwrap_err();
    assert!(err.to_string().contains("refused"), "{err}");
    assert!(!victim.join("unit").exists());
    assert!(host.remove("/state/removed/keep").is_err());
    assert_eq!(std::fs::read(victim.join("keep")).unwrap(), b"keep");

    // The name itself a link: the link is replaced and its target keeps its bytes.
    symlink(victim.join("keep"), dir.path().join("state/f")).unwrap();
    host.write("/state/f", 0o600, owner, b"new").unwrap();
    assert_eq!(std::fs::read(victim.join("keep")).unwrap(), b"keep");
    let now = std::fs::symlink_metadata(dir.path().join("state/f")).unwrap();
    assert!(now.is_file() && !now.file_type().is_symlink());
    assert_eq!(std::fs::read(dir.path().join("state/f")).unwrap(), b"new");
    // Removing a name that is a link removes the link only.
    symlink(victim.join("keep"), dir.path().join("state/g")).unwrap();
    host.remove("/state/g").unwrap();
    assert!(victim.join("keep").exists() && !dir.path().join("state/g").exists());
}

/// Catches a directory owned by a user nobody trusts on the way to a path: it is refused,
/// unless it is owned by the owner the step asks for. And a path that names no file at all.
#[test]
fn a_directory_of_another_user_on_the_way_is_refused() {
    let (dir, _) = scratch("owners");
    let mine = me(&dir);
    let nobody_trusted = RealHost::trusting(dir.path(), &[]);
    let err = nobody_trusted
        .mkdir("/a/b", 0o700, (mine.0 + 1, mine.1))
        .unwrap_err();
    let said = err.to_string();
    assert!(said.starts_with("refused: /a/b: "), "{said}");
    assert!(
        said.contains(&format!("is owned by uid {}", mine.0)),
        "{said}"
    );
    assert!(said.contains("not trusted"), "{said}");
    // The owner the step asks for is trusted, and so is a user in the list.
    nobody_trusted.mkdir("/c/d", 0o700, mine).unwrap();
    RealHost::trusting(dir.path(), &[mine.0])
        .mkdir("/c/e", 0o700, mine)
        .unwrap();
    assert!(nobody_trusted.write("/", 0o600, mine, b"x").is_err());
    assert!(nobody_trusted.mkdir("/", 0o700, mine).is_err());
}

/// Catches a failed write that leaves its temporary file behind (the next write would find
/// it, and a reader would see junk): the temporary name is gone after a rename that fails, and
/// a temporary name taken by a directory is an error.
#[test]
fn a_failed_write_leaves_no_temporary_file() {
    let (dir, host) = scratch("tmp");
    let owner = me(&dir);
    host.mkdir("/d", 0o700, owner).unwrap();
    std::fs::create_dir_all(dir.path().join("d/target/inside")).unwrap();
    assert!(host.write("/d/target", 0o600, owner, b"x").is_err());
    assert!(!dir.path().join("d/target.js-host.tmp").exists());
    std::fs::create_dir(dir.path().join("d/f.js-host.tmp")).unwrap();
    assert!(host.write("/d/f", 0o600, owner, b"x").is_err());
    assert!(dir.path().join("d/f.js-host.tmp").is_dir());
    assert!(!dir.path().join("d/f").exists());
    // A missing directory is a plain "not found", and removing from one is nothing to do.
    assert_eq!(
        host.write("/missing/f", 0o600, owner, b"x")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
    host.remove("/missing/f").unwrap();
}

/// Catches a canonical path that does not resolve links on the way (a link into a work tree
/// would hide it) or that fails on a path that is not there yet: the existing part is resolved,
/// the rest is kept, and a file in the middle is an error.
#[test]
fn canonical_resolves_the_part_that_exists() {
    let (dir, host) = scratch("canon");
    std::fs::create_dir_all(dir.path().join("real/sub")).unwrap();
    symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
    assert_eq!(host.canonical("/link/sub").unwrap(), "/real/sub");
    assert_eq!(
        host.canonical("/link/sub/new/deeper").unwrap(),
        "/real/sub/new/deeper"
    );
    assert_eq!(host.canonical("/").unwrap(), "/");
    std::fs::write(dir.path().join("file"), b"x").unwrap();
    assert!(host.canonical("/file/x").is_err());
}

/// Catches system errors that are swallowed on the way: a name the file system cannot hold is
/// an error for mkdir and for write, and the error is the system's.
#[test]
fn system_errors_on_the_last_component_are_returned() {
    let (dir, host) = scratch("longname");
    let owner = me(&dir);
    let long = format!("/{}", "n".repeat(300));
    assert!(host.mkdir(&long, 0o700, owner).is_err());
    assert!(host.write(&long, 0o600, owner, b"x").is_err());
}

/// Catches a read or a stat that follows a link planted on the way: root would read (and
/// report on) whatever the service user points it at. Both are refused, for every way of
/// reading, and what is behind the link is untouched.
#[test]
fn stat_and_read_refuse_a_link_on_the_way() {
    let (dir, host) = scratch("readlink");
    let owner = me(&dir);
    let victim = dir.path().join("victim");
    std::fs::create_dir(&victim).unwrap();
    std::fs::write(victim.join("f"), b"behind the link").unwrap();
    host.mkdir("/state", 0o700, owner).unwrap();
    symlink(&victim, dir.path().join("state/sub")).unwrap();

    for path in ["/state/sub/f", "/state/sub/missing", "/state/sub/deeper/f"] {
        let err = host.stat(path).unwrap_err();
        // The message names the path and the component that is the link.
        assert_eq!(
            err.to_string(),
            format!("refused: {path}: sub is a symbolic link; give the real path")
        );
        assert!(
            host.read(path).unwrap_err().to_string().contains("refused"),
            "{path}"
        );
        assert!(
            host.read_limited(path, 100)
                .unwrap_err()
                .to_string()
                .contains("refused"),
            "{path}"
        );
    }
    assert_eq!(std::fs::read(victim.join("f")).unwrap(), b"behind the link");
}

/// Catches a read that follows a link as the file itself, and a stat that reports what the
/// link points at: a read of a final link is refused, and stat says the name is a link.
#[test]
fn a_final_link_is_not_read_and_stat_reports_it_as_a_link() {
    let (dir, host) = scratch("finallink");
    let owner = me(&dir);
    host.mkdir("/state", 0o700, owner).unwrap();
    std::fs::write(dir.path().join("state/real"), b"real").unwrap();
    symlink(dir.path().join("state/real"), dir.path().join("state/lnk")).unwrap();
    symlink(dir.path().join("state"), dir.path().join("state/dirlnk")).unwrap();

    let err = host.read("/state/lnk").unwrap_err();
    assert!(err.to_string().contains("refused"), "{err}");
    assert_eq!(host.stat("/state/lnk").unwrap().unwrap().kind, Kind::Other);
    assert_eq!(
        host.stat("/state/dirlnk").unwrap().unwrap().kind,
        Kind::Other
    );
    assert_eq!(host.stat("/state/real").unwrap().unwrap().kind, Kind::File);
    assert_eq!(host.stat("/state").unwrap().unwrap().kind, Kind::Dir);
    assert_eq!(host.read("/state/real").unwrap().unwrap(), b"real");
    // Nothing there, and no directory there, are both "nothing".
    assert_eq!(host.stat("/state/none").unwrap(), None);
    assert_eq!(host.stat("/nodir/none").unwrap(), None);
    assert_eq!(host.read("/nodir/none").unwrap(), None);
    assert_eq!(host.read_limited("/state/none", 5).unwrap(), None);
}

/// Catches a read with no bound: a file above the limit is an error of kind `FileTooLarge`
/// after at most limit + 1 bytes, one exactly at the limit is read whole.
#[test]
fn read_limited_stops_above_the_limit() {
    let (dir, host) = scratch("limit");
    std::fs::write(dir.path().join("f"), b"0123456789").unwrap();
    assert_eq!(host.read_limited("/f", 10).unwrap().unwrap(), b"0123456789");
    let err = host.read_limited("/f", 9).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::FileTooLarge);
}

/// Catches a path the host layer lets through with a `.` or `..` component, whatever the
/// caller checked: each operation refuses it before touching anything.
#[test]
fn dot_and_dotdot_components_are_refused_by_every_operation() {
    let (dir, host) = scratch("dots");
    let owner = me(&dir);
    host.mkdir("/d", 0o700, owner).unwrap();
    std::fs::write(dir.path().join("outside"), b"x").unwrap();
    for path in ["/d/../outside", "/d/./f", "/../outside", "/d/.."] {
        for err in [
            host.stat(path).unwrap_err(),
            host.read(path).unwrap_err(),
            host.write(path, 0o600, owner, b"y").unwrap_err(),
            host.mkdir(path, 0o700, owner).unwrap_err(),
            host.remove(path).unwrap_err(),
        ] {
            assert!(
                err.to_string().contains("refused") && err.to_string().contains(". or .."),
                "{path}: {err}"
            );
        }
    }
    assert!(dir.path().join("outside").exists());
}

/// Catches a root anchor that is followed: a host rooted at a link is refused for every
/// operation (the link is the thing a planted directory would be), and nothing is made
/// behind it.
#[test]
fn a_link_as_the_root_anchor_is_refused() {
    let (dir, _) = scratch("anchor");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let anchor = dir.path().join("anchor");
    symlink(&real, &anchor).unwrap();
    let host = RealHost::under(&anchor);
    let owner = me(&dir);
    assert!(host.stat("/x").unwrap_err().to_string().contains("refused"));
    assert!(host.read("/x").is_err());
    assert!(host.write("/x", 0o600, owner, b"y").is_err());
    assert!(host.mkdir("/x", 0o700, owner).is_err());
    assert!(host.remove("/x").is_err());
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
}

/// Catches a file named as a directory being called a link, or the other way round: the
/// message says "is not a directory" for a file and for the system's own error otherwise.
#[test]
fn a_file_where_a_directory_should_be_is_named_as_such() {
    let (dir, host) = scratch("notdir");
    std::fs::write(dir.path().join("file"), b"x").unwrap();
    let err = host.stat("/file/x").unwrap_err();
    assert_eq!(err.to_string(), "refused: /file/x: file is not a directory");
    assert!(host.read("/file/x").is_err());
}
