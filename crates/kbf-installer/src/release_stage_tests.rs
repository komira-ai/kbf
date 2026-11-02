//! Installs onto a Mac layout, the `stage` verb that copies a release without moving a link, and a stage that meets a full disk.

use super::*;

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
