use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::*;

/// A copy of a real program, made not executable: the scratch directory lies under the
/// target directory, where `cargo llvm-cov` takes every executable for a test binary.
/// The program is this test binary itself, which exists on every host (macOS has no
/// `/bin/true`).
fn copy_program(to: &Path) {
    let me = std::env::current_exe().expect("the test binary's path");
    fs::copy(me, to).expect("copy a real program");
    fs::set_permissions(to, fs::Permissions::from_mode(0o644)).expect("not executable");
}

fn installed_copy(name: &str) -> (PathBuf, PathBuf) {
    let dir = kbf_host::testkit::scratch(name);
    let installed = dir.join("kbf-installer");
    copy_program(&installed);
    (dir, installed)
}

/// Catches a comparison that reports a pending restart for a program nobody touched (the
/// operator would restart a healthy proxy on every request), and one that compares the
/// wrong pair: running against a path other than the installed one.
///
/// Mutants planted: `replaced` returning `true` always; capture stat-ing the installed path
/// instead of the running one (red in the capture test below).
#[test]
fn an_untouched_install_is_not_replaced() {
    let (_dir, installed) = installed_copy("rb-untouched");
    let rb = RunningBinary::from_paths(&installed, &installed).expect("capture");
    assert!(!rb.replaced());
    assert!(!rb.replaced(), "asking is free and repeatable");
}

/// Catches an identity that is path-, size- or mtime-based: a package manager writes the new
/// file beside the old and renames it over, often with the same bytes and size. The running
/// program is the old inode, held through a second link as `/proc/self/exe` holds it.
///
/// Mutants planted: `replaced` returning `false` always; identity of (dev, size) instead of
/// (dev, ino); the `RestartPending` impl returning `false`.
#[test]
fn a_rename_over_the_install_path_is_replaced() {
    let (dir, installed) = installed_copy("rb-rename");
    let held = dir.join("held-old-image");
    fs::hard_link(&installed, &held).expect("hold the old inode, as the running image does");
    let rb = RunningBinary::from_paths(&held, &installed).expect("capture");
    assert!(!rb.replaced());
    let seam: &dyn RestartPending = &rb;
    assert!(!seam.restart_pending());
    let new = dir.join("kbf-installer.new");
    copy_program(&new);
    fs::rename(&new, &installed).expect("rename over, as apt and dnf do");
    assert!(rb.replaced());
    assert!(seam.restart_pending(), "the API seam answers the same");
}

/// Pins the documented limit: a rewrite in place keeps the inode and is not seen. If the
/// behaviour changes, the module notes and the design text change with it.
///
/// Mutant planted: comparing size or mtime besides the inode (this test goes red).
#[test]
fn a_rewrite_in_place_is_not_replaced() {
    let (_dir, installed) = installed_copy("rb-in-place");
    let rb = RunningBinary::from_paths(&installed, &installed).expect("capture");
    fs::write(&installed, b"truncated and written in place").expect("write in place");
    assert!(
        !rb.replaced(),
        "same inode: not seen, by the documented decision"
    );
}

/// Catches `false` for a program that is gone: the package was removed and the running proxy
/// is no longer the installed one. Also a path that was never there.
///
/// Mutant planted: a missing path reading as not replaced (`unwrap_or(false)` style).
#[test]
fn a_removed_install_path_is_replaced() {
    let (dir, installed) = installed_copy("rb-removed");
    let rb = RunningBinary::from_paths(&installed, &installed).expect("capture");
    fs::remove_file(&installed).expect("remove");
    assert!(rb.replaced());
    let never = RunningBinary::from_paths(&dir.join("kbf-installer.new"), &dir.join("x"));
    assert_eq!(
        never.expect_err("the running file must exist").kind(),
        io::ErrorKind::NotFound
    );
}

/// Catches `capture` taking its running identity from the wrong file: the test binary is the
/// running image, so pointing the installed path at it is "not replaced", and at another
/// file is "replaced". Also runs the real `capture()` once against the real install path.
///
/// Mutants planted: running identity read from the installed path (the second assert goes
/// red); `/proc/self/exe` replaced by another path.
#[test]
fn capture_names_the_running_image() {
    let me = running_path().expect("path");
    let same =
        RunningBinary::from_paths(&me, &std::env::current_exe().expect("exe")).expect("capture");
    assert!(!same.replaced());
    let (_dir, other) = installed_copy("rb-capture");
    let rb = RunningBinary::from_paths(&me, &other).expect("capture");
    assert!(rb.replaced(), "another file at the installed path");
    // The real entry point: whatever the host has at the install path, it must not fail.
    let real = RunningBinary::capture().expect("capture the running program");
    assert_eq!(real.installed, Path::new(INSTALLED));
}
