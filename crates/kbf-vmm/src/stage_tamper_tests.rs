//! Tests of `stage` against a Data volume an earlier guest boot has tampered with (links
//! planted where the stage writes, files it cannot remove), and of what a failed stage leaves
//! behind. Nothing here needs root or macOS.

use super::*;

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

fn mode_of(p: &Path) -> u32 {
    std::fs::metadata(p).expect("meta").permissions().mode() & 0o7777
}

/// The Data volume of a test with the directories a guest has by the time the stage runs.
fn tampered_volume(dir: &Path) -> PathBuf {
    let v = volume_in(dir);
    std::fs::create_dir_all(v.join("private/var/kbf-sim/runtimes")).expect("runtimes");
    std::fs::create_dir_all(v.join("Applications")).expect("applications");
    v
}

/// Catches a manifest written through a link the guest planted at its name (a host file
/// overwritten as root): the link is replaced by a new file, and what it pointed at is not
/// touched.
#[test]
fn a_link_planted_at_the_manifest_is_replaced_and_not_written_through() {
    let dir = tmp("tamper-manifest");
    let volume = tampered_volume(&dir);
    let outside = dir.join("host-file");
    write(&outside, "keep me", 0o640);
    let manifest = volume.join("private/var/kbf-sim/staged.json");
    std::os::unix::fs::symlink(&outside, &manifest).expect("plant");
    Harness::new(100 * GIB)
        .run(&volume, &Plan::default())
        .expect("execute");
    assert_eq!(std::fs::read_to_string(&outside).expect("host"), "keep me");
    assert_eq!(mode_of(&outside), 0o640);
    assert!(
        std::fs::symlink_metadata(&manifest)
            .expect("manifest")
            .is_file()
    );
    assert_eq!(mode_of(&manifest), 0o644);
}

/// Catches a removal that fails (a file the guest made immutable) being ignored, after which
/// the manifest was written through the link that stayed: the stage stops, names the path, and
/// the host file is untouched. Every other place a leftover is removed is the same.
#[test]
fn a_leftover_that_cannot_be_removed_stops_the_stage_and_touches_nothing_outside() {
    // (the name whose removal fails, which removal of it)
    for (name, n) in [
        ("staged.json", 0),
        ("Xcode.app", 0),
        (".Xcode.app.kbf-staging", 0),
        ("stale.dmg", 0),
        (".runtimes.kbf-staging", 0),
        ("iOS.dmg", 0),
        ("old.app", 0),
    ] {
        let dir = tmp("tamper-immutable");
        let app = fake_xcode(&dir);
        write(&dir.join("iOS.dmg"), "hello", 0o644);
        let demo = fake_app(&dir, "Demo.app");
        let plan =
            plan_with_app(Some(&app), &[dir.join("iOS.dmg")], Some(&demo), false).expect("plan");
        let volume = tampered_volume(&dir);
        std::fs::create_dir_all(volume.join("private/var/kbf-sim/apps/old.app")).expect("old");
        let outside = dir.join("host-file");
        write(&outside, "keep me", 0o644);
        let sim = volume.join("private/var/kbf-sim");
        std::os::unix::fs::symlink(&outside, sim.join("staged.json")).expect("plant");
        std::fs::create_dir_all(volume.join("Applications/Xcode.app")).expect("old");
        std::fs::create_dir_all(volume.join("Applications/.Xcode.app.kbf-staging")).expect("old");
        write(&sim.join("runtimes/stale.dmg"), "old", 0o644);
        write(&sim.join("runtimes/iOS.dmg"), "old", 0o644);
        write(&sim.join(".runtimes.kbf-staging"), "old", 0o644);
        let mut h = Harness::new(100 * GIB);
        h.unremovable = Some((name, n));
        let e = h.run(&volume, &plan).expect_err(name);
        assert!(e.starts_with("remove ") && e.contains(name), "{name}: {e}");
        assert_eq!(std::fs::read_to_string(&outside).expect("host"), "keep me");
    }
}

/// Catches a manifest written by path with `fs::write`: a link that is still there (the
/// removal did nothing) must make the create fail, not be followed.
#[test]
fn the_manifest_is_created_exclusively() {
    let dir = tmp("tamper-excl");
    let outside = dir.join("host-file");
    write(&outside, "keep me", 0o644);
    let at = dir.join("staged.json");
    std::os::unix::fs::symlink(&outside, &at).expect("plant");
    let (free, clone) = (|_: &Path| Ok(0u64), |_: &Path, _: &Path| Ok(()));
    let (chown, remove) = (|_: &Path| Ok(()), |_: &Path| Ok(()));
    let env = Env {
        free_bytes: &free,
        clone_tree: &clone,
        chown_root: &chown,
        remove: &remove,
    };
    let e = write_text(&env, &at, "text", 0o644).unwrap_err();
    assert!(e.starts_with("write "), "{e}");
    assert_eq!(std::fs::read_to_string(&outside).expect("host"), "keep me");
}

/// Catches a mode set by path: a link there is an error and the file it points at keeps its
/// mode; a pipe is opened without blocking.
#[test]
fn a_mode_is_set_on_the_open_file_and_never_through_a_link() {
    let dir = tmp("tamper-chmod");
    let outside = dir.join("host-file");
    write(&outside, "x", 0o640);
    let at = dir.join("link");
    std::os::unix::fs::symlink(&outside, &at).expect("plant");
    assert!(chmod(&at, 0o600).unwrap_err().starts_with("chmod "));
    assert_eq!(mode_of(&outside), 0o640);
    let fifo = dir.join("fifo");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("path");
    // SAFETY: `c` is a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    chmod(&fifo, 0o600).expect("a pipe is not waited for");
    assert_eq!(mode_of(&fifo), 0o600);
}

/// Catches a source that grew being copied whole: at most one byte past the plan is read, and
/// the copy is an error.
#[test]
fn a_file_that_grew_is_copied_no_further_than_one_byte_past_the_plan() {
    let dir = tmp("tamper-bound");
    let src = dir.join("big");
    std::fs::write(&src, vec![7u8; 3 << 20]).expect("big");
    for hashed in [false, true] {
        let dst = dir.join(format!("out-{hashed}"));
        let e = if hashed {
            copy_file_hashed(&src, &dst, 0o644, 5).unwrap_err()
        } else {
            copy_file(&src, &dst, 0o644, 5).unwrap_err()
        };
        assert!(
            e.contains("changed while it was copied (5 bytes planned, it grew past that)"),
            "{e}"
        );
        assert_eq!(std::fs::metadata(&dst).expect("dst").len(), 6);
    }
}

fn staging_of_xcode(volume: &Path) -> PathBuf {
    volume.join("Applications/.Xcode.app.kbf-staging")
}

/// Catches a partial Xcode.app left under the staging name or the final one when the copy
/// fails part way (a file gone since the plan), the clone is refused, or an ownership change
/// fails.
#[test]
fn a_failed_xcode_build_leaves_no_staging_tree_and_no_app() {
    type Setup = fn(&mut Harness, &Path);
    let cases: [(&str, Setup); 3] = [
        ("copy", |_, app| {
            std::fs::remove_file(app.join("Contents/Info.plist")).expect("gone");
        }),
        ("clone", |h, _| {
            h.clone_ok = true;
            h.after_clone = Some(|dst| write(&dst.join("extra"), "x", 0o644));
        }),
        ("chown", |h, _| h.chown_fails_at = Some(3)),
    ];
    for (what, setup) in cases {
        let dir = tmp("tamper-xcode");
        let app = fake_xcode(&dir);
        let plan = plan(Some(&app), &[], false).expect("plan");
        let volume = tampered_volume(&dir);
        let mut h = Harness::new(100 * GIB);
        setup(&mut h, &app);
        h.run(&volume, &plan).expect_err(what);
        assert!(!exists(&staging_of_xcode(&volume)), "{what}");
        assert!(!exists(&volume.join("Applications/Xcode.app")), "{what}");
    }
}

/// Catches a failed removal of the partial tree being lost: the error says both what failed
/// and that the partial copy is still there.
#[test]
fn a_partial_tree_that_cannot_be_removed_is_reported() {
    let dir = tmp("tamper-discard");
    let app = fake_xcode(&dir);
    let plan = plan(Some(&app), &[], false).expect("plan");
    std::fs::remove_file(app.join("Contents/Info.plist")).expect("gone");
    let volume = tampered_volume(&dir);
    let mut h = Harness::new(100 * GIB);
    // Removals of the staging name: before the build, after the failed clone, then the discard.
    h.unremovable = Some((".Xcode.app.kbf-staging", 2));
    let e = h.run(&volume, &plan).unwrap_err();
    assert!(
        e.contains("copy ") && e.contains("could not be removed"),
        "{e}"
    );
    assert!(e.contains("Operation not permitted"), "{e}");
}

/// Catches a partial runtime left where the guest lists runtimes (its name in the runtime
/// directory) or beside it (the staging name) after a bundle copy fails, a disk image grew
/// since the plan, or one was swapped for a link.
#[test]
fn a_failed_runtime_build_leaves_no_partial_runtime_and_no_staging_directory() {
    type Setup = fn(&Path, &Path);
    let cases: [(&str, &str, Setup); 3] = [
        ("bundle", "BuildManifest.plist", |_, b| {
            std::fs::remove_file(b.join("Restore/BuildManifest.plist")).expect("gone");
        }),
        ("grew", "changed while it was copied", |d, _| {
            write(&d.join("iOS.dmg"), "hello, and more", 0o644);
        }),
        ("swapped", "read ", |d, _| {
            std::fs::remove_file(d.join("iOS.dmg")).expect("rm");
            std::os::unix::fs::symlink(d.join("other"), d.join("iOS.dmg")).expect("link");
        }),
    ];
    for (what, expected, setup) in cases {
        let dir = tmp("tamper-runtime");
        let bundle = super::bundle::fake_bundle(&dir, "iOS_27.exportedBundle");
        write(&dir.join("iOS.dmg"), "hello", 0o644);
        write(&dir.join("other"), "hello", 0o644);
        let runtimes = [dir.join("iOS.dmg"), bundle.clone()];
        let plan = plan(None, &runtimes, false).expect("plan");
        setup(&dir, &bundle);
        let volume = tampered_volume(&dir);
        let rt = volume.join("private/var/kbf-sim/runtimes");
        // The dmg is staged first, so it fails first unless it is the bundle that changed.
        let e = Harness::new(100 * GIB).run(&volume, &plan).expect_err(what);
        assert!(e.contains(expected), "{what}: {e}");
        assert!(
            !exists(&rt.with_file_name(".runtimes.kbf-staging")),
            "{what}"
        );
        let left: Vec<_> = std::fs::read_dir(&rt)
            .expect("runtimes")
            .map(|e| e.expect("e").file_name())
            .collect();
        let failing = if what == "bundle" {
            "iOS_27.exportedBundle"
        } else {
            "iOS.dmg"
        };
        assert!(!left.iter().any(|n| n == failing), "{what}: {left:?}");
    }
}

/// Catches group and world write surviving in the plan or in the copy.
#[test]
fn planned_modes_lose_group_and_world_write_and_keep_the_rest() {
    let dir = tmp("tamper-modes");
    let app = fake_xcode(&dir);
    let open_file = app.join("Contents/Info.plist");
    std::fs::set_permissions(&open_file, std::fs::Permissions::from_mode(0o666)).expect("m");
    let open_dir = app.join("Contents/Frameworks");
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o775)).expect("m");
    let private = app.join("Contents/Developer/usr/bin/xcodebuild");
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).expect("m");
    let plan = plan(Some(&app), &[], false).expect("plan");
    let planned = |rel: &str| {
        plan.xcode
            .as_ref()
            .expect("x")
            .nodes
            .iter()
            .find_map(|n| match n {
                Node::File { rel: r, mode, .. } | Node::Dir { rel: r, mode }
                    if r == Path::new(rel) =>
                {
                    Some(*mode)
                }
                _ => None,
            })
    };
    assert_eq!(planned("Contents/Info.plist"), Some(0o644));
    assert_eq!(planned("Contents/Frameworks"), Some(0o755));
    assert_eq!(
        planned("Contents/Developer/usr/bin/xcodebuild"),
        Some(0o700)
    );
    let volume = tampered_volume(&dir);
    Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect("execute");
    let staged = volume.join("Applications/Xcode.app/Contents");
    assert_eq!(mode_of(&staged.join("Info.plist")), 0o644);
    assert_eq!(mode_of(&staged.join("Frameworks")), 0o755);
}

/// Catches a partial app bundle left under its staging name in the app directory (where the
/// guest looks for what to install) when the copy fails.
#[test]
fn a_failed_app_build_leaves_no_staging_tree_in_the_app_directory() {
    let dir = tmp("tamper-app");
    let demo = fake_app(&dir, "Demo.app");
    let plan = plan_with_app(None, &[], Some(&demo), false).expect("plan");
    std::fs::remove_file(demo.join("Info.plist")).expect("gone");
    let volume = tampered_volume(&dir);
    Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect_err("copy");
    let apps = volume.join("private/var/kbf-sim/apps");
    assert_eq!(std::fs::read_dir(apps).expect("apps").count(), 0);
}
