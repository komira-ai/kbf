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
