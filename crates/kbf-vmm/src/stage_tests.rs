//! Tests of `stage`: the plan of an Xcode.app and runtime images, and writing a plan into
//! a fake Data volume (a temporary directory). Nothing here needs root or macOS; ownership
//! and the tree clone are replaced through [`Env`].

use std::cell::RefCell;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::fake::{holds, tmp};
use crate::stage::link as stage_link;
use crate::stage::*;

#[path = "stage_bundle_tests.rs"]
mod bundle;

#[path = "stage_primed_tests.rs"]
mod primed;

#[path = "stage_tamper_tests.rs"]
mod tamper;

#[path = "stage_xctest_tests.rs"]
mod xctest;

#[cfg(target_os = "macos")]
#[path = "stage_xattr_tests.rs"]
mod xattr;

/// The SHA-256 of the bytes `hello`.
const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

fn write(path: &Path, text: &str, mode: u32) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
    std::fs::write(path, text).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn link(target: &str, at: &Path) {
    std::fs::create_dir_all(at.parent().expect("parent")).expect("dirs");
    std::os::unix::fs::symlink(target, at).expect("symlink");
}

/// A small Xcode.app: a tool, a plist, a framework with the usual version links.
fn fake_xcode(dir: &Path) -> PathBuf {
    let app = dir.join("Xcode.app");
    write(
        &app.join("Contents/Developer/usr/bin/xcodebuild"),
        "tool",
        0o755,
    );
    write(&app.join("Contents/Info.plist"), "plist", 0o644);
    write(
        &app.join("Contents/Frameworks/A.framework/Versions/A/lib"),
        "lib!",
        0o644,
    );
    link(
        "A",
        &app.join("Contents/Frameworks/A.framework/Versions/Current"),
    );
    link(
        "Versions/Current/lib",
        &app.join("Contents/Frameworks/A.framework/lib"),
    );
    link("../Info.plist", &app.join("Contents/Developer/Info.plist"));
    app
}

fn err_of(r: Result<TreePlan, StageError>) -> String {
    r.expect_err("refused").to_string()
}

/// Catches a plan that lists a file before its directory (the copy would fail), loses a
/// size, counts a link as a file, or follows a link instead of recording its target.
#[test]
fn the_plan_of_an_app_lists_directories_first_with_sizes_and_link_targets() {
    let dir = tmp("stage-plan");
    let app = fake_xcode(&dir);
    let p = plan_xcode(&app, false).expect("plan");
    assert_eq!(
        p.bytes,
        ("tool".len() + "plist".len() + "lib!".len()) as u64
    );
    let seen: Vec<String> = p
        .nodes
        .iter()
        .map(|n| match n {
            Node::Dir { rel, .. } => format!("d {}", rel.display()),
            Node::File { rel, bytes, .. } => format!("f {} {bytes}", rel.display()),
            Node::Link { rel, target } => format!("l {} -> {}", rel.display(), target.display()),
        })
        .collect();
    let at = |s: &str| {
        seen.iter()
            .position(|x| x == s)
            .unwrap_or_else(|| panic!("{s} in {seen:?}"))
    };
    assert!(at("d Contents") < at("f Contents/Info.plist 5"));
    assert!(
        at("d Contents/Frameworks/A.framework/Versions/A")
            < at("f Contents/Frameworks/A.framework/Versions/A/lib 4")
    );
    assert!(holds(&[
        seen.contains(&"l Contents/Frameworks/A.framework/Versions/Current -> A".to_owned()),
        seen.contains(&"l Contents/Frameworks/A.framework/lib -> Versions/Current/lib".to_owned()),
        seen.contains(&"l Contents/Developer/Info.plist -> ../Info.plist".to_owned()),
    ]));
    assert!(p.external_links.is_empty());
    // The same tree gives the same plan, in the same order.
    assert_eq!(plan_xcode(&app, false).expect("again"), p);
}

/// Catches modes copied with the setuid, setgid or sticky bits.
#[test]
fn modes_drop_the_setuid_setgid_and_sticky_bits() {
    let dir = tmp("stage-modes");
    let app = fake_xcode(&dir);
    let tool = app.join("Contents/Developer/usr/bin/xcodebuild");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o6755)).expect("chmod");
    let p = plan_xcode(&app, false).expect("plan");
    let mode = p.nodes.iter().find_map(|n| match n {
        Node::File { rel, mode, .. } if rel.ends_with("xcodebuild") => Some(*mode),
        _ => None,
    });
    assert_eq!(mode, Some(0o755));
}

/// Catches an `--xcode` that is a link (its target could be anything), a directory that
/// is not an Xcode.app, a plain file, and a missing path.
#[test]
fn the_app_itself_must_be_a_real_xcode_directory() {
    let dir = tmp("stage-root");
    let app = fake_xcode(&dir);
    link("Xcode.app", &dir.join("Alias.app"));
    assert_eq!(
        plan_xcode(&dir.join("Alias.app"), false),
        Err(StageError::Symlink(dir.join("Alias.app")))
    );
    std::fs::create_dir_all(dir.join("Empty.app/Contents")).expect("empty");
    assert!(err_of(plan_xcode(&dir.join("Empty.app"), false)).contains("not an Xcode.app"));
    write(&dir.join("file.app"), "x", 0o644);
    assert!(err_of(plan_xcode(&dir.join("file.app"), false)).contains("not an Xcode.app"));
    assert!(err_of(plan_xcode(&dir.join("none.app"), false)).contains("stat"));
    // A tool that is itself a link does not count as the tool.
    std::fs::remove_file(app.join("Contents/Developer/usr/bin/xcodebuild")).expect("rm");
    link(
        "/usr/bin/true",
        &app.join("Contents/Developer/usr/bin/xcodebuild"),
    );
    assert!(err_of(plan_xcode(&app, true)).contains("not an Xcode.app"));
}

/// Catches a link rule that lets a target out of the app: an absolute path elsewhere, one
/// into the app that climbs out with `..`, more `..` than the link is deep, or a `..` after a
/// name (which climbs out through a link the name goes through); and one that refuses a
/// link that stays inside.
#[test]
fn a_link_target_must_stay_inside_the_app() {
    let inside =
        |t: &str, depth: usize| stays_inside(Path::new(t), depth, "/Applications/Xcode.app");
    for ok in [
        ("A", 0),
        ("Versions/Current/lib", 3),
        ("../Info.plist", 1),
        ("../../x", 2),
        (".", 0),
        ("./a", 0),
        ("/Applications/Xcode.app/Contents/lib", 0),
        ("/Applications/Xcode.app", 5),
    ] {
        assert!(inside(ok.0, ok.1), "{ok:?}");
    }
    for bad in [
        ("..", 0),
        ("../x", 0),
        ("../../x", 1),
        ("a/../..", 5),
        ("a/..", 5),
        ("", 3),
        ("/usr/lib/x", 3),
        ("/Applications/Xcode.appx/x", 0),
        ("/Applications/Xcode.app/../../etc", 0),
        ("/", 0),
    ] {
        assert!(!inside(bad.0, bad.1), "{bad:?}");
    }
}

/// Catches an escaping link that is copied by default, one whose refusal does not name the
/// link, the target and the flag, and a flag that does not list what it let through.
#[test]
fn an_escaping_link_is_refused_unless_allowed_and_then_listed() {
    let dir = tmp("stage-escape");
    let app = fake_xcode(&dir);
    link("/usr/share/man", &app.join("Contents/Developer/usr/man"));
    let e = err_of(plan_xcode(&app, false));
    assert!(
        holds(&[
            e.contains("Contents/Developer/usr/man"),
            e.contains("/usr/share/man"),
            e.contains("--allow-external-links"),
        ]),
        "{e}"
    );
    let p = plan_xcode(&app, true).expect("allowed");
    assert_eq!(
        p.external_links,
        vec![(
            PathBuf::from("Contents/Developer/usr/man"),
            PathBuf::from("/usr/share/man")
        )]
    );
    // Too many `..` for the link's depth is an escape too.
    let dir2 = tmp("stage-escape2");
    let app2 = fake_xcode(&dir2);
    link("../../../../..", &app2.join("Contents/up"));
    assert!(err_of(plan_xcode(&app2, false)).contains("leaves the app"));
}

/// Catches a socket or other special file being skipped silently or copied as a file.
#[test]
fn a_special_file_is_refused() {
    let dir = tmp("stage-special");
    let app = fake_xcode(&dir);
    let _socket = std::os::unix::net::UnixListener::bind(app.join("Contents/s")).expect("socket");
    let e = err_of(plan_xcode(&app, true));
    assert!(e.contains("Contents/s") && e.contains("socket"), "{e}");
}

/// Catches a walk with no depth bound.
#[test]
fn a_tree_nested_past_the_depth_limit_is_refused() {
    let dir = tmp("stage-deep");
    let app = fake_xcode(&dir);
    let mut deep = app.join("Contents");
    for _ in 0..=MAX_DEPTH {
        deep.push("d");
    }
    std::fs::create_dir_all(&deep).expect("deep");
    assert!(err_of(plan_xcode(&app, false)).contains("deeper than"));
}

/// Catches a runtime name that could climb out of the runtime directory, hide there, or make
/// a result frame name the host refuses (`..`, over 64 bytes), a
/// link or directory taken for the image, a repeated name, and a list without a bound.
#[test]
fn runtimes_are_plain_dmg_files_with_distinct_safe_names() {
    let dir = tmp("stage-runtimes");
    write(&dir.join("iOS_27.dmg"), "hello", 0o644);
    write(&dir.join("watchOS-27.0.dmg"), "hello!", 0o644);
    let ok = plan_runtimes(&[dir.join("iOS_27.dmg"), dir.join("watchOS-27.0.dmg")]).expect("ok");
    assert_eq!(
        ok.iter()
            .map(|r| (r.name.as_str(), r.bytes))
            .collect::<Vec<_>>(),
        vec![("iOS_27.dmg", 5), ("watchOS-27.0.dmg", 6)]
    );
    assert_eq!(plan_runtimes(&[]), Ok(Vec::new()));
    // The longest allowed name is accepted: the guest turns it into a frame name.
    let longest = format!("{}.dmg", "n".repeat(MAX_RUNTIME_NAME_BYTES - 4));
    write(&dir.join(&longest), "x", 0o644);
    assert_eq!(
        plan_runtimes(&[dir.join(&longest)]).expect("longest")[0].name,
        longest
    );
    for bad in [
        "a b.dmg",
        "x.dmg.zip",
        ".dmg",
        ".hidden.dmg",
        "semi;colon.dmg",
        "ü.dmg",
        "plain",
        "a..b.dmg",
        &format!("{}.dmg", "n".repeat(MAX_RUNTIME_NAME_BYTES)),
    ] {
        write(&dir.join(bad), "x", 0o644);
        assert_eq!(
            plan_runtimes(&[dir.join(bad)]),
            Err(StageError::BadRuntimeName(dir.join(bad))),
            "{bad}"
        );
    }
    link("iOS_27.dmg", &dir.join("link.dmg"));
    assert_eq!(
        plan_runtimes(&[dir.join("link.dmg")]),
        Err(StageError::Symlink(dir.join("link.dmg")))
    );
    std::fs::create_dir(dir.join("dir.dmg")).expect("dir");
    assert_eq!(
        plan_runtimes(&[dir.join("dir.dmg")]),
        Err(StageError::NotRegular(dir.join("dir.dmg")))
    );
    write(&dir.join("other/iOS_27.dmg"), "z", 0o644);
    assert_eq!(
        plan_runtimes(&[dir.join("iOS_27.dmg"), dir.join("other/iOS_27.dmg")]),
        Err(StageError::DuplicateRuntime("iOS_27.dmg".into()))
    );
    let many: Vec<PathBuf> = (0..=MAX_RUNTIMES).map(|_| dir.join("iOS_27.dmg")).collect();
    assert_eq!(plan_runtimes(&many), Err(StageError::TooManyRuntimes));
    assert!(plan_runtimes(&[dir.join("missing.dmg")]).is_err());
}

/// What a test replaces of the host: the free space, whether the tree clone works, and the
/// paths ownership was set on.
struct Harness {
    free: u64,
    /// Reading the free space fails.
    free_fails: bool,
    clone_ok: bool,
    /// The n-th (from 0) ownership change fails.
    chown_fails_at: Option<usize>,
    chowned: RefCell<Vec<PathBuf>>,
    clones: RefCell<u32>,
    /// What a clone does to the tree it made, after it made it (the host tree changed
    /// under the clone, or the clone kept a bit).
    after_clone: Option<fn(&Path)>,
    /// The n-th (from 0) removal of a path ending in the name fails, as a file the guest made
    /// immutable does.
    unremovable: Option<(&'static str, usize)>,
    removals: RefCell<usize>,
}

impl Harness {
    fn new(free: u64) -> Self {
        Self {
            free,
            free_fails: false,
            clone_ok: false,
            chown_fails_at: None,
            chowned: RefCell::new(Vec::new()),
            clones: RefCell::new(0),
            after_clone: None,
            unremovable: None,
            removals: RefCell::new(0),
        }
    }

    fn run(&self, volume: &Path, plan: &Plan) -> Result<Value, String> {
        let free = |_: &Path| {
            if self.free_fails {
                Err(std::io::Error::other("statvfs failed"))
            } else {
                Ok(self.free)
            }
        };
        let clone = |src: &Path, dst: &Path| {
            *self.clones.borrow_mut() += 1;
            // A failed clone can leave part of the tree behind.
            std::fs::create_dir(dst)?;
            std::fs::write(dst.join("junk"), "left over")?;
            if !self.clone_ok {
                return Err(std::io::Error::from_raw_os_error(libc::EXDEV));
            }
            std::fs::remove_dir_all(dst)?;
            copy_tree(src, dst)?;
            self.after_clone.iter().for_each(|f| f(dst));
            Ok(())
        };
        let chown = |p: &Path| {
            let n = self.chowned.borrow().len();
            self.chowned.borrow_mut().push(p.to_owned());
            if self.chown_fails_at == Some(n) {
                Err(std::io::Error::other("not permitted"))
            } else {
                Ok(())
            }
        };
        let remove = |p: &Path| {
            if let Some((name, n)) = self.unremovable
                && p.ends_with(name)
            {
                let seen = *self.removals.borrow();
                *self.removals.borrow_mut() += 1;
                if seen == n {
                    return Err(std::io::Error::other("Operation not permitted"));
                }
            }
            remove_path(p)
        };
        execute(
            volume,
            plan,
            &Env {
                free_bytes: &free,
                clone_tree: &clone,
                chown_root: &chown,
                remove: &remove,
            },
        )
    }
}

/// A tree copy as a clone would make: same entries, same link targets.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        let ft = e.file_type()?;
        if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&s)?, &d)?;
        } else if ft.is_dir() {
            copy_tree(&s, &d)?;
        } else {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

const GIB: u64 = 1 << 30;

fn volume_in(dir: &Path) -> PathBuf {
    let v = dir.join("volume");
    std::fs::create_dir_all(v.join("private/var")).expect("volume");
    v
}

/// Lists every entry under `root` as `kind path [detail]`, sorted, for comparing trees.
fn listing(root: &Path) -> Vec<String> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(at)
            .expect("dir")
            .map(|e| e.expect("e"))
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            let p = e.path();
            let rel = p.strip_prefix(root).expect("rel").display().to_string();
            let meta = std::fs::symlink_metadata(&p).expect("meta");
            if meta.file_type().is_symlink() {
                out.push(format!(
                    "l {rel} -> {}",
                    std::fs::read_link(&p).expect("l").display()
                ));
            } else if meta.is_dir() {
                out.push(format!("d {rel} {:o}", meta.permissions().mode() & 0o777));
                walk(root, &p, out);
            } else {
                out.push(format!(
                    "f {rel} {:o} {}",
                    meta.permissions().mode() & 0o777,
                    std::fs::read_to_string(&p).expect("text")
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// Catches a copy that drops a file, a mode, a directory or a link, replaces a link by what
/// it points at, leaves the staging directory behind, skips the ownership of some entry, or
/// does not recover from a clone that failed half way.
#[test]
fn xcode_is_copied_entry_for_entry_when_the_clone_is_refused() {
    let dir = tmp("stage-copy");
    let app = fake_xcode(&dir);
    let volume = volume_in(&dir);
    let plan = plan(Some(&app), &[], false).expect("plan");
    let h = Harness::new(100 * GIB);
    let out = h.run(&volume, &plan).expect("execute");
    assert_eq!(*h.clones.borrow(), 1);
    assert_eq!(out["xcode"]["method"], "copy");
    assert_eq!(
        listing(&volume.join("Applications/Xcode.app")),
        listing(&app)
    );
    // Directory modes are final (the read-only case is covered below).
    assert_eq!(
        std::fs::metadata(
            volume.join("Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild")
        )
        .expect("meta")
        .permissions()
        .mode()
            & 0o777,
        0o755
    );
    let apps = listing(&volume.join("Applications"));
    assert!(apps.iter().all(|l| !l.contains("kbf-staging")), "{apps:?}");
    assert!(!volume.join("Applications/.Xcode.app.kbf-staging").exists());
    // Every entry of the copy (made under the staging name, before the rename), the app
    // directory and `Applications` itself were handed to root.
    let owned = h.chowned.borrow();
    for node in &plan.xcode.as_ref().expect("xcode").nodes {
        let want = volume
            .join("Applications/.Xcode.app.kbf-staging")
            .join(match node {
                Node::Dir { rel, .. } | Node::File { rel, .. } | Node::Link { rel, .. } => rel,
            });
        assert!(owned.contains(&want), "{}", want.display());
    }
    assert!(owned.contains(&volume.join("Applications/.Xcode.app.kbf-staging")));
    assert!(owned.contains(&volume.join("Applications")));
}

/// Catches a copy that cannot write into a directory whose final mode is read-only, and a
/// final mode that is not applied.
#[test]
fn a_read_only_directory_is_filled_and_then_closed() {
    let dir = tmp("stage-ro");
    let app = fake_xcode(&dir);
    let ro = app.join("Contents/Frameworks");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    let volume = volume_in(&dir);
    let plan = plan(Some(&app), &[], false).expect("plan");
    Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect("execute");
    let mode = std::fs::metadata(volume.join("Applications/Xcode.app/Contents/Frameworks"))
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o555);
    // Let the temporary directory be removed.
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::set_permissions(
        volume.join("Applications/Xcode.app/Contents/Frameworks"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
}

/// Catches a tree clone result that is not used, per-file copying after a good clone, and
/// ownership skipped on the clone path (a clone keeps the host user as owner).
#[test]
fn a_tree_clone_is_used_when_it_works_and_owned_by_root_afterwards() {
    let dir = tmp("stage-clone");
    let app = fake_xcode(&dir);
    let volume = volume_in(&dir);
    let plan = plan(Some(&app), &[], false).expect("plan");
    let mut h = Harness::new(100 * GIB);
    h.clone_ok = true;
    let out = h.run(&volume, &plan).expect("execute");
    assert_eq!(out["xcode"]["method"], "clone");
    assert_eq!(
        listing(&volume.join("Applications/Xcode.app")),
        listing(&app)
    );
    assert!(
        h.chowned
            .borrow()
            .contains(&volume.join("Applications/.Xcode.app.kbf-staging/Contents/Info.plist"))
    );
    assert!(h.chowned.borrow().contains(
        &volume.join("Applications/.Xcode.app.kbf-staging/Contents/Developer/Info.plist")
    ));
}

/// Catches an old Xcode.app that stays beside the new one, and a leftover staging
/// directory of an earlier failed run that breaks the next one.
#[test]
fn a_rerun_replaces_the_previous_copy_and_a_failed_runs_leftovers() {
    let dir = tmp("stage-rerun");
    let app = fake_xcode(&dir);
    let volume = volume_in(&dir);
    write(
        &volume.join("Applications/Xcode.app/old-file"),
        "old",
        0o644,
    );
    write(
        &volume.join("Applications/.Xcode.app.kbf-staging/half"),
        "half",
        0o644,
    );
    let plan = plan(Some(&app), &[], false).expect("plan");
    Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect("execute");
    assert!(!volume.join("Applications/Xcode.app/old-file").exists());
    assert!(!volume.join("Applications/.Xcode.app.kbf-staging").exists());
    assert_eq!(
        listing(&volume.join("Applications/Xcode.app")),
        listing(&app)
    );
}

/// Catches a space check that is missing, off by one at the boundary, counts only part of
/// the bytes (the runtimes), writes anything before refusing, or gives an error that does
/// not say what is needed, what is free, or what to do.
#[test]
fn staging_that_does_not_fit_is_refused_before_anything_is_written() {
    let dir = tmp("stage-space");
    let app = fake_xcode(&dir);
    write(&dir.join("rt.dmg"), "hello", 0o644);
    let plan = plan(Some(&app), &[dir.join("rt.dmg")], false).expect("plan");
    assert_eq!(plan.bytes(), 4 + 5 + 4 + 5);
    let volume = volume_in(&dir);
    let tight = plan.bytes() + HEADROOM_BYTES;
    let e = Harness::new(tight - 1)
        .run(&volume, &plan)
        .expect_err("too small");
    assert!(
        holds(&[
            e.contains("not enough room"),
            e.contains("--disk-gib"),
            e.contains("4.0 GiB headroom"),
            e.contains("free"),
        ]),
        "{e}"
    );
    assert!(!volume.join("Applications").exists());
    assert!(!volume.join("private/var/kbf-sim").exists());
    Harness::new(tight)
        .run(&volume, &plan)
        .expect("exactly enough");
    // Only the runtimes need room when only they are staged.
    let only_rt = tmp("stage-space-rt");
    write(&only_rt.join("big.dmg"), "hello", 0o644);
    let mut p = crate::stage::plan(None, &[only_rt.join("big.dmg")], false).expect("plan");
    p.runtimes[0].bytes = 9 * GIB;
    let v = volume_in(&only_rt);
    let e = Harness::new(12 * GIB)
        .run(&v, &p)
        .expect_err("9 GiB + 4 GiB > 12 GiB");
    assert!(e.contains("9.0 GiB"), "{e}");
}

/// Catches a write through a link on the guest volume: a guest could plant `Applications`
/// or the staging directory as a link to somewhere else, and the root copy would follow.
#[test]
fn a_link_where_the_volume_needs_a_directory_is_not_written_through() {
    let dir = tmp("stage-link");
    let app = fake_xcode(&dir);
    write(&dir.join("rt.dmg"), "hello", 0o644);
    let plan = plan(Some(&app), &[dir.join("rt.dmg")], false).expect("plan");
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");

    let volume = volume_in(&dir);
    link(elsewhere.to_str().expect("p"), &volume.join("Applications"));
    let e = Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect_err("link");
    assert!(e.contains("not a plain directory"), "{e}");
    assert!(std::fs::read_dir(&elsewhere).expect("dir").next().is_none());

    let volume2 = dir.join("volume2");
    std::fs::create_dir_all(volume2.join("private/var")).expect("v2");
    link(
        elsewhere.to_str().expect("p"),
        &volume2.join("private/var/kbf-sim"),
    );
    let plan_rt = crate::stage::plan(None, &[dir.join("rt.dmg")], false).expect("plan");
    let e = Harness::new(100 * GIB)
        .run(&volume2, &plan_rt)
        .expect_err("link");
    assert!(e.contains("not a plain directory"), "{e}");
    assert!(std::fs::read_dir(&elsewhere).expect("dir").next().is_none());

    // A staging directory planted as a link is removed as a link, not followed.
    let volume3 = dir.join("volume3");
    std::fs::create_dir_all(volume3.join("Applications")).expect("v3");
    link(
        elsewhere.to_str().expect("p"),
        &volume3.join("Applications/.Xcode.app.kbf-staging"),
    );
    let plan_x = crate::stage::plan(Some(&app), &[], false).expect("plan");
    Harness::new(100 * GIB)
        .run(&volume3, &plan_x)
        .expect("execute");
    assert!(std::fs::read_dir(&elsewhere).expect("dir").next().is_none());
}

/// Catches a runtime copy that is not what was hashed, records the wrong name, size or
/// digest, leaves the image group- or world-writable, keeps a runtime of an earlier setup,
/// or follows a link in the runtime directory when clearing it.
#[test]
fn runtimes_are_staged_hashed_and_replace_the_earlier_set() {
    let dir = tmp("stage-rt");
    write(&dir.join("iOS.dmg"), "hello", 0o600);
    let volume = volume_in(&dir);
    let outside = dir.join("outside");
    write(&outside, "keep me", 0o644);
    let rt_dir = volume.join("private/var/kbf-sim/runtimes");
    write(&rt_dir.join("old.dmg"), "old", 0o644);
    link(outside.to_str().expect("p"), &rt_dir.join("planted.dmg"));
    let plan = plan(None, &[dir.join("iOS.dmg")], false).expect("plan");
    let h = Harness::new(100 * GIB);
    let out = h.run(&volume, &plan).expect("execute");
    let r = &out["runtimes"][0];
    assert_eq!(
        (
            r["name"].as_str(),
            r["bytes"].as_u64(),
            r["sha256"].as_str(),
            r["dest"].as_str()
        ),
        (
            Some("iOS.dmg"),
            Some(5),
            Some(HELLO),
            Some("/private/var/kbf-sim/runtimes/iOS.dmg")
        )
    );
    assert_eq!(listing(&rt_dir), vec!["f iOS.dmg 644 hello".to_owned()]);
    assert_eq!(
        std::fs::read_to_string(&outside).expect("outside"),
        "keep me"
    );
    // Ownership is given where the runtime is built; the rename keeps it.
    assert!(
        h.chowned.borrow().contains(
            &rt_dir
                .with_file_name(".runtimes.kbf-staging")
                .join("iOS.dmg")
        )
    );
    assert!(out["xcode"].is_null());
    assert_eq!(out["manifest"], "/private/var/kbf-sim/staged.json");
}

/// Catches a source that changes between the plan and the copy being staged anyway, for a
/// runtime and for a file of Xcode.app, and a half-built Xcode.app being renamed into place.
#[test]
fn a_source_that_changed_since_the_plan_is_an_error_and_xcode_is_not_installed() {
    let dir = tmp("stage-changed");
    write(&dir.join("iOS.dmg"), "hello", 0o644);
    let volume = volume_in(&dir);
    let mut plan = plan(None, &[dir.join("iOS.dmg")], false).expect("plan");
    plan.runtimes[0].bytes = 6;
    let e = Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect_err("changed");
    assert!(e.contains("changed while it was copied"), "{e}");

    let dir = tmp("stage-changed-x");
    let app = fake_xcode(&dir);
    let volume = volume_in(&dir);
    let mut plan = crate::stage::plan(Some(&app), &[], false).expect("plan");
    for n in &mut plan.xcode.as_mut().expect("x").nodes {
        if let Node::File { bytes, rel, .. } = n
            && rel.ends_with("Info.plist")
        {
            *bytes += 1;
        }
    }
    let e = Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect_err("changed");
    assert!(e.contains("changed while it was copied"), "{e}");
    assert!(!volume.join("Applications/Xcode.app").exists());
}

/// Catches a manifest that is missing, differs from the report, is writable by others, or
/// names a path of the host.
#[test]
fn the_manifest_on_the_volume_records_what_was_staged() {
    let dir = tmp("stage-manifest");
    let app = fake_xcode(&dir);
    write(&dir.join("iOS.dmg"), "hello", 0o644);
    let volume = volume_in(&dir);
    let plan = plan(Some(&app), &[dir.join("iOS.dmg")], false).expect("plan");
    let out = Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect("execute");
    let path = volume.join("private/var/kbf-sim/staged.json");
    let text = std::fs::read_to_string(&path).expect("manifest");
    let m: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(m["runtimes"], out["runtimes"]);
    assert_eq!(m["xcode"], out["xcode"]);
    assert_eq!(m["xcode"]["dest"], "/Applications/Xcode.app");
    assert_eq!(m["xcode"]["files"], 3);
    assert_eq!(m["xcode"]["symlinks"], 3);
    assert!(!text.contains(dir.to_str().expect("p")), "{text}");
    assert_eq!(
        std::fs::metadata(&path).expect("m").permissions().mode() & 0o777,
        0o644
    );
    // The report adds the space it saw.
    assert_eq!(out["free_bytes_before"], 100 * GIB);
    assert_eq!(out["needed_bytes"], plan.bytes());
}

/// Catches a free-space reading that is zero or an error on a real directory, and no error
/// for a path that does not exist.
#[test]
fn the_free_space_of_a_real_directory_is_read() {
    let dir = tmp("stage-free");
    assert!(crate::hostfs::free_bytes(&dir).expect("statvfs") > 0);
    assert!(crate::hostfs::free_bytes(&dir.join("none")).is_err());
    // A path with a NUL cannot be passed to the kernel.
    let nul = Path::new(std::ffi::OsStr::from_bytes(b"a\0b"));
    assert!(crate::hostfs::free_bytes(nul).is_err());
}

/// Catches an ownership change that does nothing, one that follows a link, and one that
/// reports success when the kernel refused (as a user it is refused: only root may give a
/// file to root).
#[test]
fn a_file_is_given_to_root_only_when_the_kernel_allows_it() {
    let dir = tmp("stage-lchown");
    write(&dir.join("f"), "x", 0o644);
    let r = crate::hostfs::lchown_root(&dir.join("f"));
    assert_eq!(r.is_ok(), !not_root(), "{r:?}");
    // A link is changed itself: a dangling link is no error, and its target is not looked at.
    link("/nonexistent/target", &dir.join("dangling"));
    let r = crate::hostfs::lchown_root(&dir.join("dangling"));
    assert_eq!(r.is_ok(), !not_root(), "{r:?}");
    assert!(crate::hostfs::lchown_root(&dir.join("absent")).is_err());
    let nul = Path::new(std::ffi::OsStr::from_bytes(b"a\0b"));
    assert!(crate::hostfs::lchown_root(nul).is_err());
}

mod setup_flow {
    use super::*;
    use crate::fake::{FakeSystem, work_with_base};
    use crate::image::{SetupArgs, setup};
    use crate::report::Status;

    fn root() -> FakeSystem {
        let sys = FakeSystem::new();
        sys.uid.set(0);
        sys
    }

    /// Catches a staging run that is not root, writes into a directory that is not a
    /// bundle, or reaches the disk without a single thing to do.
    #[test]
    fn staging_needs_root_a_bundle_and_something_to_stage() {
        let work = work_with_base("stage-setup-pre");
        let base = work.join("base");
        let app = fake_xcode(&work);
        let args = SetupArgs {
            xcode: Some(app),
            ..SetupArgs::default()
        };
        let user = FakeSystem::new();
        assert_eq!(setup(&user, &base, &args).status, Status::NeedsManual);
        assert!(user.stage_calls.borrow().is_empty());
        let sys = root();
        assert_eq!(setup(&sys, &work.join("none"), &args).status, Status::Error);
        let r = setup(&sys, &base, &SetupArgs::default());
        assert_eq!(r.status, Status::NeedsManual);
        assert!(sys.stage_calls.borrow().is_empty() && sys.offline_calls.borrow().is_empty());
    }

    /// Catches `--xcode` and `--sim-runtime` not reaching the volume writer, the staging
    /// going to another disk, a plan that differs from what was named, and a request that
    /// also writes the daemons not doing both in order (daemons, then the staging).
    #[test]
    fn xcode_and_runtimes_are_staged_onto_the_bases_disk() {
        let work = work_with_base("stage-setup");
        let base = work.join("base");
        let app = fake_xcode(&work);
        write(&work.join("iOS.dmg"), "hello", 0o644);
        let sys = root();
        let args = SetupArgs {
            xcode: Some(app.clone()),
            sim_runtimes: vec![work.join("iOS.dmg")],
            ..SetupArgs::default()
        };
        let r = setup(&sys, &base, &args);
        assert_eq!(r.status, Status::Ok, "{r:?}");
        // Staging alone needs no guest binary: the only file written to the disk is the
        // simulator daemon's plist.
        assert_eq!(
            sys.offline_calls.borrow().clone(),
            vec![(
                base.join("disk.img"),
                vec!["Library/LaunchDaemons/com.kbf.sim.plist".to_owned()]
            )]
        );
        let calls = sys.stage_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, base.join("disk.img"));
        assert_eq!(calls[0].1.xcode.as_ref().expect("x").source, app);
        assert_eq!(calls[0].1.runtimes[0].name, "iOS.dmg");
        // The write that put the simulator daemon in is reported next to the staging.
        assert!(!r.details["offline_setup"].is_null());
        assert_eq!(
            r.details["staged"],
            serde_json::json!({"xcode": null, "runtimes": null})
        );
        drop(calls);

        let bin = work.join("guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        let both = SetupArgs {
            guest_binary: Some(bin),
            ..args
        };
        let r = setup(&sys, &base, &both);
        assert_eq!(r.status, Status::Ok, "{r:?}");
        assert_eq!(
            (
                sys.offline_calls.borrow().len(),
                sys.stage_calls.borrow().len()
            ),
            (2, 2)
        );
        // One write holds the probe daemons and the simulator daemon together.
        let written = sys.offline_calls.borrow()[1].1.clone();
        assert!(written.contains(&"usr/local/libexec/kbf-guest".to_owned()));
        assert!(written.contains(&"Library/LaunchDaemons/com.kbf.sim.plist".to_owned()));
        assert!(!r.details["offline_setup"].is_null());
    }

    /// Catches a refusal that comes after the daemons were already written to the disk, and
    /// the refusals that do not reach the report: an escaping link, a link as the app, a
    /// bad runtime name.
    #[test]
    fn a_refused_input_writes_nothing_to_the_disk() {
        let work = work_with_base("stage-setup-refuse");
        let base = work.join("base");
        let app = fake_xcode(&work);
        link("/etc", &app.join("Contents/etc"));
        let bin = work.join("guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        write(&work.join("bad name.dmg"), "x", 0o644);
        let sys = root();
        let cases = [
            (
                SetupArgs {
                    xcode: Some(app.clone()),
                    guest_binary: Some(bin.clone()),
                    ..SetupArgs::default()
                },
                "leaves the app",
            ),
            (
                SetupArgs {
                    xcode: Some(work.join("none.app")),
                    guest_binary: Some(bin.clone()),
                    ..SetupArgs::default()
                },
                "stat",
            ),
            (
                SetupArgs {
                    sim_runtimes: vec![work.join("bad name.dmg")],
                    guest_binary: Some(bin),
                    ..SetupArgs::default()
                },
                "<name>.dmg",
            ),
        ];
        for (args, want) in cases {
            let r = setup(&sys, &base, &args);
            assert_eq!(r.status, Status::Error);
            assert!(r.reason.expect("reason").contains(want));
        }
        assert!(sys.offline_calls.borrow().is_empty());
        assert!(sys.stage_calls.borrow().is_empty());
        // With the flag the same app is accepted.
        let allowed = SetupArgs {
            xcode: Some(app),
            allow_external_links: true,
            ..SetupArgs::default()
        };
        assert_eq!(setup(&sys, &base, &allowed).status, Status::Ok);
        let calls = sys.stage_calls.borrow();
        assert_eq!(
            calls[0].1.xcode.as_ref().expect("x").external_links.len(),
            1
        );
    }

    /// Catches a volume that is too small or an attach that fails being reported as
    /// success.
    #[test]
    fn a_staging_failure_is_an_error_naming_the_cause() {
        let work = work_with_base("stage-setup-fail");
        let base = work.join("base");
        let app = fake_xcode(&work);
        let sys = root();
        *sys.stage.borrow_mut() = Err("not enough room on the guest's Data volume".into());
        let args = SetupArgs {
            xcode: Some(app),
            ..SetupArgs::default()
        };
        let r = setup(&sys, &base, &args);
        assert_eq!(r.status, Status::Error);
        let reason = r.reason.expect("reason");
        assert!(
            reason.contains("staging:") && reason.contains("not enough room"),
            "{reason}"
        );
    }
}

fn fake_app(dir: &Path, name: &str) -> PathBuf {
    let app = dir.join(name);
    write(&app.join("Info.plist"), "<plist/>", 0o644);
    write(&app.join("Demo"), "binary", 0o755);
    write(&app.join("Frameworks/F.framework/F"), "framework", 0o644);
    link("F", &app.join("Frameworks/F.framework/Current"));
    app
}

/// Catches an app bundle taken without the checks of Xcode.app: a link as the bundle, a
/// directory with no Info.plist, a name the guest path or a frame name cannot carry, and
/// an absolute link that names a path of another tree.
#[test]
fn an_app_bundle_is_a_real_app_directory_with_a_safe_name_and_contained_links() {
    let dir = tmp("stage-app-plan");
    let app = fake_app(&dir, "Demo.app");
    let p = plan_app(&app, false).expect("plan");
    assert_eq!(p.name, "Demo.app");
    assert_eq!(
        p.tree.bytes,
        ("<plist/>".len() + "binary".len() + "framework".len()) as u64
    );
    assert!(
        p.tree
            .nodes
            .iter()
            .any(|n| matches!(n, Node::Link { rel, target } if rel.ends_with("Current") && target == Path::new("F")))
    );

    link("Demo.app", &dir.join("Alias.app"));
    assert_eq!(
        plan_app(&dir.join("Alias.app"), false),
        Err(StageError::Symlink(dir.join("Alias.app")))
    );
    std::fs::create_dir(dir.join("Bare.app")).expect("bare");
    assert_eq!(
        plan_app(&dir.join("Bare.app"), false),
        Err(StageError::NotApp(dir.join("Bare.app")))
    );
    for bad in ["no suffix", "a b.app", "x..app", ".hidden.app"] {
        let d = dir.join(bad);
        write(&d.join("Info.plist"), "x", 0o644);
        assert_eq!(
            plan_app(&d, false),
            Err(StageError::BadAppName(d.clone())),
            "{bad}"
        );
    }

    // Absolute links are judged against the app's own place in the guest.
    link("/private/var/kbf-sim/apps/Demo.app/Demo", &app.join("self"));
    assert!(plan_app(&app, false).is_ok());
    link("/Applications/Xcode.app/Contents", &app.join("other"));
    let e = plan_app(&app, false).expect_err("refused").to_string();
    assert!(
        e.contains("leaves the app") && e.contains("--allow-external-links"),
        "{e}"
    );
    assert_eq!(
        plan_app(&app, true)
            .expect("allowed")
            .tree
            .external_links
            .len(),
        1
    );
}

/// Catches an app that lands anywhere but under the apps directory, a directory that keeps
/// an earlier app (the guest installs the one it finds), Xcode.app touched by an
/// app-only setup, and an app that is not owned by root.
#[test]
fn an_app_is_staged_alone_in_its_directory_and_replaces_the_earlier_one() {
    let dir = tmp("stage-app");
    let app = fake_app(&dir, "Demo.app");
    let volume = volume_in(&dir);
    write(
        &volume.join("private/var/kbf-sim/apps/Old.app/Info.plist"),
        "old",
        0o644,
    );
    write(&volume.join("Applications/Xcode.app/keep"), "keep", 0o644);
    let plan = plan_with_app(None, &[], Some(&app), false).expect("plan");
    let h = Harness::new(100 * GIB);
    let out = h.run(&volume, &plan).expect("execute");
    let apps = volume.join("private/var/kbf-sim/apps");
    assert_eq!(listing(&apps.join("Demo.app")), listing(&app));
    let top: Vec<_> = std::fs::read_dir(&apps)
        .expect("dir")
        .map(|e| e.expect("e").file_name())
        .collect();
    assert_eq!(top, ["Demo.app"]);
    assert_eq!(
        std::fs::read_to_string(volume.join("Applications/Xcode.app/keep")).expect("keep"),
        "keep"
    );
    assert_eq!(
        (out["app"]["name"].as_str(), out["app"]["dest"].as_str()),
        (Some("Demo.app"), Some("/private/var/kbf-sim/apps/Demo.app"))
    );
    assert_eq!(out["xcode"], Value::Null);
    assert!(
        h.chowned
            .borrow()
            .contains(&apps.join(".Demo.app.kbf-staging/Demo"))
    );
    let m: Value = serde_json::from_str(
        &std::fs::read_to_string(volume.join("private/var/kbf-sim/staged.json")).expect("manifest"),
    )
    .expect("json");
    assert_eq!(m["app"], out["app"]);
    // A rerun replaces it.
    write(&app.join("Extra"), "more", 0o644);
    let plan = plan_with_app(None, &[], Some(&app), false).expect("plan");
    Harness::new(100 * GIB).run(&volume, &plan).expect("rerun");
    assert_eq!(listing(&apps.join("Demo.app")), listing(&app));
}

/// Catches a space check that forgets the app.
#[test]
fn the_app_counts_toward_the_space_needed() {
    let dir = tmp("stage-app-space");
    let app = fake_app(&dir, "Demo.app");
    let plan = plan_with_app(None, &[], Some(&app), false).expect("plan");
    assert_eq!(plan.bytes(), plan.app.as_ref().expect("app").tree.bytes);
    assert!(plan.bytes() > 0 && !plan.is_empty());
    let volume = volume_in(&dir);
    let e = Harness::new(plan.bytes() + HEADROOM_BYTES - 1)
        .run(&volume, &plan)
        .expect_err("tight");
    assert!(e.contains("not enough room"), "{e}");
}

mod setup_app_flow {
    use super::*;
    use crate::fake::{FakeSystem, work_with_base};
    use crate::image::{SetupArgs, setup};
    use crate::report::Status;

    /// Catches `--sim-app` not reaching the volume writer or the daemon, and a refused app
    /// reaching the disk.
    #[test]
    fn the_app_is_staged_with_the_daemon_and_a_refused_app_writes_nothing() {
        let work = work_with_base("stage-setup-app");
        let base = work.join("base");
        let app = fake_app(&work, "Demo.app");
        let sys = FakeSystem::new();
        sys.uid.set(0);
        let args = SetupArgs {
            sim_app: Some(app.clone()),
            ..SetupArgs::default()
        };
        let r = setup(&sys, &base, &args);
        assert_eq!(r.status, Status::Ok, "{r:?}");
        assert_eq!(
            sys.offline_calls.borrow()[0].1,
            ["Library/LaunchDaemons/com.kbf.sim.plist"]
        );
        let calls = sys.stage_calls.borrow();
        assert_eq!(calls[0].1.app.as_ref().expect("app").name, "Demo.app");
        assert!(calls[0].1.xcode.is_none() && calls[0].1.runtimes.is_empty());
        drop(calls);

        link("/etc", &app.join("etc"));
        let sys = FakeSystem::new();
        sys.uid.set(0);
        let r = setup(&sys, &base, &args);
        assert_eq!(r.status, Status::Error);
        assert!(r.reason.expect("reason").contains("leaves the app"));
        assert!(sys.offline_calls.borrow().is_empty() && sys.stage_calls.borrow().is_empty());
    }

    /// Catches a setup that writes only the probe daemons and still installs the simulator
    /// daemon (every other probe would then start the simulator run).
    #[test]
    fn a_setup_with_nothing_staged_writes_no_simulator_daemon() {
        let work = work_with_base("stage-setup-nosim");
        let base = work.join("base");
        let bin = work.join("guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        let sys = FakeSystem::new();
        sys.uid.set(0);
        let args = SetupArgs {
            guest_binary: Some(bin),
            ..SetupArgs::default()
        };
        assert_eq!(setup(&sys, &base, &args).status, Status::Ok);
        let written = sys.offline_calls.borrow()[0].1.clone();
        assert!(
            written.iter().all(|p| !p.contains("com.kbf.sim")),
            "{written:?}"
        );
        assert!(sys.stage_calls.borrow().is_empty());
    }
}

fn not_root() -> bool {
    // SAFETY: geteuid takes no arguments and cannot fail.
    unsafe { libc::geteuid() != 0 }
}

/// Catches a refusal that names the wrong kind of file, or a mode that falls through to
/// the wrong name: every file type the kernel has is told apart.
#[test]
#[allow(clippy::useless_conversion)] // mode_t is 32 bits on Linux and 16 on macOS
fn the_kind_of_a_refused_file_is_named_from_its_mode() {
    let m = |t: libc::mode_t| u32::from(t) | 0o644;
    assert_eq!(kind_of(m(libc::S_IFIFO)), "named pipe");
    assert_eq!(kind_of(m(libc::S_IFSOCK)), "socket");
    assert_eq!(kind_of(m(libc::S_IFCHR)), "device");
    assert_eq!(kind_of(m(libc::S_IFBLK)), "device");
    assert_eq!(kind_of(m(libc::S_IFREG)), "special file");
    assert_eq!(kind_of(m(libc::S_IFDIR)), "special file");
}

/// Catches a walk that stops at the first unreadable directory without saying which, and
/// one that cannot be bounded: the entry and depth limits are real limits.
#[test]
fn an_unreadable_directory_and_the_walk_limits_are_errors_naming_the_cause() {
    let dir = tmp("stage-walk");
    let app = fake_xcode(&dir);
    let tight = Limits {
        max_entries: 3,
        max_depth: MAX_DEPTH,
    };
    let e = plan_xcode_within(&app, false, &tight).expect_err("too many");
    assert_eq!(e, StageError::TooMany(app.clone(), 3));
    assert!(e.to_string().contains("more than 3 entries"));
    let shallow = Limits {
        max_entries: MAX_ENTRIES,
        max_depth: 2,
    };
    assert!(matches!(
        plan_xcode_within(&app, false, &shallow),
        Err(StageError::TooDeep(_, 2))
    ));
    // Exactly the entries the tree has fit.
    let all = plan_xcode(&app, false).expect("plan").nodes.len();
    let exact = Limits {
        max_entries: all,
        max_depth: MAX_DEPTH,
    };
    assert!(plan_xcode_within(&app, false, &exact).is_ok());

    if not_root() {
        let locked = app.join("Contents/Frameworks");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let e = err_of(plan_xcode(&app, false));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(e.contains("read") && e.contains("Frameworks"), "{e}");
    }
}

/// Catches a tool that leaves a staged tree half done without saying so, or one that
/// cannot be told a host call failed: each fallible host call turns into an error naming
/// what failed, so each is tried.
#[test]
fn the_host_calls_report_what_failed() {
    let dir = tmp("stage-helpers");
    let missing = dir.join("none/deeper");
    let env_failing = |_: &Path| -> std::io::Result<()> { Err(std::io::Error::other("no")) };
    let ok_free = |_: &Path| Ok(0u64);
    let no_clone = |_: &Path, _: &Path| -> std::io::Result<()> { Err(std::io::Error::other("no")) };
    let env = Env {
        free_bytes: &ok_free,
        clone_tree: &no_clone,
        chown_root: &env_failing,
        remove: &remove_path,
    };
    assert!(chmod(&missing, 0o755).unwrap_err().starts_with("chmod "));
    assert!(chown(&env, &missing).unwrap_err().starts_with("chown "));
    write(&dir.join("exists"), "x", 0o644);
    assert!(
        mkdir(&dir.join("exists"), 0o755)
            .unwrap_err()
            .starts_with("create ")
    );
    assert!(
        write_text(&env, &missing, "t", 0o644)
            .unwrap_err()
            .starts_with("write ")
    );
    assert!(
        rename(&missing, &dir.join("b"))
            .unwrap_err()
            .starts_with("rename ")
    );
    assert!(
        apply_modes(vec![(missing.clone(), 0o755)])
            .unwrap_err()
            .starts_with("chmod ")
    );
    assert!(
        copy_file(&missing, &dir.join("c"), 0o644, 0)
            .unwrap_err()
            .starts_with("copy ")
    );
    // The source changed after the plan: more bytes than planned.
    let e = copy_file(&dir.join("exists"), &dir.join("d"), 0o644, 7).unwrap_err();
    assert!(
        e.contains("changed while it was copied (7 bytes planned, 1 copied)"),
        "{e}"
    );
    write_text(&env, &dir.join("t"), "text", 0o600).expect("write");
    assert_eq!(std::fs::read_to_string(dir.join("t")).expect("t"), "text");
    stage_link(Path::new("target"), &dir.join("l")).expect("link");
    assert!(
        stage_link(Path::new("target"), &dir.join("l"))
            .unwrap_err()
            .starts_with("link ")
    );
    // The parent of the new directory is a file, a link or missing.
    std::fs::create_dir(dir.join("vol")).expect("vol");
    write(&dir.join("vol/file"), "x", 0o644);
    let e = ensure_dir(&dir.join("vol"), Path::new("file/inner"), &env).unwrap_err();
    assert!(e.contains("not a plain directory"), "{e}");
    // A directory that has to be made is handed to root: a refusal to do that is the error.
    let e = ensure_dir(&dir.join("vol"), Path::new("new"), &env).unwrap_err();
    assert!(e.starts_with("chown "), "{e}");
}

/// Catches a stat error on the way to a directory being taken for "not there" (and the
/// directory created through it), and a failure to create reported as anything but that.
#[test]
fn a_directory_that_cannot_be_examined_or_created_is_an_error() {
    if !not_root() {
        return;
    }
    let dir = tmp("stage-ensure");
    let volume = dir.join("volume");
    std::fs::create_dir_all(volume.join("private/var")).expect("v");
    let free = |_: &Path| Ok(0u64);
    let clone = |_: &Path, _: &Path| -> std::io::Result<()> { Ok(()) };
    let chown = |_: &Path| -> std::io::Result<()> { Ok(()) };
    let env = Env {
        free_bytes: &free,
        clone_tree: &clone,
        chown_root: &chown,
        remove: &remove_path,
    };
    let lock = |p: &Path, mode| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).expect("chmod")
    };
    lock(&volume.join("private"), 0o000);
    let e = ensure_dir(&volume, Path::new("private/var/x"), &env);
    lock(&volume.join("private"), 0o755);
    assert!(e.unwrap_err().starts_with("stat "));
    lock(&volume, 0o555);
    let e = ensure_dir(&volume, Path::new("new"), &env);
    lock(&volume, 0o755);
    assert!(e.unwrap_err().starts_with("create "));
}

/// Catches a stage that carries on after a failure to hand a file to root (every file of
/// the guest's Xcode would then be owned by the host user): it fails at the first ownership
/// change that fails, whichever one it is, and says which path.
#[test]
fn a_failed_ownership_change_stops_the_stage_wherever_it_happens() {
    let dir = tmp("stage-chown");
    let app = fake_xcode(&dir);
    write(&dir.join("rt.dmg"), "hello", 0o644);
    let plan = plan(Some(&app), &[dir.join("rt.dmg")], false).expect("plan");
    let h = Harness::new(100 * GIB);
    h.run(&volume_in(&dir), &plan).expect("a clean run");
    let calls = h.chowned.borrow().len();
    assert!(calls > 10, "{calls}");
    for n in 0..calls {
        let work = tmp(&format!("stage-chown-{n}"));
        let mut h = Harness::new(100 * GIB);
        h.chown_fails_at = Some(n);
        let e = h.run(&volume_in(&work), &plan).expect_err("fails");
        assert!(
            e.starts_with("chown ") && e.contains("not permitted"),
            "{n}: {e}"
        );
    }
}

/// Catches a free-space reading that fails being taken for plenty of room.
#[test]
fn a_failure_to_read_the_free_space_is_an_error_and_nothing_is_written() {
    let dir = tmp("stage-free-err");
    let app = fake_xcode(&dir);
    let plan = plan(Some(&app), &[], false).expect("plan");
    let volume = volume_in(&dir);
    let mut h = Harness::new(100 * GIB);
    h.free_fails = true;
    let e = h.run(&volume, &plan).expect_err("fails");
    assert!(
        e.starts_with("read the free space of ") && e.contains("statvfs failed"),
        "{e}"
    );
    assert!(!volume.join("Applications").exists());
}

/// Catches a copy that skips the planned file, link or directory that is no longer there
/// to be copied: a source that vanished, a link or directory that already exists, and an
/// unreadable runtime directory are errors naming the path.
#[test]
fn the_copy_stops_on_a_source_that_vanished_or_a_name_that_is_taken() {
    let dir = tmp("stage-copy-err");
    let app = fake_xcode(&dir);
    let good = plan(Some(&app), &[], false).expect("plan");

    // A file removed between the plan and the copy.
    let mut gone = good.clone();
    std::fs::remove_file(app.join("Contents/Info.plist")).expect("rm");
    let e = Harness::new(100 * GIB)
        .run(&volume_in(&dir), &gone)
        .expect_err("gone");
    assert!(e.starts_with("copy ") && e.contains("Info.plist"), "{e}");
    write(&app.join("Contents/Info.plist"), "plist", 0o644);

    // The same link twice: the second cannot be made.
    let tree = gone.xcode.as_mut().expect("x");
    let first_link = tree
        .nodes
        .iter()
        .find(|n| matches!(n, Node::Link { .. }))
        .cloned()
        .expect("link");
    tree.nodes.push(first_link);
    let work = tmp("stage-copy-err2");
    let e = Harness::new(100 * GIB)
        .run(&volume_in(&work), &gone)
        .expect_err("twice");
    assert!(e.starts_with("link "), "{e}");

    // The same directory twice.
    let mut twice = good.clone();
    let tree = twice.xcode.as_mut().expect("x");
    let first_dir = tree
        .nodes
        .iter()
        .find(|n| matches!(n, Node::Dir { .. }))
        .cloned()
        .expect("dir");
    tree.nodes.push(first_dir);
    let work = tmp("stage-copy-err3");
    let e = Harness::new(100 * GIB)
        .run(&volume_in(&work), &twice)
        .expect_err("twice");
    assert!(e.starts_with("create "), "{e}");
}

/// Catches a runtime directory that cannot be listed passing as empty (the stale runtimes
/// in it would be installed by the guest).
#[test]
fn a_runtime_directory_that_cannot_be_listed_is_an_error() {
    if !not_root() {
        return;
    }
    let dir = tmp("stage-rt-unreadable");
    write(&dir.join("iOS.dmg"), "hello", 0o644);
    let volume = volume_in(&dir);
    let rt = volume.join("private/var/kbf-sim/runtimes");
    std::fs::create_dir_all(&rt).expect("rt");
    std::fs::set_permissions(&rt, std::fs::Permissions::from_mode(0o300)).expect("chmod");
    let plan = plan(None, &[dir.join("iOS.dmg")], false).expect("plan");
    let e = Harness::new(100 * GIB).run(&volume, &plan);
    std::fs::set_permissions(&rt, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(e.unwrap_err().starts_with("read "));
}

/// Catches a rerun that deletes the runtime it is about to stage again, or a runtime
/// directory that keeps a stale one: the second setup with the same runtime leaves it in
/// place, byte for byte.
#[test]
fn a_runtime_that_is_staged_again_is_replaced_in_place() {
    let dir = tmp("stage-rt-again");
    write(&dir.join("iOS.dmg"), "hello", 0o644);
    let volume = volume_in(&dir);
    let plan = plan(None, &[dir.join("iOS.dmg")], false).expect("plan");
    Harness::new(100 * GIB).run(&volume, &plan).expect("first");
    let out = Harness::new(100 * GIB).run(&volume, &plan).expect("second");
    assert_eq!(out["runtimes"][0]["sha256"], HELLO);
    assert_eq!(
        std::fs::read(volume.join("private/var/kbf-sim/runtimes/iOS.dmg")).expect("dmg"),
        b"hello"
    );
}

/// Catches links that leave the app being copied without a trace in the report: the
/// links the flag let through are listed in what was staged, with their targets.
#[test]
fn the_links_let_through_by_the_flag_are_listed_in_the_staging_report() {
    let dir = tmp("stage-ext-report");
    let app = fake_xcode(&dir);
    link("/usr/share/man", &app.join("Contents/Developer/man"));
    let plan = plan(Some(&app), &[], true).expect("plan");
    let out = Harness::new(100 * GIB)
        .run(&volume_in(&dir), &plan)
        .expect("execute");
    assert_eq!(
        out["xcode"]["external_links"],
        serde_json::json!([{"path": "Contents/Developer/man", "target": "/usr/share/man"}])
    );
}

/// Catches an emptiness test that forgets one of the three things a setup stages (a setup
/// of only runtimes, or only an app, would then write the daemon and stage nothing).
#[test]
fn a_plan_is_empty_only_when_nothing_is_staged() {
    let dir = tmp("stage-empty");
    let app = fake_app(&dir, "Demo.app");
    write(&dir.join("rt.dmg"), "x", 0o644);
    assert!(Plan::default().is_empty());
    assert!(
        !plan(Some(&fake_xcode(&dir)), &[], false)
            .expect("xcode")
            .is_empty()
    );
    assert!(
        !plan(None, &[dir.join("rt.dmg")], false)
            .expect("rt")
            .is_empty()
    );
    assert!(
        !plan_with_app(None, &[], Some(&app), false)
            .expect("app")
            .is_empty()
    );
}

/// Catches an app path that is a plain file or is missing being taken for a bundle, or
/// reported as something else.
#[test]
fn an_app_path_that_is_a_file_or_missing_is_refused_with_the_cause() {
    let dir = tmp("stage-app-bad");
    write(&dir.join("File.app"), "x", 0o644);
    assert_eq!(
        plan_app(&dir.join("File.app"), false),
        Err(StageError::NotApp(dir.join("File.app")))
    );
    let e = plan_app(&dir.join("Missing.app"), false)
        .expect_err("missing")
        .to_string();
    assert!(e.starts_with("stat "), "{e}");
}

/// Catches an apps directory that cannot be listed passing as empty (a stale app in it
/// would be installed instead).
#[test]
fn an_apps_directory_that_cannot_be_listed_is_an_error() {
    if !not_root() {
        return;
    }
    let dir = tmp("stage-apps-unreadable");
    let app = fake_app(&dir, "Demo.app");
    let volume = volume_in(&dir);
    let apps = volume.join("private/var/kbf-sim/apps");
    std::fs::create_dir_all(&apps).expect("apps");
    std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o300)).expect("chmod");
    let plan = plan_with_app(None, &[], Some(&app), false).expect("plan");
    let e = Harness::new(100 * GIB).run(&volume, &plan);
    std::fs::set_permissions(&apps, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(e.unwrap_err().starts_with("read "));
}

/// Catches a tree clone that keeps the setuid, setgid or sticky bit of the host tree (the
/// plan's modes are written on the file-by-file path only).
#[test]
fn a_cloned_tree_has_no_setuid_setgid_or_sticky_bit() {
    let dir = tmp("stage-clone-modes");
    let app = fake_xcode(&dir);
    let tool = app.join("Contents/Developer/usr/bin/xcodebuild");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o6755)).expect("chmod");
    let sticky = app.join("Contents/Frameworks");
    std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).expect("chmod");
    let plan = plan(Some(&app), &[], false).expect("plan");
    let volume = volume_in(&dir);
    let mut h = Harness::new(100 * GIB);
    h.clone_ok = true;
    let out = h.run(&volume, &plan).expect("execute");
    assert_eq!(out["xcode"]["method"], "clone");
    let mode = |p: &str| {
        std::fs::metadata(volume.join("Applications/Xcode.app").join(p))
            .expect("meta")
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(
        (
            mode("Contents/Developer/usr/bin/xcodebuild"),
            mode("Contents/Frameworks")
        ),
        (0o755, 0o755)
    );
}

/// A change made to a cloned tree.
type Mutation = fn(&Path);

/// Catches a clone of a tree that is not the planned one being installed: a link out of the
/// app, a new file, a new pipe, a file replaced by a link, or a changed size. Nothing is
/// put under the real name.
#[test]
fn a_cloned_tree_that_differs_from_the_plan_is_refused() {
    let cases: [(&str, Mutation); 5] = [
        ("escaping link", |t| {
            link("/etc", &t.join("Contents/escape"));
        }),
        ("new file", |t| write(&t.join("Contents/new"), "new", 0o644)),
        ("new pipe", |t| {
            let path = std::ffi::CString::new(t.join("Contents/pipe").as_os_str().as_bytes())
                .expect("path");
            // SAFETY: `path` is a NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);
        }),
        ("file became a link", |t| {
            let f = t.join("Contents/Info.plist");
            std::fs::remove_file(&f).expect("rm");
            link("/etc/hostname", &f);
        }),
        ("size changed", |t| {
            write(&t.join("Contents/Info.plist"), "plist plus", 0o644);
        }),
    ];
    for (name, after) in cases {
        let dir = tmp("stage-clone-diff");
        let app = fake_xcode(&dir);
        let volume = volume_in(&dir);
        let plan = plan(Some(&app), &[], false).expect("plan");
        let mut h = Harness::new(100 * GIB);
        h.clone_ok = true;
        h.after_clone = Some(after);
        let e = h.run(&volume, &plan).expect_err(name);
        assert!(
            e.contains("cloned tree does not match the plan"),
            "{name}: {e}"
        );
        assert!(!volume.join("Applications/Xcode.app").exists(), "{name}");
    }
}

/// Catches a link planted in the clone that the plan accepted being treated as a reason to
/// refuse (an allowed external link is in the plan and in the clone alike).
#[test]
fn a_clone_with_the_links_the_plan_accepted_is_installed() {
    let dir = tmp("stage-clone-ext");
    let app = fake_xcode(&dir);
    link("/usr/lib/outside", &app.join("Contents/outside"));
    let plan = plan(Some(&app), &[], true).expect("plan");
    let volume = volume_in(&dir);
    let mut h = Harness::new(100 * GIB);
    h.clone_ok = true;
    let out = h.run(&volume, &plan).expect("execute");
    assert_eq!(out["xcode"]["method"], "clone");
    assert_eq!(
        std::fs::read_link(volume.join("Applications/Xcode.app/Contents/outside")).expect("l"),
        Path::new("/usr/lib/outside")
    );
}

/// Catches a file swapped for a link after the plan being read through the link as root: the
/// link points at a file of the same size, so only the refusal to follow it stops the copy.
#[test]
fn a_file_swapped_for_a_link_after_the_plan_is_not_copied() {
    let dir = tmp("stage-swap");
    let app = fake_xcode(&dir);
    let secret = dir.join("secret");
    write(&secret, "plist", 0o600);
    let plan = plan(Some(&app), &[], false).expect("plan");
    let victim = app.join("Contents/Info.plist");
    std::fs::remove_file(&victim).expect("rm");
    link(secret.to_str().expect("p"), &victim);
    let volume = volume_in(&dir);
    let e = Harness::new(100 * GIB)
        .run(&volume, &plan)
        .expect_err("swapped");
    assert!(e.starts_with("copy ") && e.contains("Info.plist"), "{e}");
    assert!(!volume.join("Applications/Xcode.app").exists());
    let staged = volume.join("Applications/.Xcode.app.kbf-staging/Contents/Info.plist");
    assert!(
        std::fs::read_to_string(staged).map_or(true, |t| t != "plist"),
        "the target of the link was copied"
    );
}

/// Catches a file swapped for a directory after the plan being accepted by the open (a
/// directory opens for reading) and failing only inside the copy.
#[test]
fn a_file_swapped_for_a_directory_after_the_plan_is_refused_as_not_regular() {
    let dir = tmp("stage-swap-dir");
    let app = fake_xcode(&dir);
    let plan = plan(Some(&app), &[], false).expect("plan");
    let victim = app.join("Contents/Info.plist");
    std::fs::remove_file(&victim).expect("rm");
    std::fs::create_dir(&victim).expect("dir");
    let e = Harness::new(100 * GIB)
        .run(&volume_in(&dir), &plan)
        .expect_err("swapped");
    assert!(e.contains("no longer the regular file"), "{e}");
}

/// Catches a setup that names no runtime leaving the runtimes of an earlier setup in place
/// (the guest installs what it finds) and a manifest that still lists them.
#[test]
fn a_setup_without_runtimes_clears_the_earlier_ones() {
    let dir = tmp("stage-no-rt");
    write(&dir.join("iOS.dmg"), "hello", 0o644);
    let app = fake_xcode(&dir);
    let volume = volume_in(&dir);
    let h = Harness::new(100 * GIB);
    let first = plan(Some(&app), &[dir.join("iOS.dmg")], false).expect("plan");
    h.run(&volume, &first).expect("first");
    let rt_dir = volume.join("private/var/kbf-sim/runtimes");
    assert_eq!(listing(&rt_dir).len(), 1);
    let second = plan(Some(&app), &[], false).expect("plan");
    let out = h.run(&volume, &second).expect("second");
    assert!(listing(&rt_dir).is_empty(), "{:?}", listing(&rt_dir));
    assert_eq!(out["runtimes"], serde_json::json!([]));
    let manifest =
        std::fs::read_to_string(volume.join("private/var/kbf-sim/staged.json")).expect("manifest");
    let m: Value = serde_json::from_str(&manifest).expect("json");
    assert_eq!(m["runtimes"], serde_json::json!([]));
}
