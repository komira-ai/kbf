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

#[path = "release_stage_tests.rs"]
mod stage;

#[path = "release_atomic_tests.rs"]
mod atomic;

#[path = "gc_retain_tests.rs"]
mod retention;

#[path = "fixed_paths_tests.rs"]
mod fixed_paths;
