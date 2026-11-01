//! The way a release appears on the host: made apart and renamed whole, state before links, swept stage directories, link order, and `gc` on unreadable directories.

use super::*;

/// One look at the host, taken just before a step was applied.
#[derive(Debug, Clone)]
struct Glance {
    /// `file` or `link`.
    step: &'static str,
    /// Whether `releases/<A>` exists under its final name.
    published: bool,
    /// The `.stage-` directories in `releases/`.
    stages: Vec<String>,
    /// Whether the stage (or final) directory already holds `kbf`.
    kbf_there: bool,
    /// What `state.json` names as `current`, as the state is on disk at this moment.
    state_current: Option<CommitId>,
}

/// A [`ScratchHost`] that looks at the disk before every file and link step, and can refuse
/// the link steps (a host that dies between the state and the links).
struct Watch {
    host: ScratchHost,
    layout: Layout,
    seen: std::cell::RefCell<Vec<Glance>>,
    /// The path of each link step, in the order the steps were applied.
    links: std::cell::RefCell<Vec<String>>,
    fail_links: bool,
}

impl Watch {
    fn new(name: &str) -> Watch {
        let (host, layout) = host(name);
        Watch {
            host,
            layout,
            seen: std::cell::RefCell::new(Vec::new()),
            links: std::cell::RefCell::new(Vec::new()),
            fail_links: false,
        }
    }

    fn glance(&self, step: &'static str) {
        let releases = self.host.at("/var/lib/kbf-installer/releases");
        let stages: Vec<String> = fs::read_dir(&releases)
            .map(|d| {
                d.map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with(".stage-"))
                    .collect()
            })
            .unwrap_or_default();
        let kbf_there = releases.join(A).join("kbf").exists()
            || stages.iter().any(|s| releases.join(s).join("kbf").exists());
        self.seen.borrow_mut().push(Glance {
            step,
            published: releases.join(A).exists(),
            stages,
            kbf_there,
            state_current: state::read_unlocked(&self.layout)
                .ok()
                .flatten()
                .and_then(|s| s.current),
        });
    }
}

impl Exec for Watch {
    fn euid(&self) -> u32 {
        self.host.euid()
    }
    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        self.host.inspect(path)
    }
    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>> {
        self.host.stat(path)
    }
    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.host.query(cmd)
    }
    fn http_status(&self, url: &HttpUrl) -> io::Result<u16> {
        self.host.http_status(url)
    }
    fn apply(&mut self, step: &Step) -> io::Result<()> {
        match step {
            Step::WriteFile { .. } => self.glance("file"),
            Step::Symlink { path, .. } => {
                self.links.borrow_mut().push(path.to_string());
                self.glance("link");
                if self.fail_links {
                    return Err(io::Error::other("the host died before the link moved"));
                }
            }
            _ => {}
        }
        self.host.apply(step)
    }
    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// S19: a new release is made apart and appears under its name whole. At every file the
/// install writes, `releases/<A>` does not exist and a `.stage-` directory does; when it is
/// done the name holds the release, the stage is gone, and only then are the links moved
/// (so at the first link the release is complete under its name).
///
/// Catches a release written in place: a crash (or a reader such as `gc`) in the middle would
/// find `releases/<A>` half written, and the next install would refuse it as "there with
/// other content" for ever.
///
/// Catches: `staged` set to `None` in `copy_release` (files written at the final name).
#[test]
fn a_new_release_is_made_apart_and_appears_whole_under_its_name() {
    let mut w = Watch::new("s19-apart");
    let a = source("s19-apart-src", "kbf A", Some(UNITS));
    let layout = w.layout.clone();
    let mut out = Vec::new();
    let install_a = Install {
        commit: c(A),
        from: a,
    };
    install(&mut w, &layout, me(), &install_a, Mode::Apply, &mut out).expect("install");
    let seen = w.seen.borrow().clone();
    let files: Vec<_> = seen.iter().filter(|g| g.step == "file").collect();
    assert_eq!(files.len(), 3, "kbf and two units: {seen:?}");
    for g in &files {
        assert!(
            !g.published,
            "a file was written under the final name: {g:?}"
        );
        assert_eq!(g.stages.len(), 1, "{g:?}");
    }
    let first_link = seen.iter().find(|g| g.step == "link").expect("a link");
    assert!(
        first_link.published && first_link.kbf_there,
        "{first_link:?}"
    );
    assert!(
        first_link.stages.is_empty(),
        "the stage is gone: {first_link:?}"
    );
    assert_eq!(releases_left(&w.host), [A.to_owned(), "current".to_owned()]);
    assert_eq!(
        fs::read_to_string(
            w.host
                .at(&format!("/var/lib/kbf-installer/releases/{A}/kbf"))
        )
        .expect("kbf"),
        "kbf A"
    );
}

/// S19 and S23: the state is the truth and is written after the release is whole and before
/// any link moves. At the first link the state already names the new release; if the host
/// dies there (the link step refused here), the state says `current` is A, the link is
/// absent, and `reconcile` carries the swap out.
///
/// Catches links moved before the state is written (a crash would leave a link the state does
/// not know, and `reconcile` would move it back to the old release), and a state written
/// before the release is whole.
///
/// Catches: `switch` writing the state after `execute` of the links.
#[test]
fn the_state_is_written_before_the_links_and_reconcile_finishes_a_swap_that_died() {
    let mut w = Watch::new("s23-state-first");
    w.fail_links = true;
    let a = source("s23-state-first-src", "kbf A", None);
    let layout = w.layout.clone();
    let err = install_on_watch(&mut w, &layout, &a).expect_err("the link step is refused");
    assert!(err.to_string().contains("died"), "{err}");
    let seen = w.seen.borrow().clone();
    let at_link = seen.iter().find(|g| g.step == "link").expect("a link step");
    assert_eq!(
        at_link.state_current,
        Some(c(A)),
        "the state came after the links"
    );
    assert!(
        at_link.published,
        "the release was not whole under its name yet"
    );
    assert_eq!(state_of(&layout).current, Some(c(A)));
    assert_eq!(link(&w.host, "current"), None, "no link moved");

    w.fail_links = false;
    let mut log = Vec::new();
    assert!(crate::activate::reconcile(&mut w, &layout, &mut log).expect("reconcile"));
    assert_eq!(link(&w.host, "current"), Some(A.to_owned()));
}

fn install_on_watch(w: &mut Watch, layout: &Layout, from: &Path) -> Result<(), ReleaseError> {
    let a = Install {
        commit: c(A),
        from: from.to_owned(),
    };
    install(w, layout, me(), &a, Mode::Apply, &mut Vec::new())
}

/// S19: a copy that fails leaves nothing under the release's name and no stage directory, and
/// a retry then works (the old code left a half directory the retry refused).
///
/// Catches a stage directory left behind by a failed copy, and a name taken by a partial.
#[test]
fn a_failed_copy_leaves_no_stage_and_nothing_under_the_name() {
    let from = source("s19-failed", "kbf A", None);
    let (host, root) = host("s19-failed");
    let mut exec = BadCopy {
        host,
        skip: false,
        hit: "/kbf",
    };
    let a = Install {
        commit: c(A),
        from: from.clone(),
    };
    install(&mut exec, &root, me(), &a, Mode::Apply, &mut Vec::new()).expect_err("bad copy");
    assert_eq!(releases_left(&exec.host), Vec::<String>::new());
    let mut good = exec.host;
    install(&mut good, &root, me(), &a, Mode::Apply, &mut Vec::new()).expect("retry");
    assert_eq!(releases_left(&good), [A.to_owned(), "current".to_owned()]);
}

/// S19: a dry run prints the plan against the final names and makes nothing, no stage
/// directory included; a re-install of the release that is there is done in place (no stage,
/// no rename), as it always was.
#[test]
fn a_dry_run_makes_no_stage_and_a_reinstall_is_in_place() {
    let (mut exec, root) = host("s19-dry");
    let from = source("s19-dry-src", "kbf A", None);
    let out = install_on(&mut exec, &root, A, &from, Mode::DryRun).expect("dry");
    assert!(!out.contains(".stage-"), "{out}");
    assert_eq!(releases_left(&exec), Vec::<String>::new());
    install_on(&mut exec, &root, A, &from, Mode::Apply).expect("install");
    let again = install_on(&mut exec, &root, A, &from, Mode::Apply).expect("again");
    assert!(!again.contains(".stage-"), "{again}");
    assert_eq!(releases_left(&exec), [A.to_owned(), "current".to_owned()]);
}

/// Every file under `dir` with its bytes, and every directory, by relative name.
fn snapshot(dir: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for item in fs::read_dir(at).expect("list") {
            let path = item.expect("entry").path();
            let name = path
                .strip_prefix(root)
                .expect("below")
                .display()
                .to_string();
            let meta = fs::symlink_metadata(&path).expect("stat");
            if meta.is_dir() {
                out.insert(name, None);
                walk(root, &path, out);
            } else if meta.is_file() {
                out.insert(name, Some(fs::read(&path).expect("read")));
            } else {
                out.insert(
                    name,
                    Some(
                        fs::read_link(&path)
                            .expect("link")
                            .display()
                            .to_string()
                            .into_bytes(),
                    ),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// S21: `stage` and `install` read the state before they publish anything. With the state
/// unusable (both files damaged, host activated) they stop with the releases directory
/// byte for byte as it was: no release, no stage directory.
///
/// Catches `stage` copying the release first and only then finding the state unusable, which
/// left a published release behind an error that said "nothing was changed".
///
/// Catches: the state read in `stage` moved after `copy_release`.
#[test]
fn stage_and_install_on_an_unusable_state_publish_nothing() {
    let (mut exec, root) = host("s21-stage");
    let a = source("s21-stage-a", "kbf A", None);
    let b = source("s21-stage-b", "kbf B", None);
    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    install_on(&mut exec, &root, B, &a, Mode::Apply).expect("install B");
    let dir = exec.at("/var/lib/kbf-installer");
    fs::write(dir.join("state.json"), "{ not json").expect("ruin");
    fs::write(dir.join("state.json.bak"), "{ not json").expect("ruin");
    let before = snapshot(&dir);

    let mut out = Vec::new();
    let a2 = Install {
        commit: c(C),
        from: b.clone(),
    };
    let err = stage(&mut exec, &root, me(), &a2, Mode::Apply, &mut out).expect_err("stage");
    assert!(
        matches!(&err, ReleaseError::State(s) if s.needs_operator()),
        "{err}"
    );
    let err = install(&mut exec, &root, me(), &a2, Mode::Apply, &mut out).expect_err("install");
    assert!(
        matches!(&err, ReleaseError::State(s) if s.needs_operator()),
        "{err}"
    );
    assert_eq!(snapshot(&dir), before, "a refused verb changed the host");
}

/// S19: a `.stage-` directory a killed run left (between making the release and renaming
/// it) is removed, under the lock, at the start of `install`, `stage` and `gc`; a dry run
/// removes nothing; a real release is never touched.
///
/// Catches stage directories piling up for ever (each is a whole release), and a sweep that
/// takes a published release for a stage.
///
/// Catches: the sweep call left out of `install`.
#[test]
fn a_stage_directory_a_killed_run_left_is_swept_by_install_stage_and_gc() {
    let (mut exec, root) = host("s19-sweep");
    let a = source("s19-sweep-a", "kbf A", None);
    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    let leftover = exec.at(&format!("/var/lib/kbf-installer/releases/.stage-{B}.dead"));
    let plant = || {
        fs::create_dir_all(leftover.join("units")).expect("stage");
        fs::write(leftover.join("kbf"), "half").expect("file");
    };
    plant();
    // A stage that is a plain file is removed too, not only a directory.
    let stray = exec.at("/var/lib/kbf-installer/releases/.stage-stray-file");
    fs::write(&stray, "x").expect("file");
    install_on(&mut exec, &root, A, &a, Mode::DryRun).expect("dry");
    assert!(stray.exists(), "a dry run removed a file");
    assert!(leftover.exists(), "a dry run removed something");
    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install");
    assert!(!leftover.exists(), "install left the stage");
    assert!(!stray.exists(), "install left the stage file");
    plant();
    let st = Install {
        commit: c(B),
        from: source("s19-sweep-b", "kbf B", None),
    };
    stage(&mut exec, &root, me(), &st, Mode::Apply, &mut Vec::new()).expect("stage");
    assert!(!leftover.exists(), "stage left the stage");
    plant();
    gc_on(&mut exec, &root, 5).expect("gc");
    assert!(!leftover.exists(), "gc left the stage");
    assert!(
        exec.at(&format!("/var/lib/kbf-installer/releases/{A}/kbf"))
            .exists()
    );
    assert!(
        exec.at(&format!("/var/lib/kbf-installer/releases/{B}/kbf"))
            .exists()
    );
}

/// The state directory and the install root are the package's: made when missing, never
/// chmod-ed when there. A 0700 `/var/db/kbf-installer` (macOS) and a 0750 install root stay
/// as they are; the ones the install makes are 0755.
///
/// Catches an install that widens a directory the package made private.
///
/// Catches: the "only when missing" test dropped (both always planned).
#[test]
fn an_existing_state_directory_and_install_root_keep_their_mode() {
    let (mut exec, layout) = mac("s5-modes");
    for (dir, mode) in [
        ("/var/db/kbf-installer", 0o700),
        ("/usr/local/kbf-installer", 0o750),
    ] {
        fs::create_dir_all(exec.at(dir)).expect("pkg dir");
        fs::set_permissions(exec.at(dir), fs::Permissions::from_mode(mode)).expect("mode");
    }
    let from = mac_source("s5-modes-src", "daemon A");
    install_on(&mut exec, &layout, A, &from, Mode::Apply).expect("install");
    assert_eq!(mode_of(&exec, "/var/db/kbf-installer"), 0o700);
    assert_eq!(mode_of(&exec, "/usr/local/kbf-installer"), 0o750);
    assert_eq!(mode_of(&exec, "/usr/local/kbf-installer/releases"), 0o755);

    let (mut exec, root) = host("s5-modes-linux");
    fs::create_dir_all(exec.at("/var/lib/kbf-installer")).expect("dir");
    fs::set_permissions(
        exec.at("/var/lib/kbf-installer"),
        fs::Permissions::from_mode(0o750),
    )
    .expect("mode");
    let from = source("s5-modes-linux-src", "kbf A", None);
    install_on(&mut exec, &root, A, &from, Mode::Apply).expect("install");
    assert_eq!(mode_of(&exec, "/var/lib/kbf-installer"), 0o750);
}

/// The links move `previous` first, then `current`: until `current` moves, both name whole
/// releases. Checked for `install` (the second install has both) and for `reconcile`.
///
/// Catches the order swapped, which would for an instant point `current` at the new release
/// with `previous` still on the one before it, and lose `previous` on a crash between.
///
/// Catches: `previous` and `current` swapped in `switch`; the same in `converge`.
#[test]
fn the_links_move_previous_before_current() {
    let mut w = Watch::new("m7-order");
    let layout = w.layout.clone();
    let a = source("m7-a", "kbf A", None);
    let b = source("m7-b", "kbf B", None);
    let inst = |commit: &str, from: &Path| Install {
        commit: c(commit),
        from: from.to_owned(),
    };
    install(
        &mut w,
        &layout,
        me(),
        &inst(A, &a),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect("A");
    w.links.borrow_mut().clear();
    install(
        &mut w,
        &layout,
        me(),
        &inst(B, &b),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect("B");
    let names: Vec<String> = w
        .links
        .borrow()
        .iter()
        .map(|p| p.rsplit('/').next().expect("name").to_owned())
        .collect();
    assert_eq!(names, ["previous", "current"]);

    // The same for the repair of the links after an interrupted verb.
    fs::remove_file(w.host.at("/var/lib/kbf-installer/releases/previous")).expect("rm");
    fs::remove_file(w.host.at("/var/lib/kbf-installer/releases/current")).expect("rm");
    w.links.borrow_mut().clear();
    crate::activate::reconcile(&mut w, &layout, &mut Vec::new()).expect("reconcile");
    let names: Vec<String> = w
        .links
        .borrow()
        .iter()
        .map(|p| p.rsplit('/').next().expect("name").to_owned())
        .collect();
    assert_eq!(names, ["previous", "current"]);
}

/// Stage names do not repeat within a run (nor across runs: the pid and the clock are in them).
///
/// Catches two stages of one process landing in one directory.
///
/// Catches: `unique` returning a constant.
#[test]
fn stage_names_are_unique() {
    let names: std::collections::BTreeSet<String> = (0..2_000).map(|_| unique()).collect();
    assert_eq!(names.len(), 2_000);
}

/// Catches a path the installer cannot look at being taken for one that is not there (it
/// would then plan to make it, over something it never saw): with `releases/` unreadable, a
/// dry run of an install stops with the host's error naming the path, and changes nothing.
///
/// Catches: the inspect error read as `None` (the dry run plans the release).
#[test]
fn a_path_that_cannot_be_inspected_is_an_error_not_an_absent_one() {
    let (mut exec, root) = host("inspect-denied");
    let a = source("inspect-denied-a", "kbf A", None);
    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    let releases = exec.at("/var/lib/kbf-installer/releases");
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o000)).expect("lock the directory");
    let err = install_on(&mut exec, &root, B, &a, Mode::DryRun);
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o755)).expect("unlock");
    let err = err.expect_err("denied");
    assert!(matches!(err, ReleaseError::Host(_)), "{err}");
    assert!(err.to_string().contains("releases"), "{err}");
}

/// Catches `gc` failing on a host whose `releases/` directory is not there yet (a host that
/// was never installed to): it has nothing to remove and says so.
///
/// Catches: the missing directory an error.
#[test]
fn gc_with_no_releases_directory_has_nothing_to_do() {
    let (mut exec, root) = host("gc-empty");
    let out = gc_on(&mut exec, &root, 2).expect("gc");
    assert!(!out.contains("remove"), "{out}");
}

/// Catches a `releases/` directory that cannot be listed being read as an empty one (`gc`
/// would then report nothing to remove on a host full of old releases): only a missing
/// directory is empty, an unreadable one is an error naming it.
///
/// Catches: any listing error taken as "no releases".
#[test]
fn gc_on_an_unreadable_releases_directory_is_an_error_not_an_empty_one() {
    let (mut exec, root) = host("gc-denied");
    let a = source("gc-denied-a", "kbf A", None);
    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    let releases = exec.at("/var/lib/kbf-installer/releases");
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o100))
        .expect("traversable, not listable");
    let result = gc(
        &mut exec,
        &root,
        2,
        &Default::default(),
        Mode::DryRun,
        &mut Vec::new(),
    );
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o755)).expect("unlock");
    let err = result.expect_err("denied");
    assert!(matches!(err, ReleaseError::Read { .. }), "{err}");
}
