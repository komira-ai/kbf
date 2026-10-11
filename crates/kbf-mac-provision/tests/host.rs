//! [`RealHost`]: the programs it runs get an empty environment, paths are read under
//! its root, and `lstat` does not follow links. The programs run here are this
//! machine's `env` and `true`, under `/`.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use kbf_mac_provision::host::{Host, RealHost, mode_string, on_root};

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-mac-provision-host")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Catches: the caller's environment passed to the programs a check runs, where a
/// `DEVELOPER_DIR` or `SDKROOT` changes what `xcode-select -p` says, or a locale
/// changes the output a check parses.
#[test]
fn programs_run_with_only_lc_all_c() {
    let out = RealHost::new("/").run("/usr/bin/env", &[]).unwrap();
    assert!(out.success);
    assert_eq!(out.stdout, "LC_ALL=C\n");
}

/// Catches: a program's failure reported as success, or a missing program as a
/// program that printed nothing.
#[test]
fn exit_status_and_missing_programs_are_reported() {
    let host = RealHost::new("/");
    assert!(host.run("/usr/bin/true", &[]).unwrap().success);
    assert!(!host.run("/usr/bin/env", &["/nonexistent"]).unwrap().success);
    assert!(host.run("/nonexistent/program", &[]).is_err());
}

/// Catches: a root that is not joined to the Mac's paths (with or without a trailing
/// `/`), so a test reads this machine's files.
#[test]
fn paths_are_joined_to_the_root() {
    assert_eq!(on_root("/", "/etc/x"), "/etc/x");
    assert_eq!(on_root("/r", "/etc/x"), "/r/etc/x");
    assert_eq!(on_root("/r/", "/etc/x"), "/r/etc/x");
    let dir = scratch("joined");
    fs::create_dir_all(dir.join("usr/bin")).unwrap();
    let host = RealHost::new(dir.to_str().unwrap());
    assert_eq!(host.root(), dir.to_str().unwrap());
    // /usr/bin/env exists on this machine but not under the root.
    assert!(host.run("/usr/bin/env", &[]).is_err());
}

/// Catches: `lstat` following a link (so a link planted at the error log looks like
/// the file it names), or reporting the wrong type, mode or link count.
#[test]
fn lstat_reports_type_mode_links_and_owner_without_following_links() {
    let dir = scratch("lstat");
    let host = RealHost::new(dir.to_str().unwrap());
    fs::write(dir.join("file"), "x").unwrap();
    fs::set_permissions(dir.join("file"), fs::Permissions::from_mode(0o640)).unwrap();
    fs::hard_link(dir.join("file"), dir.join("hard")).unwrap();
    std::os::unix::fs::symlink(dir.join("file"), dir.join("link")).unwrap();
    fs::create_dir(dir.join("dir")).unwrap();
    let file = host.lstat("/file").unwrap();
    assert_eq!((file.kind, file.mode, file.links), ('-', 0o640, 2));
    assert_eq!(file.uid, rustix::process::geteuid().as_raw());
    assert_eq!(host.lstat("/link").unwrap().kind, 'l');
    assert_eq!(host.lstat("/dir").unwrap().kind, 'd');
    assert_eq!(host.lstat("/missing"), None);
    assert_eq!(RealHost::new("/").lstat("/dev/null").unwrap().kind, '?');
    assert!(host.euid() == rustix::process::geteuid().as_raw());
}

/// Catches: a mode string that hides a set-id or sticky bit, so a set-gid log
/// directory passes as `drwxr-xr-x`.
#[test]
fn mode_strings_show_every_bit() {
    for (kind, mode, want) in [
        ('-', 0o644, "-rw-r--r--"),
        ('d', 0o755, "drwxr-xr-x"),
        ('-', 0o4755, "-rwsr-xr-x"),
        ('-', 0o4644, "-rwSr--r--"),
        ('d', 0o2775, "drwxrwsr-x"),
        ('d', 0o2764, "drwxrwSr--"),
        ('d', 0o1777, "drwxrwxrwt"),
        ('d', 0o1776, "drwxrwxrwT"),
        ('l', 0o000, "l---------"),
    ] {
        assert_eq!(mode_string(kind, mode), want, "{mode:o}");
    }
}

/// Catches: a program run in another directory than asked, or without the variables
/// `apply` adds (`DEVELOPER_DIR` for `xcodebuild`), or with the caller's environment.
#[test]
fn run_in_sets_the_directory_and_adds_only_the_variables_given() {
    let dir = scratch("run-in");
    fs::create_dir_all(dir.join("work")).unwrap();
    let host = RealHost::new("/");
    let at = format!("{}/work", dir.display());
    let pwd = host.run_in("/bin/pwd", &[], Some(&at), &[]).unwrap();
    assert_eq!(pwd.stdout, format!("{at}\n"));
    let env = host
        .run_in("/usr/bin/env", &[], None, &[("DEVELOPER_DIR", "/x")])
        .unwrap();
    assert_eq!(env.stdout, "DEVELOPER_DIR=/x\nLC_ALL=C\n");
}

/// Catches: a write that follows a link (so `restart` empties the file a planted link
/// names), that replaces a file's owner and mode when it truncates it, or that creates
/// a file with another mode.
#[test]
fn write_creates_with_its_mode_truncates_in_place_and_refuses_links() {
    let dir = scratch("write");
    let host = RealHost::new(dir.to_str().unwrap());
    host.write("/new", b"abc", 0o600).unwrap();
    assert_eq!(fs::read(dir.join("new")).unwrap(), b"abc");
    assert_eq!(host.lstat("/new").unwrap().mode, 0o600);
    host.chmod("/new", 0o640).unwrap();
    host.write("/new", b"", 0o600).unwrap();
    assert_eq!(fs::read(dir.join("new")).unwrap(), b"");
    assert_eq!(host.lstat("/new").unwrap().mode, 0o640);
    fs::write(dir.join("target"), "keep").unwrap();
    std::os::unix::fs::symlink(dir.join("target"), dir.join("link")).unwrap();
    assert!(host.write("/link", b"", 0o600).is_err());
    assert_eq!(fs::read_to_string(dir.join("target")).unwrap(), "keep");
}

/// Catches: `chown` changing what a link names instead of the link, `rename` not
/// replacing its target, `mkdir` not making the directories above, and `remove`
/// following a link, failing on nothing, or leaving a directory's contents.
#[test]
fn mkdir_chown_rename_and_remove_act_on_the_path_itself() {
    let dir = scratch("change");
    let host = RealHost::new(dir.to_str().unwrap());
    host.mkdir("/a/b/c").unwrap();
    assert!(dir.join("a/b/c").is_dir());
    let (uid, gid) = (host.lstat("/a").unwrap().uid, host.lstat("/a").unwrap().gid);
    fs::write(dir.join("a/file"), "x").unwrap();
    std::os::unix::fs::symlink(dir.join("a/file"), dir.join("a/link")).unwrap();
    host.chown("/a/link", uid, gid).unwrap();
    assert!(host.chown("/missing", uid, gid).is_err());
    fs::write(dir.join("a/other"), "y").unwrap();
    host.rename("/a/other", "/a/file").unwrap();
    assert_eq!(fs::read_to_string(dir.join("a/file")).unwrap(), "y");
    host.remove("/a/link").unwrap();
    assert_eq!(fs::read_to_string(dir.join("a/file")).unwrap(), "y");
    host.remove("/a/file").unwrap();
    host.remove("/a/missing").unwrap();
    host.remove("/a").unwrap();
    assert!(!dir.join("a").exists());
    // A path through a file is an error, not nothing.
    fs::write(dir.join("f"), "").unwrap();
    assert!(host.remove("/f/x").is_err());
    host.pause(0);
}
