use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::Output;
use std::time::{Duration, UNIX_EPOCH};

use kbf_host::exec::{Entry, Meta};
use kbf_host::path::Root;
use kbf_host::step::{Cmd, HttpUrl, Step};
use kbf_host::testkit::{ScratchHost, scratch};
use kbf_host::trust::Untrusted;

use super::super::state::PendingConfirm;
use super::super::state::tests::{A, B, C, c};
use super::*;

const D: &str = "dddddddddddddddddddddddddddddddddddddddd";
const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const S: &str = "ffffffffffffffffffffffffffffffffffffffff";

/// The test's own user, standing in for root as the owner of a release: a scratch apply
/// can chown only to it.
fn me() -> Owner {
    Owner {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    }
}

/// A scratch host with the directories a Linux host has above the layout.
fn host(name: &str) -> (ScratchHost, Layout) {
    let exec = ScratchHost::new(&format!("deploy-release-{name}"));
    for sub in ["run", "var/lib", "usr/local", "etc"] {
        fs::create_dir_all(exec.root.join(sub)).expect("seed");
    }
    let root = Layout::linux(Root::new(&exec.root).expect("absolute"));
    (exec, root)
}

/// A release to install: `kbf` holding `binary`, and `units/` with `units` when given.
fn source(name: &str, binary: &str, units: Option<&[(&str, &str)]>) -> PathBuf {
    let dir = scratch(&format!("deploy-release-src-{name}"));
    fs::write(dir.join("kbf"), binary).expect("binary");
    if let Some(units) = units {
        fs::create_dir(dir.join("units")).expect("units");
        for (unit, text) in units {
            fs::write(dir.join("units").join(unit), text).expect("unit");
        }
    }
    dir
}

const UNITS: &[(&str, &str)] = &[
    ("kbf-server.service", "[Unit]\n"),
    ("kbf.slice", "[Slice]\n"),
];

fn install_on(
    exec: &mut ScratchHost,
    root: &Layout,
    commit: &str,
    from: &Path,
    mode: Mode,
) -> Result<String, ReleaseError> {
    let a = Install {
        commit: c(commit),
        from: from.to_owned(),
    };
    let mut out = Vec::new();
    install(exec, root, me(), &a, mode, &mut out)?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

fn rollback_on(exec: &mut ScratchHost, root: &Layout, mode: Mode) -> Result<String, ReleaseError> {
    let mut out = Vec::new();
    rollback(exec, root, mode, &mut out)?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

fn gc_on(exec: &mut ScratchHost, root: &Layout, keep: usize) -> Result<String, ReleaseError> {
    let mut out = Vec::new();
    gc(exec, root, keep, &BTreeSet::new(), Mode::Apply, &mut out)?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

/// Where a link under `/usr/local/kbf` points; `None` when there is no link.
fn link(exec: &ScratchHost, name: &str) -> Option<String> {
    // A Linux host's tree, else a Mac's.
    let linux = exec.at(&format!("/var/lib/kbf-installer/releases/{name}"));
    let mac = exec.at(&format!("/usr/local/kbf-installer/releases/{name}"));
    let target = fs::read_link(linux).or_else(|_| fs::read_link(mac)).ok()?;
    Some(target.display().to_string())
}

fn state_of(root: &Layout) -> State {
    state::read_unlocked(root).expect("read").expect("a state")
}

fn mode_of(exec: &ScratchHost, path: &str) -> u32 {
    fs::symlink_metadata(exec.at(path)).expect(path).mode() & 0o7777
}

/// Catches an install that leaves `current` or `previous` on the wrong release (or
/// records other commits than the links name), a rollback that does not go back, a
/// rollback with no previous release that does anything but refuse (it would point
/// `current` at nothing, or at itself), and a release copied with other bytes or modes.
///
/// Catches: rollback falling back to the current release when there is no
/// previous one (`state.previous.clone().or(state.current.clone())`).
#[test]
fn install_twice_then_roll_back_and_refuse_a_second_rollback() {
    let (mut exec, root) = host("flow");
    let a = source("flow-a", "kbf A", Some(UNITS));
    let out = install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    assert!(
        out.contains(&format!(
            "links: current none -> {A}, previous none -> none\n"
        )),
        "{out}"
    );
    assert!(
        out.ends_with("then check `kbf-deploy host status`\n"),
        "{out}"
    );
    assert_eq!(link(&exec, "current"), Some(A.to_owned()));
    assert_eq!(link(&exec, "previous"), None);
    let rel = format!("/var/lib/kbf-installer/releases/{A}");
    assert_eq!(
        fs::read_to_string(exec.at(&format!("{rel}/kbf"))).expect("kbf"),
        "kbf A"
    );
    let unit = fs::read_to_string(exec.at(&format!("{rel}/units/kbf.slice"))).expect("unit");
    assert_eq!(unit, "[Slice]\n");
    let modes = [
        ("/var/lib/kbf-installer", 0o755),
        ("/var/lib/kbf-installer/releases", 0o755),
        (rel.as_str(), 0o755),
        (&format!("{rel}/kbf"), 0o755),
        (&format!("{rel}/units"), 0o755),
        (&format!("{rel}/units/kbf-server.service"), 0o644),
    ];
    for (path, mode) in modes {
        assert_eq!(mode_of(&exec, path), mode, "{path}");
    }

    let b = source("flow-b", "kbf B", None);
    let out = install_on(&mut exec, &root, B, &b, Mode::Apply).expect("install B");
    assert!(
        out.contains(&format!(
            "links: current {A} -> {B}, previous none -> {A}\n"
        )),
        "{out}"
    );
    assert_eq!(link(&exec, "current"), Some(B.to_owned()));
    assert_eq!(link(&exec, "previous"), Some(A.to_owned()));
    let s = state_of(&root);
    assert_eq!((s.current, s.previous), (Some(c(B)), Some(c(A))));

    let out = rollback_on(&mut exec, &root, Mode::Apply).expect("rollback");
    assert!(
        out.contains(&format!(
            "links: current {B} -> {A}, previous {A} -> none\n"
        )),
        "{out}"
    );
    assert_eq!(link(&exec, "current"), Some(A.to_owned()));
    assert_eq!(link(&exec, "previous"), None);
    let s = state_of(&root);
    assert_eq!((s.current, s.previous), (Some(c(A)), None));

    let err = rollback_on(&mut exec, &root, Mode::Apply).expect_err("no previous");
    assert_eq!(err.to_string(), "no previous release to roll back to");
    assert_eq!(link(&exec, "current"), Some(A.to_owned()));
    assert_eq!(state_of(&root).current, Some(c(A)));
}

/// Catches a rollback onto a release whose directory (or binary) is gone, which would
/// leave `current` pointing at nothing a service can start.
#[test]
fn a_rollback_onto_a_release_that_is_gone_is_refused() {
    let (mut exec, root) = host("gone");
    install_on(
        &mut exec,
        &root,
        A,
        &source("gone-a", "A", None),
        Mode::Apply,
    )
    .expect("A");
    install_on(
        &mut exec,
        &root,
        B,
        &source("gone-b", "B", None),
        Mode::Apply,
    )
    .expect("B");
    fs::remove_file(exec.at(&format!("/var/lib/kbf-installer/releases/{A}/kbf"))).expect("remove");
    let err = rollback_on(&mut exec, &root, Mode::Apply).expect_err("gone");
    assert_eq!(
        err.to_string(),
        format!("the previous release {A} is gone (no binary at its directory)")
    );
    assert_eq!(link(&exec, "current"), Some(B.to_owned()));
}

/// Catches an install that replaces a release directory already there with other
/// content (a commit names one build; different bytes under its name are a wrong or
/// tampered copy), that takes a file at the release's path for a directory, or that
/// refuses the same release again; and an install of the current release that moves
/// `previous` onto it.
#[test]
fn a_release_directory_with_other_content_is_refused() {
    let (mut exec, root) = host("differs");
    let a = source("differs-a", "kbf A", Some(UNITS));
    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    let rel = format!("/var/lib/kbf-installer/releases/{A}");
    let other_binary = source("differs-bin", "kbf X", Some(UNITS));
    let other_unit = source("differs-unit", "kbf A", Some(&[("kbf.slice", "[Other]\n")]));
    let new_unit = source("differs-new", "kbf A", Some(&[("new.service", "[Unit]\n")]));
    let cases = [
        (other_binary, format!("{rel}/kbf")),
        (other_unit, format!("{rel}/units/kbf.slice")),
        (new_unit, format!("{rel}/units/new.service")),
    ];
    for (from, path) in cases {
        let err = install_on(&mut exec, &root, A, &from, Mode::Apply).expect_err(&path);
        assert_eq!(
            err.to_string(),
            format!("{path} differs from the release being installed: a release is never replaced")
        );
    }
    assert_eq!(
        fs::read_to_string(exec.at(&format!("{rel}/kbf"))).expect("kbf"),
        "kbf A"
    );

    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("the same release again");
    let s = state_of(&root);
    assert_eq!((s.current, s.previous), (Some(c(A)), None));

    fs::write(
        exec.at(&format!("/var/lib/kbf-installer/releases/{B}")),
        "a file",
    )
    .expect("seed");
    let err = install_on(&mut exec, &root, B, &a, Mode::Apply).expect_err("a file");
    assert_eq!(
        err.to_string(),
        format!("/var/lib/kbf-installer/releases/{B} is there and is not a directory")
    );
    assert_eq!(state_of(&root).current, Some(c(A)));
}

/// A [`ScratchHost`] whose copy of `kbf` lands wrong: with other bytes, or not at all.
struct BadCopy {
    host: ScratchHost,
    skip: bool,
    /// The file whose copy lands wrong: the path ends with this.
    hit: &'static str,
}

impl Exec for BadCopy {
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
            Step::WriteFile {
                path, perms, owner, ..
            } if path.as_str().ends_with(self.hit) => {
                if self.skip {
                    return Ok(());
                }
                let bytes = b"tampered".to_vec();
                let (path, perms, owner) = (path.clone(), *perms, *owner);
                self.host.apply(&Step::WriteFile {
                    path,
                    perms,
                    owner,
                    bytes,
                })
            }
            _ => self.host.apply(step),
        }
    }
    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// Catches a copy that is not checked before `current` names it: a binary that landed
/// with other bytes, or not at all, is refused and no link or state moves.
#[test]
fn a_copy_whose_sha256_differs_is_refused_before_the_switch() {
    let from = source("badcopy", "kbf A", None);
    let want = sha256(b"kbf A");
    for (skip, got) in [
        (false, sha256(b"tampered")),
        (true, "none (no file)".to_owned()),
    ] {
        let (host, root) = host(&format!("badcopy-{skip}"));
        let mut exec = BadCopy {
            host,
            skip,
            hit: "/kbf",
        };
        let a = Install {
            commit: c(A),
            from: from.clone(),
        };
        let err = install(&mut exec, &root, me(), &a, Mode::Apply, &mut Vec::new())
            .expect_err("a bad copy");
        // The copy is read back where it was made: under the stage name, before any rename.
        let (head, tail) = (
            format!("the copy at /var/lib/kbf-installer/releases/.stage-{A}."),
            format!("/kbf has sha256 {got}; the source has {want}"),
        );
        let text = err.to_string();
        assert!(text.starts_with(&head) && text.ends_with(&tail), "{text}");
        assert_eq!(link(&exec.host, "current"), None);
        assert_eq!(state::read_unlocked(&root).expect("read"), None);
    }
}

/// Catches a copy checked for the first binary only: in a release of two binaries the
/// second lands with other bytes (or not at all) and is refused the same way, before any
/// link or state names the release.
///
/// Catches: the read-back loop over `source.binaries` cut to its first element.
#[test]
fn every_binary_of_a_release_is_read_back_not_only_the_first() {
    let from = source("badcopy2", "kbf A", None);
    fs::write(from.join("kbf-daemon"), "daemon A").expect("daemon");
    for (skip, got) in [
        (false, sha256(b"tampered")),
        (true, "none (no file)".to_owned()),
    ] {
        let (host, root) = host(&format!("badcopy2-{skip}"));
        let mut exec = BadCopy {
            host,
            skip,
            hit: "/kbf-daemon",
        };
        let a = Install {
            commit: c(A),
            from: from.clone(),
        };
        let err = install(&mut exec, &root, me(), &a, Mode::Apply, &mut Vec::new())
            .expect_err("a bad copy of the second binary");
        let want = sha256(b"daemon A");
        let (head, tail) = (
            format!("the copy at /var/lib/kbf-installer/releases/.stage-{A}."),
            format!("/kbf-daemon has sha256 {got}; the source has {want}"),
        );
        let text = err.to_string();
        assert!(text.starts_with(&head) && text.ends_with(&tail), "{text}");
        assert_eq!(link(&exec.host, "current"), None);
        assert_eq!(state::read_unlocked(&root).expect("read"), None);
    }
}

/// Every file and link under `dir`, with its mode and its bytes or target.
fn tree(dir: &Path) -> BTreeMap<PathBuf, (u32, String)> {
    let mut found = BTreeMap::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(d) = dirs.pop() {
        for item in fs::read_dir(&d).expect("list") {
            let p = item.expect("entry").path();
            let meta = fs::symlink_metadata(&p).expect("stat");
            let what = if meta.is_dir() {
                dirs.push(p.clone());
                String::new()
            } else if meta.file_type().is_symlink() {
                fs::read_link(&p).expect("link").display().to_string()
            } else {
                fs::read_to_string(&p).expect("file")
            };
            found.insert(p, (meta.mode(), what));
        }
    }
    found
}

/// Catches a dry run that changes anything: copies, moves a link, writes the state or
/// even makes the lock file. Each verb runs dry on a fresh host and on one with two
/// releases, and the tree must be byte for byte what it was.
///
/// Catches: the lock taken in a dry run too (`Mode::DryRun => Some(..)`).
#[test]
fn a_dry_run_writes_nothing() {
    let (mut exec, root) = host("dry");
    let a = source("dry-a", "kbf A", Some(UNITS));
    let before = tree(&exec.root);
    let out = install_on(&mut exec, &root, A, &a, Mode::DryRun).expect("dry install");
    assert!(
        out.contains("DRY-RUN: write /var/lib/kbf-installer/releases/"),
        "{out}"
    );
    assert!(!out.contains("next, by hand"), "{out}");
    let mut out = Vec::new();
    gc(
        &mut exec,
        &root,
        0,
        &BTreeSet::new(),
        Mode::DryRun,
        &mut out,
    )
    .expect("dry gc");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        "keeping: none and the 2 newest others; removing: none\nnothing to do\n"
    );
    assert_eq!(tree(&exec.root), before);
    assert!(!exec.at("/run/kbf-installer/lock").exists());

    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("A");
    install_on(
        &mut exec,
        &root,
        B,
        &source("dry-b", "kbf B", None),
        Mode::Apply,
    )
    .expect("B");
    // Three others besides current and previous: the two newest stay (the floor of `keep`),
    // so the dry gc below has C, the oldest, to name.
    seed_release(&exec, C, 1);
    seed_release(&exec, E, 2_000);
    seed_release(&exec, S, 3_000);
    let before = tree(&exec.root);
    let out = install_on(
        &mut exec,
        &root,
        C,
        &source("dry-c", "C", None),
        Mode::DryRun,
    );
    assert!(out.expect_err("C differs").to_string().contains("differs"));
    install_on(
        &mut exec,
        &root,
        D,
        &source("dry-d", "D", None),
        Mode::DryRun,
    )
    .expect("D");
    let out = rollback_on(&mut exec, &root, Mode::DryRun).expect("dry rollback");
    assert!(
        out.contains(&format!(
            "DRY-RUN: link /var/lib/kbf-installer/releases/current -> {A}"
        )),
        "{out}"
    );
    let mut out = Vec::new();
    gc(
        &mut exec,
        &root,
        0,
        &BTreeSet::new(),
        Mode::DryRun,
        &mut out,
    )
    .expect("dry gc");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(
        out.contains(&format!(
            "DRY-RUN: remove /var/lib/kbf-installer/releases/{C}\n"
        )),
        "{out}"
    );
    assert_eq!(tree(&exec.root), before);
}

/// A release directory made by hand: `kbf`, `units/a.service`, and a link out of it
/// to `escape` (removed, never followed), modified at `secs` after the epoch.
fn seed_release(exec: &ScratchHost, commit: &str, secs: u64) {
    let dir = exec.at(&format!("/var/lib/kbf-installer/releases/{commit}"));
    fs::create_dir_all(dir.join("units")).expect("dirs");
    fs::write(dir.join("kbf"), commit).expect("kbf");
    fs::write(dir.join("units/a.service"), "[Unit]\n").expect("unit");
    symlink(exec.at("/etc/escape"), dir.join("escape")).expect("link");
    let t = UNIX_EPOCH + Duration::from_secs(secs);
    fs::File::open(&dir)
        .expect("open")
        .set_modified(t)
        .expect("mtime");
}

fn releases_left(exec: &ScratchHost) -> Vec<String> {
    // Nothing yet when the directory was never made (a dry run makes none).
    let mut names: Vec<String> = fs::read_dir(exec.at("/var/lib/kbf-installer/releases"))
        .map(|d| {
            d.map(|i| i.expect("entry").file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Catches a gc that removes a release still in use: the state's current, previous or
/// staged one, or (with a state that does not name them) the one a link points to;
/// that keeps the oldest instead of the newest under `--keep`; that follows a link out
/// of a release (here into `/etc/escape`); or that removes an entry of `releases/` that
/// is not a release.
///
/// Catches: gc without the state's current (`[state.previous, state.staged]`),
/// and without the state's previous (`[state.current, state.staged]`).
#[test]
fn gc_keeps_current_previous_staged_and_the_newest() {
    let (mut exec, root) = host("gc");
    fs::create_dir_all(exec.at("/etc/escape")).expect("escape");
    fs::write(exec.at("/etc/escape/keep"), "x").expect("escape");
    for (i, commit) in [A, B, C, D, E, S].into_iter().enumerate() {
        seed_release(&exec, commit, 1_000 + i as u64);
    }
    let releases = exec.at("/var/lib/kbf-installer/releases");
    fs::write(releases.join("notes.txt"), "x").expect("stray");
    fs::create_dir(releases.join("tmp")).expect("stray");
    fs::write(releases.join(D.replace('d', "9")), "x").expect("a file named as a commit");
    let state = State {
        current: Some(c(A)),
        previous: Some(c(B)),
        staged: Some(c(S)),
        ..State::default()
    };
    fs::create_dir_all(exec.at("/var/lib/kbf-installer")).expect("state dir");
    Locked::acquire(&root)
        .expect("lock")
        .write(&state)
        .expect("write");
    // No links: only the state names what is in use. One other is asked for; a host keeps
    // two (the floor), so only the oldest of C, D, E goes.
    let out = gc_on(&mut exec, &root, 1).expect("gc");
    assert!(
        out.contains(&format!(
            "keeping: {B} {A} {S} and the 2 newest others; removing: {C}\n"
        )),
        "{out}"
    );
    assert!(out.contains("left alone (not a release): "), "{out}");
    let mut want = vec![A, B, D, E, S, "notes.txt", "tmp"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    want.push(D.replace('d', "9"));
    want.sort();
    assert_eq!(releases_left(&exec), want);
    assert_eq!(
        fs::read_to_string(exec.at("/etc/escape/keep")).expect("kept"),
        "x"
    );

    // A state that names nothing: the links keep what they point to.
    Locked::acquire(&root)
        .expect("lock")
        .write(&State::default())
        .expect("write");
    // The links point at the two oldest, which the newest-two rule would remove.
    symlink(A, exec.at("/var/lib/kbf-installer/releases/current")).expect("link");
    symlink(B, exec.at("/var/lib/kbf-installer/releases/previous")).expect("link");
    gc_on(&mut exec, &root, 0).expect("gc");
    let mut want = vec![A, B, E, S, "current", "previous", "notes.txt", "tmp"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    want.push(D.replace('d', "9"));
    want.sort();
    assert_eq!(releases_left(&exec), want);
}

/// Catches a verb that moves links under a pending commit-confirm deploy, which owns
/// them until it is confirmed or rolled back.
#[test]
fn every_verb_refuses_while_a_deploy_is_pending() {
    let (mut exec, root) = host("pending");
    let state = State {
        pending_confirm: Some(PendingConfirm {
            commit: c(C),
            previous: None,
            activation: "1-1".to_owned(),
            deadline_wall: Some(1),
            deadline_mono_s: None,
            starts_left: None,
        }),
        ..State::default()
    };
    fs::create_dir_all(exec.at("/var/lib/kbf-installer")).expect("state dir");
    Locked::acquire(&root)
        .expect("lock")
        .write(&state)
        .expect("write");
    let want =
        format!("a commit-confirm deploy of {C} is pending: confirm it or let it roll back first");
    let from = source("pending", "A", None);
    let errs = [
        install_on(&mut exec, &root, A, &from, Mode::DryRun).expect_err("install"),
        rollback_on(&mut exec, &root, Mode::Apply).expect_err("rollback"),
    ];
    for err in errs {
        assert_eq!(err.to_string(), want);
    }
}

/// Catches a `gc` that removes anything while a swap is on trial: the release on trial, the
/// one it came from (S22), a release a process runs, and every other release stay, and the
/// output says why nothing was removed.
///
/// Catches: the check for `pending_confirm` removed from `gc` (the surplus is pruned and
/// `releases/` shrinks).
#[test]
fn gc_removes_nothing_while_the_swap_is_on_trial() {
    let (mut exec, root) = host("gc-pending");
    let (p, q) = ("1".repeat(40), "2".repeat(40));
    for r in [A, B, C, &p, &q] {
        fs::create_dir_all(exec.at(&format!("/var/lib/kbf-installer/releases/{r}"))).expect("dir");
    }
    let state = State {
        current: Some(c(C)),
        pending_confirm: Some(PendingConfirm {
            commit: c(C),
            previous: Some(c(&p)),
            activation: "1-1".to_owned(),
            deadline_wall: Some(1),
            deadline_mono_s: None,
            starts_left: None,
        }),
        ..State::default()
    };
    Locked::acquire(&root)
        .expect("lock")
        .write(&state)
        .expect("write");
    let running: BTreeSet<_> = [c(&q)].into();
    let mut out = Vec::new();
    gc(&mut exec, &root, 2, &running, Mode::Apply, &mut out).expect("gc");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        format!("removing nothing: {C} is on trial (confirm it or let it go back first)\n")
    );
    let mut want = vec![A.to_owned(), B.to_owned(), C.to_owned(), p, q];
    want.sort();
    assert_eq!(releases_left(&exec), want);
}

/// Catches a unit name that is not UTF-8 being used (it could not be named in a plan
/// line): the check refuses it, whatever the filesystem can hold.
#[test]
fn a_name_that_is_not_utf8_is_refused() {
    let full = Path::new("/src/units").join(OsStr::from_bytes(b"bad\xff.service"));
    let name = full.file_name().expect("a name");
    let err = utf8(name, &full).expect_err("not utf-8");
    assert_eq!(err.to_string(), format!("{full:?} is not a UTF-8 name"));
    assert_eq!(
        utf8(OsStr::new("a.service"), &full).expect("utf-8"),
        "a.service"
    );
}

/// Catches errors passed over or misnamed: a source without `kbf`, with `units` a file,
/// with a unit whose name is not UTF-8 (it could not be named in a plan line), a state
/// file that cannot be parsed, a `current` that is a directory, and a layout directory
/// that cannot be read.
#[test]
fn errors_name_what_is_wrong() {
    let (mut exec, root) = host("errors");
    let empty = scratch("deploy-release-src-empty");
    let err = install_on(&mut exec, &root, A, &empty, Mode::DryRun).expect_err("no kbf");
    assert!(
        err.to_string()
            .starts_with(&format!("cannot read {}", empty.join("kbf").display()))
    );
    let units_file = source("errors-units", "A", None);
    fs::write(units_file.join("units"), "x").expect("seed");
    let err = install_on(&mut exec, &root, A, &units_file, Mode::DryRun).expect_err("units");
    assert!(err.to_string().starts_with(&format!(
        "cannot read {}",
        units_file.join("units").display()
    )));
    // APFS refuses to create a name that is not UTF-8 (EILSEQ), so only Linux can seed one;
    // `a_name_that_is_not_utf8_is_refused` pins the check itself on every platform.
    if cfg!(target_os = "linux") {
        let odd = source("errors-odd", "A", Some(&[]));
        let name = OsStr::from_bytes(b"bad\xff.service");
        fs::write(odd.join("units").join(name), "x").expect("seed");
        let err = install_on(&mut exec, &root, A, &odd, Mode::DryRun).expect_err("not utf-8");
        assert_eq!(
            err.to_string(),
            format!("{:?} is not a UTF-8 name", odd.join("units").join(name))
        );
    }

    let ok = source("errors-ok", "A", None);
    fs::create_dir_all(exec.at("/var/lib/kbf-installer/releases/current")).expect("a directory");
    let err = install_on(&mut exec, &root, A, &ok, Mode::DryRun).expect_err("not a link");
    assert_eq!(
        err.to_string(),
        "refusing to replace /var/lib/kbf-installer/releases/current with a symbolic link: it is not one"
    );
    fs::create_dir_all(exec.at("/var/lib/kbf-installer")).expect("dir");
    fs::write(exec.at("/var/lib/kbf-installer/state.json"), "{").expect("seed");
    let err = rollback_on(&mut exec, &root, Mode::DryRun).expect_err("broken state");
    assert!(
        err.to_string()
            .contains("the state is unusable and nothing was changed"),
        "{err}"
    );

    let (mut exec, root) = host("errors-unreadable");
    fs::create_dir_all(exec.at("/var/lib/kbf-installer")).expect("seed");
    fs::write(exec.at("/var/lib/kbf-installer/releases"), "a file").expect("seed");
    let err = gc_on(&mut exec, &root, 0).expect_err("unreadable");
    // The sweep of stage directories lists `releases/` first and cannot.
    let want = format!(
        "cannot read {}: ",
        exec.at("/var/lib/kbf-installer/releases").display()
    );
    assert!(err.to_string().starts_with(&want), "{err}");
}

/// A scratch Mac: the directories every macOS has above the layout (`/var/run`,
/// `/var/db`, `/usr`) and not `/usr/local`, which a fresh one lacks.
fn mac(name: &str) -> (ScratchHost, Layout) {
    let exec = ScratchHost::new(&format!("deploy-release-mac-{name}"));
    for sub in ["var/run", "var/db", "usr"] {
        fs::create_dir_all(exec.root.join(sub)).expect("seed");
    }
    let layout = Layout::new(
        Root::new(&exec.root).expect("absolute"),
        crate::layout::Platform::MacOs,
    );
    (exec, layout)
}

/// A release for a Mac: `kbf` holding `binary`.
fn mac_source(name: &str, binary: &str) -> PathBuf {
    let dir = scratch(&format!("deploy-release-src-{name}"));
    fs::write(dir.join("kbf"), binary).expect("binary");
    dir
}

/// Catches a release of two binaries not being copied whole: both come out 0755 with their
/// bytes, the first is named in the release line as a release of one always was, and the
/// second has a line of its own with its size and sha256.
#[test]
fn a_release_of_two_binaries_is_installed_whole_and_reported() {
    let (mut exec, root) = host("two-binaries");
    let from = source("two-binaries-src", "kbf A", None);
    fs::write(from.join("kbf-daemon"), "daemon A").expect("daemon");
    let out = install_on(&mut exec, &root, A, &from, Mode::Apply).expect("install");
    for (file, text) in [("kbf", "kbf A"), ("kbf-daemon", "daemon A")] {
        let path = format!("/var/lib/kbf-installer/releases/{A}/{file}");
        assert_eq!(fs::read_to_string(exec.at(&path)).expect(file), text);
        assert_eq!(mode_of(&exec, &path), 0o755);
    }
    let r = format!("/var/lib/kbf-installer/releases/{A}");
    assert!(
        out.starts_with(&format!(
            "release {A}: {r} (5 bytes, sha256 {}), 0 unit(s)\nalso kbf-daemon (8 bytes, sha256 {})\n",
            sha256(b"kbf A"),
            sha256(b"daemon A")
        )),
        "{out}"
    );

    // The release is whole or not there: a second binary the directory lacks is a
    // different release, and is refused.
    let (mut exec2, root2) = host("two-binaries-differs");
    let only_kbf = source("two-binaries-only", "kbf A", None);
    install_on(&mut exec2, &root2, A, &only_kbf, Mode::Apply).expect("install");
    let err = install_on(&mut exec2, &root2, A, &from, Mode::Apply).expect_err("a new binary");
    assert!(
        matches!(&err, ReleaseError::Differs { path } if path.as_str().ends_with("/kbf-daemon")),
        "{err}"
    );
}

/// Catches a source with only `kbf` (all a Mac release has) or only `kbf-daemon` being
/// refused, and a source with neither being refused naming `kbf` as before.
#[test]
fn a_source_needs_one_binary_and_either_will_do() {
    let only = mac_source("kbf-only", "kbf A");
    let s = Source::read(&only).expect("reads");
    assert_eq!(s.binaries, [("kbf", b"kbf A".to_vec())]);
    let daemon = scratch("deploy-release-src-daemon-only");
    fs::write(daemon.join("kbf-daemon"), "daemon A").expect("daemon");
    let s = Source::read(&daemon).expect("reads");
    assert_eq!(s.binaries, [("kbf-daemon", b"daemon A".to_vec())]);
    let none = scratch("deploy-release-src-none");
    let err = Source::read(&none).expect_err("nothing");
    assert!(
        err.to_string()
            .starts_with(&format!("cannot read {}", none.join("kbf").display())),
        "{err}"
    );
    // A binary that cannot be read is an error, not a missing one.
    let unreadable = scratch("deploy-release-src-unreadable");
    fs::create_dir(unreadable.join("kbf")).expect("a directory");
    let err = Source::read(&unreadable).expect_err("a directory");
    assert!(err.to_string().starts_with("cannot read "), "{err}");
}

/// Catches the macOS layout not being used: an install on a Mac puts the state under
/// `/var/db/kbf-installer` and takes the lock at `/var/run/kbf-installer/lock`, never at
/// `/var/lib` or `/run`; it makes the `/usr/local` a fresh Mac lacks (root's, 0755) and
/// the state directory; the links point at the release; the next-steps line names the
/// daemon and `launchctl`, not `kbf` and `systemctl`.
///
/// Catches: `Locked::acquire` taking the Linux lock (`/run` is not there).
#[test]
fn a_mac_install_uses_the_mac_layout_and_makes_its_missing_parents() {
    let (mut exec, layout) = mac("flow");
    let a = mac_source("mac-flow-a", "daemon A");
    let b = mac_source("mac-flow-b", "daemon B");
    assert!(!exec.at("/usr/local").exists());
    let out = install_on(&mut exec, &layout, A, &a, Mode::Apply).expect("install A");
    install_on(&mut exec, &layout, B, &b, Mode::Apply).expect("install B");
    assert!(
        out.contains("state: written to /var/db/kbf-installer/state.json\n"),
        "{out}"
    );
    assert!(
        out.contains("restart the units that run /usr/local/kbf-installer/releases/current/kbf (launchctl kickstart -k system/<label>)"),
        "{out}"
    );
    assert!(exec.at("/var/db/kbf-installer/state.json").is_file());
    assert!(exec.at("/var/run/kbf-installer/lock").is_file());
    assert!(!exec.at("/var/lib").exists() && !exec.at("/run").exists());
    for dir in [
        "/usr/local",
        "/var/db/kbf-installer",
        "/usr/local/kbf-installer",
    ] {
        assert_eq!(mode_of(&exec, dir), 0o755, "{dir}");
    }
    assert_eq!(link(&exec, "current"), Some(B.to_owned()));
    assert_eq!(link(&exec, "previous"), Some(A.to_owned()));
    assert_eq!(state_of(&layout).current, Some(c(B)));
    assert!(
        !exec
            .at(&format!("/usr/local/kbf-installer/releases/{A}/kbf-daemon"))
            .exists()
    );

    // Rolling back finds the previous release by its daemon.
    let out = rollback_on(&mut exec, &layout, Mode::Apply).expect("rollback");
    assert!(
        out.contains(&format!("links: current {B} -> {A}, previous {A} -> none")),
        "{out}"
    );
    assert_eq!(link(&exec, "current"), Some(A.to_owned()));
}

/// Catches `/usr/local` being touched when the host has one: a Mac (or any host) whose
/// `/usr/local` is 0775 keeps that mode (its owner is the host's business; the trust
/// check refuses what others can write), the plan has no step for it, and a host without
/// one gets `mkdir /usr/local` as the first step.
///
/// Catches: `/usr/local` always planned (the 0775 becomes 0755).
#[test]
fn usr_local_is_made_when_missing_and_left_alone_when_there() {
    // The installer tree is under `/usr/local` on a Mac only.
    let (mut exec, root) = mac("usr-local");
    fs::create_dir(exec.at("/usr/local")).expect("seed");
    fs::set_permissions(exec.at("/usr/local"), fs::Permissions::from_mode(0o775)).expect("mode");
    let from = mac_source("usr-local-src", "daemon A");
    let out = install_on(&mut exec, &root, A, &from, Mode::Apply).expect("install");
    assert!(!out.contains("/usr/local ("), "{out}");
    assert_eq!(mode_of(&exec, "/usr/local"), 0o775);

    let (mut exec, root) = mac("usr-local-missing");
    let out = install_on(&mut exec, &root, A, &from, Mode::DryRun).expect("dry run");
    let first_step = out
        .lines()
        .find(|l| l.starts_with("DRY-RUN"))
        .expect("a step");
    let owner = me();
    assert_eq!(
        first_step,
        format!(
            "DRY-RUN: mkdir /usr/local (0755, owner {}:{})",
            owner.uid, owner.gid
        )
    );
    install_on(&mut exec, &root, A, &from, Mode::Apply).expect("install");
    assert_eq!(mode_of(&exec, "/usr/local"), 0o755);

    // A Linux host's tree is not under it: no step for `/usr/local` at all.
    let (mut exec, root) = host("usr-local-linux");
    let linux = source("usr-local-linux-src", "kbf A", None);
    let out = install_on(&mut exec, &root, A, &linux, Mode::DryRun).expect("dry run");
    assert!(!out.contains("/usr/local"), "{out}");
}

/// Catches `stage` doing what `install` does: it must copy and check the release, record
/// it as the state's `staged` one (and change nothing else of the state), and move no link.
/// It proceeds while a deploy is pending, keeping that deploy's record; a dry run prints
/// the plan and changes nothing; and `gc` keeps the staged release.
///
/// Catches: `stage` switching the links; `stage` refusing while pending; the
/// staged commit not recorded (`gc` then removes the release).
#[test]
fn stage_copies_and_records_the_release_and_moves_no_link() {
    let (mut exec, root) = host("stage");
    let a = source("stage-a", "kbf A", Some(UNITS));
    let b = source("stage-b", "kbf B", None);
    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    Locked::acquire(&root)
        .expect("lock")
        .write(&State {
            pending_confirm: Some(PendingConfirm {
                commit: c(A),
                previous: None,
                activation: "1-1".to_owned(),
                deadline_wall: Some(1),
                deadline_mono_s: None,
                starts_left: None,
            }),
            ..state_of(&root)
        })
        .expect("pending");

    let before = tree(&exec.root);
    let mut out = Vec::new();
    stage(
        &mut exec,
        &root,
        me(),
        &Install {
            commit: c(B),
            from: b.clone(),
        },
        Mode::DryRun,
        &mut out,
    )
    .expect("dry run");
    assert_eq!(tree(&exec.root), before);
    assert!(
        String::from_utf8(out)
            .expect("utf-8")
            .contains(&format!("release {B}:"))
    );

    let mut out = Vec::new();
    stage(
        &mut exec,
        &root,
        me(),
        &Install {
            commit: c(B),
            from: b,
        },
        Mode::Apply,
        &mut out,
    )
    .expect("stage while pending");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(out.ends_with(&format!("state: {B} staged\n")), "{out}");
    assert_eq!(
        fs::read_to_string(exec.at(&format!("/var/lib/kbf-installer/releases/{B}/kbf")))
            .expect("copied"),
        "kbf B"
    );
    let s = state_of(&root);
    assert_eq!((s.current, s.staged), (Some(c(A)), Some(c(B))));
    assert_eq!(s.pending_confirm.map(|p| p.commit), Some(c(A)));
    assert_eq!(link(&exec, "current"), Some(A.to_owned()));
    assert_eq!(link(&exec, "previous"), None);

    // The staged release survives a gc that removes everything else.
    Locked::acquire(&root)
        .expect("lock")
        .write(&State {
            pending_confirm: None,
            ..state_of(&root)
        })
        .expect("clear");
    gc_on(&mut exec, &root, 0).expect("gc");
    assert!(
        exec.at(&format!("/var/lib/kbf-installer/releases/{B}/kbf"))
            .exists()
    );
}

/// Catches a release that `install` makes current staying staged as well: with B staged, the
/// install of B makes it current (A previous) and nothing is staged; with C staged, the
/// install of B leaves C staged (another release's decision is not this one's).
///
/// Catches: `staged` kept by the switch; `staged` cleared whatever it names.
#[test]
fn installing_the_staged_release_makes_it_current_and_no_longer_staged() {
    let stage_on = |exec: &mut ScratchHost, root: &Layout, commit: &str, from: &Path| {
        let a = Install {
            commit: c(commit),
            from: from.to_owned(),
        };
        stage(exec, root, me(), &a, Mode::Apply, &mut Vec::new()).expect("stage");
    };
    for (staged, want) in [(B, None), (C, Some(c(C)))] {
        let (mut exec, root) = host(&format!("install-staged-{staged}"));
        let a = source(&format!("install-staged-a-{staged}"), "kbf A", None);
        let b = source(&format!("install-staged-b-{staged}"), "kbf B", None);
        let other = source(&format!("install-staged-c-{staged}"), "kbf C", None);
        install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
        stage_on(
            &mut exec,
            &root,
            staged,
            if staged == B { &b } else { &other },
        );
        assert_eq!(state_of(&root).staged, Some(c(staged)));

        install_on(&mut exec, &root, B, &b, Mode::Apply).expect("install B");
        let s = state_of(&root);
        assert_eq!(
            (s.current, s.previous),
            (Some(c(B)), Some(c(A))),
            "{staged}"
        );
        assert_eq!(s.staged, want, "{staged}");
    }
}

/// Catches a stage that replaces a release or leaves a half copy: a release directory with
/// other bytes is refused and keeps its bytes, and a source with nothing to copy is
/// refused before the lock.
#[test]
fn stage_never_replaces_a_release() {
    let (mut exec, root) = host("stage-differs");
    let a = source("stage-differs-a", "kbf A", None);
    let other = source("stage-differs-b", "kbf other", None);
    let install = |from: &Path| Install {
        commit: c(A),
        from: from.to_owned(),
    };
    stage(
        &mut exec,
        &root,
        me(),
        &install(&a),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect("stage");
    let err = stage(
        &mut exec,
        &root,
        me(),
        &install(&other),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("other bytes");
    assert!(matches!(err, ReleaseError::Differs { .. }), "{err}");
    assert_eq!(
        fs::read_to_string(exec.at(&format!("/var/lib/kbf-installer/releases/{A}/kbf")))
            .expect("kept"),
        "kbf A"
    );
    let empty = scratch("deploy-release-src-stage-empty");
    let err = stage(
        &mut exec,
        &root,
        me(),
        &install(&empty),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("no binary");
    assert!(err.to_string().starts_with("cannot read "), "{err}");
}

/// A host whose disk fills: it applies `left` file writes, and the next one fails with
/// ENOSPC. Everything else is the scratch host's.
struct DiskFull<'a> {
    host: &'a mut ScratchHost,
    left: usize,
}

impl Exec for DiskFull<'_> {
    fn euid(&self) -> u32 {
        self.host.euid()
    }

    fn inspect(&self, path: &HostPath) -> std::io::Result<Option<Entry>> {
        self.host.inspect(path)
    }

    fn stat(&self, path: &HostPath) -> std::io::Result<Option<Meta>> {
        self.host.stat(path)
    }

    fn query(&self, cmd: &Cmd) -> std::io::Result<Output> {
        self.host.query(cmd)
    }

    fn http_status(&self, url: &HttpUrl) -> std::io::Result<u16> {
        self.host.http_status(url)
    }

    fn apply(&mut self, step: &Step) -> std::io::Result<()> {
        if matches!(step, Step::WriteFile { .. }) {
            if self.left == 0 {
                return Err(std::io::Error::from_raw_os_error(28));
            }
            self.left -= 1;
        }
        self.host.apply(step)
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// Catches a stage that fills the disk and leaves a half-written release directory:
/// the retry would then find the directory there with other content and refuse it for
/// ever, and `gc` would keep a broken release. After the failure nothing of the release
/// is there and the state is unchanged; the retry on a healthy disk succeeds. A release
/// that was already installed whole is never removed by a failed repeat of its stage.
///
/// Catches: the partial directory left in place; the directory removed whether
/// or not the copy made it (the installed release goes with a failed repeat).
#[test]
fn a_stage_that_fills_the_disk_leaves_no_partial_release_and_can_be_retried() {
    let (mut exec, root) = host("disk-full");
    let from = source("disk-full", "kbf B", Some(UNITS));
    fs::write(from.join("kbf-daemon"), "daemon B").expect("a second binary");
    let dir = exec.at(&format!("/var/lib/kbf-installer/releases/{B}"));
    let want = Install {
        commit: c(B),
        from: from.clone(),
    };

    // The first binary is written, the second is not.
    let err = stage(
        &mut DiskFull {
            host: &mut exec,
            left: 1,
        },
        &root,
        me(),
        &want,
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("the disk is full");
    assert!(err.to_string().contains("os error 28"), "{err}");
    assert!(!dir.exists(), "{:?}", tree(&exec.root));
    assert!(
        state::read_unlocked(&root).expect("read").is_none() || state_of(&root).staged.is_none()
    );

    // The retry, on a disk with room (the same host type, so that one copy of the code
    // under test runs on both its paths).
    stage(
        &mut DiskFull {
            host: &mut exec,
            left: 100,
        },
        &root,
        me(),
        &want,
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect("retry");
    assert_eq!(fs::read_to_string(dir.join("kbf")).expect("kbf"), "kbf B");
    assert_eq!(
        fs::read_to_string(dir.join("kbf-daemon")).expect("daemon"),
        "daemon B"
    );
    assert_eq!(state_of(&root).staged, Some(c(B)));

    // A repeat of a stage that was whole, failing on a full disk, removes nothing. (The
    // binary's mode is changed so that the repeat has a file to write again.)
    fs::set_permissions(dir.join("kbf"), fs::Permissions::from_mode(0o600)).expect("chmod");
    stage(
        &mut DiskFull {
            host: &mut exec,
            left: 0,
        },
        &root,
        me(),
        &want,
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("the disk is full again");
    assert_eq!(fs::read_to_string(dir.join("kbf")).expect("kbf"), "kbf B");
    assert!(dir.join("units").join("kbf.slice").exists());
}

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

#[path = "gc_retain_tests.rs"]
mod retention;

#[path = "fixed_paths_tests.rs"]
mod fixed_paths;
