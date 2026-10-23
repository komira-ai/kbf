//! The sweep, on a real filesystem. Tests run unprivileged, so every file they make is
//! the test's own: "the departing uid" is the test's uid to see what is removed, and
//! any other uid to see that nothing is.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::*;
use kbf_testkit::dir::TestDir;

/// A point of a sweep at which a test makes entries vanish (see [`vanishing`]).
pub(super) enum Point<'a> {
    /// A directory was listed; the walk goes on with the names it read.
    Listed,
    /// The entry of this name was examined; the walk goes on with what it found.
    Examined(&'a OsStr),
}

/// A test's hook: the point reached, and how many listings the sweep has made so far.
type Hook = Box<dyn FnMut(&Point<'_>, usize)>;

thread_local! {
    /// This thread's hook and its count of listings.
    static HOOK: RefCell<Option<(usize, Hook)>> = const { RefCell::new(None) };
}

/// Called by the sweep at each [`Point`] (see `super::listed` and `super::examine`).
pub(super) fn reached(point: Point<'_>) {
    // Taken out while it runs, so a hook that walks cannot re-enter itself.
    let Some((mut listings, mut hook)) = HOOK.with_borrow_mut(Option::take) else {
        return;
    };
    listings += usize::from(matches!(point, Point::Listed));
    hook(&point, listings);
    HOOK.with_borrow_mut(|slot| *slot = Some((listings, hook)));
}

/// Runs `run` with `hook` called at each point its sweeps reach.
fn vanishing<T>(hook: impl FnMut(&Point<'_>, usize) + 'static, run: impl FnOnce() -> T) -> T {
    HOOK.with_borrow_mut(|slot| *slot = Some((0, Box::new(hook))));
    let out = run();
    HOOK.with_borrow_mut(|slot| *slot = None);
    out
}

/// A hook that runs `act` right after the sweep's `n`th listing.
fn after_listing(n: usize, mut act: impl FnMut() + 'static) -> impl FnMut(&Point<'_>, usize) {
    move |point, listings| {
        if matches!(point, Point::Listed) && listings == n {
            act();
        }
    }
}

/// A hook that removes `path` right after the sweep examined an entry named `name`.
fn after_examining(name: &'static str, path: PathBuf) -> impl FnMut(&Point<'_>, usize) {
    move |point, _| {
        if matches!(point, Point::Examined(seen) if *seen == name) {
            remove(&path);
        }
    }
}

/// Removes `path`, a file or a whole directory, if it is there.
fn remove(path: &Path) {
    let _ = fs::remove_file(path).or_else(|_| fs::remove_dir_all(path));
}

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn other() -> u32 {
    me() + 1
}

fn root() -> bool {
    me() == 0
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-mac-session-sweep-{name}"),
    )
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn owned_plan(base: &Path) -> SweepPlan {
    SweepPlan {
        named: Vec::new(),
        owned: vec![base.to_path_buf()],
        protected: Vec::new(),
    }
}

/// A shared folder holding a file, a directory tree and an empty directory.
fn populate(base: &Path) {
    fs::write(base.join("file"), "x").unwrap();
    fs::create_dir_all(base.join("tree/a/b")).unwrap();
    fs::write(base.join("tree/a/b/deep"), "x").unwrap();
    fs::create_dir(base.join("empty")).unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches: a sweep that leaves the departing uid's files in a shared place (the
/// next user of the uid would own them), or one that removes the shared folder itself.
#[test]
fn everything_the_uid_owns_goes_and_the_folder_stays() {
    let _base_own = scratch("owned");
    let base = _base_own.join("Shared");
    fs::create_dir(&base).unwrap();
    populate(&base);
    let swept = sweep(&owned_plan(&base), "kbf-lease-1-1", me());
    assert_eq!(
        swept,
        Swept {
            removed: 3,
            errors: Vec::new()
        }
    );
    assert!(names(&base).is_empty());
    assert!(base.is_dir());
}

/// Catches: a sweep that removes what another uid owns (another lease's files, the
/// system's), or one that never descends into another owner's directory.
#[test]
fn nothing_of_another_uid_goes() {
    let _base_own = scratch("others");
    let base = _base_own.join("Shared");
    fs::create_dir(&base).unwrap();
    populate(&base);
    let swept = sweep(&owned_plan(&base), "kbf-lease-1-1", other());
    assert_eq!(swept, Swept::default());
    assert_eq!(names(&base), ["empty", "file", "tree"]);
    assert!(base.join("tree/a/b/deep").is_file());
}

/// Catches: following a symbolic link in the sweep (S10: "follow links in the
/// sweep"). Another lease user can plant a link in a shared folder to a place root
/// would then empty; the link must go and its target stay.
#[test]
fn a_planted_link_is_removed_and_its_target_kept() {
    let dir = scratch("link");
    let base = dir.join("Shared");
    let outside = dir.join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious"), "x").unwrap();
    symlink(&outside, base.join("to-outside")).unwrap();
    symlink(outside.join("precious"), base.join("to-file")).unwrap();
    fs::create_dir(base.join("sub")).unwrap();
    symlink(&outside, base.join("sub/nested-link")).unwrap();

    let swept = sweep(&owned_plan(&base), "kbf-lease-1-1", me());
    assert_eq!(
        swept,
        Swept {
            removed: 3,
            errors: Vec::new()
        }
    );
    assert!(names(&base).is_empty());
    assert_eq!(names(&outside), ["precious"]);

    // A plan directory that is itself a link is not entered either.
    symlink(&outside, dir.join("linked-base")).unwrap();
    let swept = sweep(&owned_plan(&dir.join("linked-base")), "u", me());
    assert_eq!(swept.removed, 0);
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    assert_eq!(names(&outside), ["precious"]);
}

/// Catches: a named entry (home folder, crontab, launchd's per-uid files) left behind,
/// a template not filled in, or a missing one taken for an error.
#[test]
fn named_entries_go_whole_and_absent_ones_are_fine() {
    let dir = scratch("named");
    fs::create_dir_all(dir.join("Users/kbf-lease-2-7/Library/Caches")).unwrap();
    fs::write(dir.join("Users/kbf-lease-2-7/Library/Caches/c"), "x").unwrap();
    fs::create_dir_all(dir.join("Users/kbf-lease-2-8")).unwrap();
    fs::create_dir(dir.join("tabs")).unwrap();
    fs::write(dir.join("tabs/kbf-lease-2-7"), "* * * * * true\n").unwrap();
    fs::create_dir(dir.join("launchd")).unwrap();
    fs::write(dir.join("launchd/disabled.4242.plist"), "x").unwrap();
    fs::write(dir.join("launchd/disabled.4243.plist"), "x").unwrap();
    let at = |rest: &str| format!("{}/{rest}", dir.display());
    let plan = SweepPlan {
        named: vec![
            at("Users/{user}"),
            at("tabs/{user}"),
            at("launchd/disabled.{uid}.plist"),
            at("launchd/loginitems.{uid}.plist"),
            at("no-such-parent/{user}"),
        ],
        owned: vec![dir.join("no-such-folder")],
        protected: Vec::new(),
    };
    let swept = sweep(&plan, "kbf-lease-2-7", 4242);
    assert_eq!(
        swept,
        Swept {
            removed: 3,
            errors: Vec::new()
        }
    );
    assert_eq!(names(&dir.join("Users")), ["kbf-lease-2-8"]);
    assert!(names(&dir.join("tabs")).is_empty());
    assert_eq!(names(&dir.join("launchd")), ["disabled.4243.plist"]);

    let root_path = SweepPlan {
        named: vec!["/".to_owned()],
        owned: Vec::new(),
        protected: Vec::new(),
    };
    let swept = sweep(&root_path, "u", 1);
    assert_eq!(swept.errors, ["/: not a path to an entry"]);
}

/// Catches: an error swallowed (a sweep reported complete with entries left), a walk that
/// stops at the first stuck entry, and another owner's unreadable directory failing this
/// uid's sweep (the shared places hold other leases' directories).
#[test]
fn places_that_cannot_be_read_are_errors_and_the_walk_goes_on() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("errors");
    let locked = dir.join("locked");
    fs::create_dir(&locked).unwrap();
    let base = dir.join("Shared");
    fs::create_dir(&base).unwrap();
    fs::create_dir(base.join("no-search")).unwrap();
    fs::write(base.join("no-search/f"), "x").unwrap();
    fs::create_dir(base.join("no-read")).unwrap();
    fs::write(base.join("file"), "x").unwrap();
    chmod(&base.join("no-search"), 0o400);
    chmod(&base.join("no-read"), 0o000);
    chmod(&locked, 0o000);
    fs::create_dir(dir.join("tabs")).unwrap();
    fs::write(dir.join("tabs/u"), "x").unwrap();
    chmod(&dir.join("tabs"), 0o600);

    let plan = SweepPlan {
        named: vec![format!("{}/tabs/{{user}}", dir.display())],
        owned: vec![locked.clone(), base.clone()],
        protected: Vec::new(),
    };
    // Another uid: the walk descends, and a directory it cannot search below the plan's
    // folders belongs to someone else (another lease may be flipping it): logged and
    // skipped, never this uid's failure. The plan's own places are still errors: a base
    // that is not in the plan's `protected` set and cannot be opened is one.
    let swept = sweep(&plan, "u", other());
    let mut errors = swept.errors.clone();
    errors.sort();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].contains("/locked"), "{errors:?}");
    assert!(errors[1].contains("/tabs/u"), "{errors:?}");
    assert_eq!(swept.removed, 0);

    // The departing uid: what it owns goes where it can, the rest is reported.
    let swept = sweep(&owned_plan(&base), "u", me());
    let mut errors = swept.errors;
    errors.sort();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].contains("Shared/no-read"), "{errors:?}");
    assert!(errors[1].contains("Shared/no-search"), "{errors:?}");
    assert_eq!(swept.removed, 1);
    assert!(!base.join("file").exists());

    for path in [
        &base.join("no-search"),
        &base.join("no-read"),
        &locked,
        &dir.join("tabs"),
    ] {
        chmod(path, 0o700);
    }
}

/// Catches a sweep that fails a clean because root is refused a plan base itself (on a
/// Mac, `sfanalytics` and `trustd/private` answer `EPERM` to root in the daemon's
/// context, so every clean would fail and every slot go dirty), and the opposite: a skip
/// that is too wide. A protected base that opens with `EACCES` is skipped with no error and the
/// next base is still swept; a base that is no directory (`ENOTDIR`), a link (`ELOOP`) and
/// an error below a base that could be listed are all still errors.
#[test]
fn a_base_root_is_denied_is_skipped_and_every_other_error_still_fails() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("denied-base");
    let denied_base = dir.join("denied");
    fs::create_dir(&denied_base).unwrap();
    chmod(&denied_base, 0o000);
    let swept_base = dir.join("swept");
    fs::create_dir(&swept_base).unwrap();
    fs::write(swept_base.join("mine"), "x").unwrap();
    fs::create_dir(swept_base.join("no-read")).unwrap();
    fs::write(swept_base.join("no-read/f"), "x").unwrap();
    chmod(&swept_base.join("no-read"), 0o000);
    let file = dir.join("file");
    fs::write(&file, "x").unwrap();
    let link = dir.join("link");
    symlink(&swept_base, &link).unwrap();

    let plan = SweepPlan {
        named: Vec::new(),
        owned: vec![denied_base.clone(), swept_base.clone()],
        protected: vec![denied_base.clone()],
    };
    let swept = sweep(&plan, "u", me());
    // Only the entry below the base that could be listed is an error.
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    assert!(
        swept.errors[0].contains("swept/no-read"),
        "{:?}",
        swept.errors
    );
    assert_eq!(swept.removed, 1);
    assert!(!swept_base.join("mine").exists());

    // Protected bases that are no directory, or a link, are errors as well.
    for bad in [&file, &link] {
        let mut plan = owned_plan(bad);
        plan.protected = vec![bad.clone()];
        let swept = sweep(&plan, "u", me());
        assert_eq!(swept.errors.len(), 1, "{bad:?}: {:?}", swept.errors);
        assert_eq!(swept.removed, 0);
    }
    // The same denied base, not in the protected set: an error, and the check that looks
    // at the places a sweep would (`denied_base`) agrees with the sweep on all of these.
    let mut plan = owned_plan(&denied_base);
    let swept = sweep(&plan, "u", me());
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    assert!(swept.errors[0].contains("/denied"), "{:?}", swept.errors);
    assert!(!plan.denied_base(&denied_base));
    plan.protected = vec![denied_base.clone()];
    assert!(sweep(&plan, "u", me()).errors.is_empty());
    assert!(plan.denied_base(&denied_base));
    assert!(!plan.denied_base(&swept_base), "listable and unprotected");
    plan.protected.push(swept_base.clone());
    assert!(!plan.denied_base(&swept_base), "protected but listable");
    plan.protected.push(dir.join("absent"));
    assert!(
        !plan.denied_base(&dir.join("absent")),
        "protected but absent"
    );
    plan.protected.push(file.clone());
    assert!(
        !plan.denied_base(&file),
        "protected, not a directory: not a denial"
    );
    chmod(&denied_base, 0o700);
    chmod(&swept_base.join("no-read"), 0o700);
}

/// Catches a denial read too widely or too narrowly: only `EPERM` and `EACCES` (the
/// two answers a protected folder gives root) are a skip. And the warning once per base:
/// the first skip of a base says so, a second skip of it does not, and another base does.
#[test]
fn only_eperm_and_eacces_are_a_denial_and_each_base_is_named_once() {
    let dir = scratch("first-denial");
    let plan = SweepPlan {
        protected: ["x", "y", "a", "b", "c", "d"].map(|n| dir.join(n)).to_vec(),
        ..SweepPlan::default()
    };
    for n in [libc::EPERM, libc::EACCES] {
        assert!(
            plan.skips(&dir.join("x"), &io::Error::from_raw_os_error(n)),
            "{n}"
        );
    }
    // Only a protected base: the same errors at any other path are not a skip.
    for n in [libc::EPERM, libc::EACCES] {
        assert!(
            !plan.skips(&dir.join("other"), &io::Error::from_raw_os_error(n)),
            "{n}"
        );
    }
    for n in [
        libc::ENOENT,
        libc::ENOTDIR,
        libc::ELOOP,
        libc::EIO,
        libc::EBUSY,
    ] {
        assert!(
            !plan.skips(&dir.join("y"), &io::Error::from_raw_os_error(n)),
            "{n}"
        );
    }
    assert!(!plan.skips(&dir.join("y"), &io::Error::other("no errno")));
    let (a, b) = (dir.join("a"), dir.join("b"));
    assert!(first_denial(&a));
    assert!(!first_denial(&a));
    assert!(first_denial(&b));
    assert!(!first_denial(&b));

    // What is logged: one warning naming the base for two skips of it, one more for another.
    #[derive(Clone, Default)]
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl io::Write for Log {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let (c, d) = (dir.join("c"), dir.join("d"));
    tracing::subscriber::with_default(subscriber, || {
        let eperm = io::Error::from_raw_os_error(libc::EPERM);
        assert!(plan.skips(&c, &eperm));
        assert!(plan.skips(&c, &eperm));
        assert!(plan.skips(&d, &eperm));
    });
    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.contains(&c.display().to_string()), "{text}");
    assert!(text.contains(&d.display().to_string()), "{text}");
    assert!(text.contains("slot profile denies leases access"), "{text}");
}

/// Catches a deep tree that makes the sweep fail (a lease builds 300 directories to cost
/// its slot for good), a search of another owner's deep tree that fails or blocks, and a
/// removal that recurses without a bound: another uid's view of the tree is searched to
/// the limit with no error and no removal; the departing uid's own tree is removed whole.
#[test]
fn deep_trees_are_removed_and_other_owners_are_searched_to_a_limit() {
    let _base_own = scratch("deep");
    let base = _base_own.join("Shared");
    let mut deep = base.clone();
    for _ in 0..=MAX_DEPTH + 1 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("f"), "x").unwrap();
    // Another uid's view: the tree is someone else's; searched to the limit, no error.
    let swept = sweep(&owned_plan(&base), "u", other());
    assert_eq!(swept, Swept::default());
    // The departing uid's own tree: removed, whatever its depth.
    let swept = sweep(&owned_plan(&base), "u", me());
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(names(&base), Vec::<String>::new());
}

/// Makes `levels` directories below `top`, one inside the next, with a file in the last,
/// through descriptors (a path that long does not fit in one call).
fn build_deep(top: &Path, levels: usize) {
    let mut fd = rustix::fs::openat(rustix::fs::CWD, top, DIRECTORY, Mode::empty()).unwrap();
    for _ in 0..levels {
        rustix::fs::mkdirat(&fd, "d", Mode::from_raw_mode(0o700)).unwrap();
        fd = rustix::fs::openat(&fd, "d", DIRECTORY, Mode::empty()).unwrap();
    }
    let leaf = rustix::fs::openat(
        &fd,
        "leaf",
        OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    );
    drop(leaf.unwrap());
}

/// Catches a removal that cannot take a tree deeper than its recursion bound and one that
/// loses part of it: a tree 1000 directories
/// deep, with a file at every hundredth level, is removed by `empty`, by `sweep`, by a
/// named entry, and by a kept folder, leaving nothing and nothing parked behind.
#[test]
fn a_tree_a_thousand_deep_is_removed_by_every_walk() {
    for how in ["empty", "sweep", "named", "kept"] {
        let dir = scratch(&format!("thousand-{how}"));
        let top = dir.join("top");
        fs::create_dir(&top).unwrap();
        build_deep(&top, 1000);
        let swept = match how {
            "empty" => empty(&top, me()),
            "sweep" => sweep(&owned_plan(&dir), "u", me()),
            "named" => {
                let plan = SweepPlan {
                    named: vec![format!("{}/d", top.display())],
                    owned: Vec::new(),
                    protected: Vec::new(),
                };
                sweep(&plan, "u", me())
            }
            _ => empty_keeping(&dir, me(), &["top"]),
        };
        assert_eq!(swept.errors, Vec::<String>::new(), "{how}");
        let left = match how {
            "named" => names(&top),
            "sweep" => names(&dir),
            "kept" => names(&top),
            _ => names(&top),
        };
        assert_eq!(
            left,
            Vec::<String>::new(),
            "{how}: nothing is left or parked"
        );
    }
}

/// Catches: a walk that crosses into another mounted filesystem, or that removes a
/// directory other than the one it examined (one swapped in by a rename).
#[test]
fn another_filesystem_or_a_swapped_directory_is_not_entered() {
    let _base_own = scratch("dev");
    let base = _base_own.join("Shared");
    fs::create_dir(&base).unwrap();
    populate(&base);
    let dir = open_base(&base).unwrap().unwrap();
    let dev = rustix::fs::fstat(&dir).unwrap().st_dev;
    let mut swept = Swept::default();
    // Every entry looks as if it were on another filesystem: none is touched.
    walk_owned(
        &dir,
        &base,
        (me(), dev + 1),
        (0, &Park::new(&dir)),
        &mut swept,
    )
    .unwrap();
    assert_eq!(swept, Swept::default());
    assert_eq!(names(&base), ["empty", "file", "tree"]);

    let stat = rustix::fs::statat(&dir, "tree", AtFlags::SYMLINK_NOFOLLOW).unwrap();
    let error = remove_entry(
        &dir,
        OsStr::new("tree"),
        &stat,
        (me(), dev + 1, 1),
        &Park::new(&dir),
    )
    .unwrap_err();
    assert!(error.to_string().contains("another filesystem"), "{error}");

    let empty = rustix::fs::statat(&dir, "empty", AtFlags::SYMLINK_NOFOLLOW).unwrap();
    let error = open_child(&dir, OsStr::new("tree"), &empty).unwrap_err();
    assert!(error.to_string().contains("replaced"), "{error}");
    assert_eq!(names(&base), ["empty", "file", "tree"]);
}

#[test]
fn the_macos_plan_names_the_places_of_the_design() {
    let schedules = SweepPlan::macos_schedules();
    assert!(
        schedules
            .named
            .contains(&"/private/var/at/tabs/{user}".to_owned())
    );
    assert!(
        schedules
            .owned
            .contains(&PathBuf::from("/private/var/at/jobs"))
    );
    let plan = SweepPlan::macos();
    assert!(
        plan.named
            .contains(&"/private/var/db/com.apple.xpc.launchd/loginitems.{uid}.plist".to_owned())
    );
    assert!(plan.owned.contains(&Path::new("/Users").join("Shared")));
    assert!(plan.owned.contains(&PathBuf::from("/private/var/folders")));
    assert!(
        plan.named
            .contains(&"/private/var/db/mds/messages/{uid}".to_owned())
    );
    // Only the two data-vault stores are skipped when root is denied them: not
    // `/private/var/folders`, `/private/tmp`, the shared user folder nor the schedules.
    assert_eq!(
        plan.protected,
        [
            PathBuf::from("/private/var/protected/sfanalytics"),
            PathBuf::from("/private/var/protected/trustd/private"),
        ]
    );
    assert!(SweepPlan::macos_schedules().protected.is_empty());
    for store in ["sfanalytics", "trustd/private"] {
        assert!(
            plan.owned
                .contains(&Path::new("/private/var/protected").join(store)),
            "{store}"
        );
    }
}

/// Catches: an emptied folder that keeps an entry because the uid does not own it (a
/// reused user's home would keep what root or another uid left in it), one that
/// removes the folder itself or follows a link out of it, an absent folder taken for
/// an error, and an entry that cannot be removed swallowed or stopping the rest.
#[test]
fn empty_removes_every_entry_whoever_owns_it_and_keeps_the_folder() {
    let dir = scratch("empty");
    let home = dir.join("home");
    fs::create_dir(&home).unwrap();
    populate(&home);
    let outside = dir.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), "x").unwrap();
    symlink(&outside, home.join("link")).unwrap();
    // The entries are the test's, not `other()`'s: what is emptied goes whoever owns it.
    let swept = empty(&home, other());
    assert_eq!(
        swept,
        Swept {
            removed: 4,
            errors: Vec::new()
        }
    );
    assert!(names(&home).is_empty());
    assert!(home.is_dir());
    assert!(outside.join("kept").exists(), "the link was followed");
    assert_eq!(empty(&dir.join("absent"), me()), Swept::default());
    if root() {
        eprintln!("skipped the error half: permission bits do not stop root");
        return;
    }
    fs::create_dir(home.join("locked")).unwrap();
    fs::write(home.join("locked/f"), "x").unwrap();
    fs::write(home.join("file"), "x").unwrap();
    chmod(&home.join("locked"), 0o000);
    let swept = empty(&home, me());
    chmod(&home.join("locked"), 0o700);
    assert_eq!(swept.removed, 1, "{swept:?}");
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("home/locked:"), "{swept:?}");
    assert_eq!(names(&home), ["locked"]);
}

/// Catches (kbf#652): a sweep that records an error for an entry which
/// vanished between the listing of its folder and its examination, so `user-delete`
/// refused with "the sweep for ... is incomplete: ...: No such file or directory".
/// First the departing uid's own entries (gone before the walk examines them; the one
/// still there is removed), then the case of the issue: a file of another owner deep
/// in another owner's folders (`triald`'s temporary file under the lease user's
/// `/private/var/folders/.../T`), which the walk only examines.
#[test]
fn an_entry_that_vanishes_after_the_listing_is_not_an_error() {
    let _base_own = scratch("vanish-listed");
    let base = _base_own.join("Shared");
    fs::create_dir(&base).unwrap();
    fs::write(base.join("daemon-temp"), "x").unwrap();
    fs::create_dir_all(base.join("daemon-dir/inner")).unwrap();
    fs::write(base.join("kept-until-swept"), "x").unwrap();
    let (temp, dir) = (base.join("daemon-temp"), base.join("daemon-dir"));
    let swept = vanishing(
        after_listing(1, move || {
            remove(&temp);
            remove(&dir);
        }),
        || sweep(&owned_plan(&base), "kbf-lease-10-2", me()),
    );
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(swept.removed, 1, "{swept:?}");
    assert!(names(&base).is_empty());

    // `Shared` is listing 1, `T` 2, `com.apple.triald` 3.
    let triald = base.join("T/com.apple.triald");
    fs::create_dir_all(&triald).unwrap();
    fs::write(triald.join("TRIFileBackedMutableStringArray"), "x").unwrap();
    fs::write(triald.join("kept"), "x").unwrap();
    let gone = triald.join("TRIFileBackedMutableStringArray");
    let swept = vanishing(after_listing(3, move || remove(&gone)), || {
        sweep(&owned_plan(&base), "kbf-lease-10-2", other())
    });
    assert_eq!(swept, Swept::default());
    assert_eq!(names(&triald), ["kept"]);
}

/// Catches (kbf#652): a directory being emptied whose entries vanish
/// between its listing and their examination (the same daemon, one level down) that
/// fails the sweep, or one that leaves the directory behind because of it; and the
/// same for `empty`, which clears the lease user's home. The vanished entries are the
/// second listing's, the one of the directory being removed.
#[test]
fn entries_that_vanish_while_a_directory_is_emptied_are_not_an_error() {
    let _base_own = scratch("vanish-emptied");
    let base = _base_own.join("Shared");
    let tree = base.join("tree");
    for (run, uid) in [("sweep", me()), ("empty", other())] {
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("sub/f"), "x").unwrap();
        fs::write(tree.join("g"), "x").unwrap();
        let gone = tree.clone();
        // Both list `Shared` (1), then `tree` (2): the entries go in between.
        let hook = after_listing(2, move || {
            remove(&gone.join("g"));
            remove(&gone.join("sub"));
        });
        let swept = vanishing(hook, || {
            if run == "sweep" {
                sweep(&owned_plan(&base), "u", uid)
            } else {
                empty(&base, uid)
            }
        });
        assert_eq!(swept.errors, Vec::<String>::new(), "{run}");
        assert_eq!(swept.removed, 1, "{run}: {swept:?}");
        assert!(
            names(&base).is_empty(),
            "{run}: the emptied directory stayed"
        );
    }
}

/// Catches (kbf#652): `empty` (the home folder) recording an error for an
/// entry gone since the listing, or counting it as removed.
#[test]
fn empty_passes_over_an_entry_gone_since_the_listing() {
    let _home_own = scratch("vanish-home");
    let home = _home_own.join("home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("gone"), "x").unwrap();
    fs::write(home.join("file"), "x").unwrap();
    let gone = home.join("gone");
    let swept = vanishing(after_listing(1, move || remove(&gone)), || {
        empty(&home, other())
    });
    assert_eq!(
        swept,
        Swept {
            removed: 1,
            errors: Vec::new()
        }
    );
    assert!(names(&home).is_empty());
}

/// Catches (kbf#652): a directory gone between its examination and its
/// opening that fails the sweep: another owner's directory the walk searches, and one
/// of the departing uid it removes.
#[test]
fn a_directory_gone_since_it_was_examined_holds_nothing() {
    let _base_own = scratch("vanish-open");
    let base = _base_own.join("Shared");
    for (what, uid) in [("searched", other()), ("removed", me())] {
        fs::create_dir_all(base.join("dir/sub")).unwrap();
        fs::write(base.join("kept"), "x").unwrap();
        let swept = vanishing(after_examining("dir", base.join("dir")), || {
            sweep(&owned_plan(&base), "u", uid)
        });
        assert_eq!(swept.errors, Vec::<String>::new(), "{what}");
        assert!(!base.join("dir").exists(), "{what}");
        let _ = fs::remove_file(base.join("kept"));
    }
}

/// Catches (kbf#652): a removal step that fails on an entry gone since
/// it was examined (the unlink or the directory's opening answers "no such file").
/// The entry is examined, then removed by someone else, then removal runs on the stale
/// examination: no listing is involved, so this is the narrowest window.
#[test]
fn an_entry_gone_since_it_was_examined_is_removed_already() {
    let _base_own = scratch("vanish-stale");
    let base = _base_own.join("Shared");
    fs::create_dir_all(base.join("dir")).unwrap();
    fs::write(base.join("file"), "x").unwrap();
    let dir = open_base(&base).unwrap().expect("the folder exists");
    let dev = rustix::fs::fstat(&dir).unwrap().st_dev;
    for name in ["file", "dir"] {
        let stat = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        remove(&base.join(name));
        remove_entry(
            &dir,
            OsStr::new(name),
            &stat,
            (me(), dev, 1),
            &Park::new(&dir),
        )
        .unwrap_or_else(|why| panic!("{name}: {why}"));
    }
}

/// Catches (kbf#652): a directory removed after the walk opened it that
/// fails the walk when listed. The walk relies on rustix's `Dir`, which reads a removed
/// directory as empty (its own fallback for the "no such file or directory" the kernel
/// answers), so this pins that.
#[test]
fn listing_a_directory_removed_since_it_was_opened_finds_nothing() {
    let _dir_own = scratch("vanish-listing");
    let dir = _dir_own.join("dir");
    fs::create_dir(&dir).unwrap();
    let opened = open_base(&dir).unwrap().expect("the directory exists");
    fs::remove_dir(&dir).unwrap();
    assert_eq!(listed(&opened).unwrap(), Vec::<OsString>::new());
}

/// Catches (kbf#652): a fix that swallows more than "no such file": a
/// folder the sweep cannot read (permission denied) must still leave the sweep
/// incomplete when another entry vanishes in the same walk, and the vanished entry
/// must not add an error of its own.
#[test]
fn a_vanished_entry_does_not_hide_a_real_error() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let _base_own = scratch("vanish-real");
    let base = _base_own.join("Shared");
    fs::create_dir(&base).unwrap();
    fs::write(base.join("daemon-temp"), "x").unwrap();
    fs::create_dir(base.join("locked")).unwrap();
    fs::write(base.join("locked/f"), "x").unwrap();
    chmod(&base.join("locked"), 0o000);
    let temp = base.join("daemon-temp");
    let swept = vanishing(after_listing(1, move || remove(&temp)), || {
        sweep(&owned_plan(&base), "u", me())
    });
    chmod(&base.join("locked"), 0o700);
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("Shared/locked: "), "{swept:?}");
    assert!(!swept.errors[0].contains("No such file"), "{swept:?}");
}

/// A `/var/folders` entry as macOS makes it: the kept folders `0`, `C`, `T` and `X`,
/// each holding a file and a tree, and the stray `junk` file, `stray` directory and
/// `T`-named link the entry root must not keep.
fn entry_root(dir: &Path) -> PathBuf {
    let entry = dir.join("entry");
    fs::create_dir(&entry).unwrap();
    for kept in ["0", "C", "T", "X"] {
        fs::create_dir_all(entry.join(kept).join("sub/deeper")).unwrap();
        fs::write(entry.join(kept).join("file"), "x").unwrap();
        fs::write(entry.join(kept).join("sub/deeper/leaf"), "x").unwrap();
    }
    fs::write(entry.join("junk"), "x").unwrap();
    fs::create_dir_all(entry.join("stray/inside")).unwrap();
    fs::write(entry.join("stray/inside/f"), "x").unwrap();
    entry
}

/// Catches the entry sweep that removes the kept folders themselves (the next lease's
/// `getconf DARWIN_USER_TEMP_DIR` would name a folder that is gone), the one that
/// empties only the kept folders and leaves a file or folder of another name in the
/// entry root (a pool user's file there survived the clean: `junk` and `stray` here), one that keeps a folder that is
/// not named, and one that walks through a link planted under a kept name.
#[test]
fn empty_keeping_empties_the_named_folders_and_removes_everything_else() {
    let dir = scratch("keeping");
    let entry = entry_root(&dir);
    let outside = dir.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), "x").unwrap();
    // `Y` is not named: a link under a kept name is not a folder either.
    fs::remove_dir_all(entry.join("X")).unwrap();
    symlink(&outside, entry.join("X")).unwrap();
    let swept = empty_keeping(&entry, me(), &["0", "C", "T", "X"]);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(names(&entry), ["0", "C", "T"]);
    for kept in ["0", "C", "T"] {
        assert_eq!(names(&entry.join(kept)), Vec::<String>::new(), "{kept}");
    }
    assert!(outside.join("kept").exists(), "the link was followed");
    // Nothing named: the entry is emptied whole.
    let swept = empty_keeping(&entry, me(), &[]);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert!(names(&entry).is_empty());
}

/// Catches a kept folder whose contents the sweep cannot remove hiding behind the
/// removal of the rest: the error names the kept folder's entry and the other entries
/// still go.
#[test]
fn a_kept_folder_that_cannot_be_emptied_is_an_error_and_the_rest_goes() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("keeping-error");
    let entry = entry_root(&dir);
    fs::create_dir(entry.join("T/locked")).unwrap();
    fs::write(entry.join("T/locked/f"), "x").unwrap();
    chmod(&entry.join("T/locked"), 0o000);
    let swept = empty_keeping(&entry, me(), &["0", "C", "T", "X"]);
    chmod(&entry.join("T/locked"), 0o700);
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("entry/T/locked:"), "{swept:?}");
    assert_eq!(names(&entry), ["0", "C", "T", "X"]);
    assert_eq!(names(&entry.join("T")), ["locked"]);
    assert!(names(&entry.join("C")).is_empty());
}

/// Catches a kept folder removed under the sweep (gone since it was examined) read as an
/// error, as the plain walk treats a vanished entry.
#[test]
fn a_kept_folder_gone_since_it_was_examined_holds_nothing() {
    let dir = scratch("keeping-gone");
    let entry = entry_root(&dir);
    let gone = entry.join("C");
    let swept = vanishing(after_examining("C", gone), || {
        empty_keeping(&entry, me(), &["C"])
    });
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    assert!(names(&entry).is_empty());
}

/// Catches a mode that stays 0 (the owner's `u+rwx` is not set on what the departing
/// uid owns, so the removal that follows could not enter it as that uid), a clear that
/// changes what another uid owns (a hard link to a system file keeps its mode: here an
/// entry the walk does not count as the uid's), one that follows a link out of the
/// tree, one that walks into another mode's folder without listing it, and a walk that
/// stops at the first error.
#[test]
fn clear_flags_makes_the_uids_entries_rwx_and_leaves_other_owners_alone() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("clear");
    let home = dir.join("home");
    fs::create_dir_all(home.join("a/b")).unwrap();
    fs::write(home.join("a/b/f"), "x").unwrap();
    fs::write(home.join("g"), "x").unwrap();
    let outside = dir.join("outside");
    fs::write(&outside, "x").unwrap();
    chmod(&outside, 0o600);
    symlink(&outside, home.join("link")).unwrap();
    chmod(&home.join("a/b/f"), 0o400);
    chmod(&home.join("g"), 0o400);
    chmod(&home.join("a/b"), 0o500);
    chmod(&home, 0o500);
    let swept = clear_flags(&home, me());
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let modes = [
        mode(&home.join("a/b/f")),
        mode(&home.join("g")),
        mode(&home.join("a/b")),
        mode(&outside),
    ];
    // Another uid's entries: counted nowhere, changed nowhere.
    let others = clear_flags(&home, other());
    chmod(&home, 0o700);
    chmod(&home.join("a/b"), 0o700);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(modes, [0o700, 0o700, 0o700, 0o600]);
    assert_eq!(swept.removed, 5, "a, b, f, g and the link: {swept:?}");
    assert_eq!(others.removed, 0, "{others:?}");
    assert_eq!(clear_flags(&dir.join("absent"), me()), Swept::default());
}

/// Catches a flag clear that fails on a deep tree (a lease that builds one would cost its
/// slot for good: the removal that follows clears the locks itself) or that recurses
/// without a bound: past the limit it stops looking, with no error, for either owner.
#[test]
fn clear_flags_stops_at_its_depth_limit_without_an_error() {
    let dir = scratch("clear-deep");
    let home = dir.join("home");
    fs::create_dir(&home).unwrap();
    let mut deep = home.clone();
    for _ in 0..=MAX_DEPTH + 1 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    let swept = clear_flags(&home, me());
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    // Another owner's deep tree is nobody's to fail on: searched to the limit, no error.
    assert_eq!(clear_flags(&home, other()), Swept::default());
}

/// Catches a kept folder that another process swaps for a link between the examination and
/// the open being followed, or passing as emptied: the open refuses the link (it is
/// opened without following) and the entry is an error naming the folder, the link's
/// target untouched.
#[test]
fn a_kept_folder_swapped_for_a_link_since_it_was_examined_is_an_error() {
    let dir = scratch("kept-swap");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("C")).unwrap();
    let outside = dir.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), "x").unwrap();
    let (c, target) = (entry.join("C"), outside.clone());
    let done = std::cell::Cell::new(false);
    let swept = vanishing(
        move |point, _| {
            if matches!(point, Point::Examined(n) if *n == "C") && !done.replace(true) {
                remove(&c);
                symlink(&target, &c).unwrap();
            }
        },
        || empty_keeping(&entry, me(), &["C"]),
    );
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("entry/C:"), "{swept:?}");
    assert!(outside.join("kept").exists(), "the link was followed");
}

/// Catches an entry of a kept folder that vanishes between the listing and its examination
/// read as an error: it is removed already, and the folder is emptied without one.
#[test]
fn an_entry_of_a_kept_folder_gone_since_the_listing_is_no_error() {
    let dir = scratch("kept-gone-entry");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("C")).unwrap();
    fs::write(entry.join("C/only"), "x").unwrap();
    let only = entry.join("C/only");
    let swept = vanishing(after_listing(2, move || remove(&only)), || {
        empty_keeping(&entry, me(), &["C"])
    });
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
}

/// Catches a flag clear that errors on an entry gone since the listing, and one that enters
/// another filesystem: the first is passed over, and with every entry reported on another
/// device none is changed or entered.
#[test]
fn clear_flags_passes_over_a_vanished_entry_and_never_crosses_a_device() {
    let dir = scratch("clear-edges");
    let home = dir.join("home");
    fs::create_dir_all(home.join("a")).unwrap();
    fs::write(home.join("b"), "x").unwrap();
    chmod(&home.join("b"), 0o400);
    let gone = home.join("b");
    let swept = vanishing(after_listing(1, move || remove(&gone)), || {
        clear_flags(&home, me())
    });
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    let opened = open_base(&home).unwrap().unwrap();
    let dev = rustix::fs::fstat(&opened).unwrap().st_dev;
    let mut other_device = Swept::default();
    clear_below(&opened, &home, me(), dev + 1, 0, &mut other_device).unwrap();
    assert_eq!(other_device, Swept::default());
}

/// Runs `body` in a child of this test binary whose soft limit of open descriptors is 120
/// (a Mac's default is 256, and the daemon's soft limit can stay there): the parent runs
/// the test again as the child (`name` is the test's own name) and asserts it passed.
/// A walk that holds a descriptor per level to 256 fails in the child with `EMFILE`.
fn with_few_descriptors(name: &str, body: impl FnOnce()) {
    if std::env::var_os("KBF_FEW_DESCRIPTORS").is_some() {
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        let lowered = rustix::process::Rlimit {
            current: Some(120),
            maximum: limit.maximum,
        };
        rustix::process::setrlimit(rustix::process::Resource::Nofile, lowered).unwrap();
        body();
        return;
    }
    let path = module_path!().split_once("::").map_or("", |(_, rest)| rest);
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{path}::{name}"),
            "--test-threads=1",
            "--nocapture",
        ])
        .env("KBF_FEW_DESCRIPTORS", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "with 120 descriptors: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "the child ran the test: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Catches a walk that holds a descriptor per level to the full depth limit (a lease's
/// deep tree would then fail the flag clear with `EMFILE` on a Mac whose soft limit is
/// 256, on every re-clean): with only 120 descriptors, a tree 1000 deep is cleared of
/// flags and removed, each with no error.
#[test]
fn a_tree_a_thousand_deep_needs_few_descriptors() {
    with_few_descriptors("a_tree_a_thousand_deep_needs_few_descriptors", || {
        let dir = scratch("few-descriptors");
        let home = dir.join("home");
        fs::create_dir(&home).unwrap();
        build_deep(&home, 1000);
        let cleared = clear_flags(&home, me());
        assert_eq!(cleared.errors, Vec::<String>::new(), "{cleared:?}");
        let emptied = empty(&home, me());
        assert_eq!(emptied.errors, Vec::<String>::new(), "{emptied:?}");
        assert_eq!(names(&home), Vec::<String>::new());
    });
}

/// Catches a subtree moved up although it is not the entry that was judged (a directory
/// swapped in at its name since the judgement would be moved, and removed, as if it were
/// the uid's): the entry is looked at again before the move, and a different one is an
/// error that leaves it where it is.
#[test]
fn a_deep_subtree_replaced_since_it_was_judged_is_not_moved() {
    let dir = scratch("park-replaced");
    let top = dir.join("top");
    fs::create_dir(&top).unwrap();
    // Levels named l0 to l69, one inside the next, built through descriptors.
    let mut fd = rustix::fs::openat(rustix::fs::CWD, &top, DIRECTORY, Mode::empty()).unwrap();
    for level in 0..70 {
        let name = format!("l{level}");
        rustix::fs::mkdirat(&fd, name.as_str(), Mode::from_raw_mode(0o700)).unwrap();
        fd = rustix::fs::openat(&fd, name.as_str(), DIRECTORY, Mode::empty()).unwrap();
    }
    let mut parent = top.clone();
    for level in 0..64 {
        parent.push(format!("l{level}"));
    }
    let victim = parent.join("l64");
    let done = std::cell::Cell::new(false);
    let moved = parent.join("moved");
    let swept = vanishing(
        {
            let victim = victim.clone();
            move |point, _| {
                if matches!(point, Point::Examined(n) if *n == "l64") && !done.replace(true) {
                    fs::rename(&victim, &moved).unwrap();
                    fs::create_dir(&victim).unwrap();
                }
            }
        },
        || empty(&top, me()),
    );
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("replaced"), "{swept:?}");
    assert!(victim.is_dir(), "the replacement was moved or removed");
    assert!(parent.join("moved").is_dir(), "what was judged was lost");
}

/// Catches a move that replaces an entry already at the park's name (a name made ahead in
/// the tree being emptied would be lost or would stop the move): the name is taken, so
/// the next is used, and the entry made ahead is still there.
#[test]
fn a_name_made_ahead_at_the_parks_is_skipped_not_replaced() {
    let dir = scratch("park-name");
    let top = dir.join("top");
    fs::create_dir(&top).unwrap();
    build_deep(&top, 100);
    let planted = top.join(format!(".kbf-deep-{}-0", std::process::id()));
    let done = std::cell::Cell::new(false);
    let swept = vanishing(
        {
            let planted = planted.clone();
            move |point, _| {
                if matches!(point, Point::Examined(n) if *n == "d") && !done.replace(true) {
                    fs::create_dir(&planted).unwrap();
                    fs::write(planted.join("kept"), "x").unwrap();
                }
            }
        },
        || empty(&top, me()),
    );
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    assert_eq!(
        names(&top),
        [planted.file_name().unwrap().to_string_lossy().into_owned()]
    );
    assert!(
        planted.join("kept").exists(),
        "the entry made ahead was replaced"
    );
}

/// Catches a mode change that follows a link swapped in at an entry's name after it was
/// judged, or lands on another file swapped in (the target, which the uid does not own,
/// would be made writable by the owner bit): in either case the target's mode is as it
/// was.
#[test]
fn an_entry_swapped_in_after_the_judgement_does_not_change_its_targets_mode() {
    for swap in ["link", "file"] {
        let dir = scratch(&format!("chmod-{swap}"));
        let home = dir.join("home");
        fs::create_dir(&home).unwrap();
        fs::write(home.join("b"), "x").unwrap();
        chmod(&home.join("b"), 0o400);
        let target = dir.join("target");
        fs::write(&target, "x").unwrap();
        chmod(&target, 0o400);
        let done = std::cell::Cell::new(false);
        let (b, other) = (home.join("b"), target.clone());
        let swept = vanishing(
            move |point, _| {
                if matches!(point, Point::Examined(n) if *n == "b") && !done.replace(true) {
                    // Moved away, not removed: a freed inode number can be handed to the
                    // file made next, which would then look like the one judged.
                    fs::rename(&b, b.with_file_name("b.judged")).unwrap();
                    if swap == "link" {
                        symlink(&other, &b).unwrap();
                    } else {
                        fs::write(&b, "y").unwrap();
                        chmod(&b, 0o400);
                    }
                }
            },
            || clear_flags(&home, me()),
        );
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode(&target),
            0o400,
            "{swap}: the target was changed ({swept:?})"
        );
        if swap == "file" {
            assert_eq!(
                mode(&home.join("b")),
                0o400,
                "the file swapped in was changed"
            );
        }
    }
}
