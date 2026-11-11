use std::ffi::CString;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use super::*;
use crate::in_child::{dumpable, in_child};

/// Steps compare by what they print: a held descriptor by its number.
impl PartialEq for Step {
    fn eq(&self, other: &Self) -> bool {
        format!("{self:?}") == format!("{other:?}")
    }
}

const EPERM: u8 = 1;

fn temp(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(std::env::temp_dir(), &format!("kbf-task-child-{name}"))
}

fn c(path: &Path) -> CString {
    CString::new(path.to_str().expect("utf-8")).expect("no NUL")
}

fn outcome(step: u8, errno: u8) -> Outcome {
    Outcome { step, errno }
}

/// Catches the steps or their order changing: the child must be dumpable to write its own
/// `/proc` files, which need the namespaces to exist first, and must be non-dumpable again
/// before `lo`, the mounts and the filter. The network namespace is made only for a Command
/// denied the network, and none of the user ones for root. The mounts come last, as given.
#[test]
fn the_plan_is_these_steps_in_this_order() {
    let mounts = || {
        vec![
            (PRIVATE, Step::Private),
            (CHDIR, Step::Chdir(c(Path::new("/")))),
        ]
    };
    let user = |net| {
        vec![
            (DUMPABLE, Step::Dumpable(true)),
            (
                UNSHARE,
                Step::Unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID | net),
            ),
            (
                SETGROUPS,
                Step::Write(c(Path::new("/proc/self/setgroups")), b"deny".to_vec()),
            ),
            (
                UID_MAP,
                Step::Write(c(Path::new("/proc/self/uid_map")), b"1000 1000 1".to_vec()),
            ),
            (
                GID_MAP,
                Step::Write(c(Path::new("/proc/self/gid_map")), b"2000 2000 1".to_vec()),
            ),
            (DUMPABLE, Step::Dumpable(false)),
        ]
    };
    let (denied, _report, _) = plan(1000, 2000, true, true, mounts(), make_pipe).unwrap();
    let mut want = user(libc::CLONE_NEWNET);
    want.push((LOOPBACK, Step::Loopback));
    want.extend(mounts());
    assert_eq!(denied.steps(), want);

    let (allowed, _report, _) = plan(1000, 2000, false, true, mounts(), make_pipe).unwrap();
    let mut want = user(0);
    want.extend(mounts());
    assert_eq!(allowed.steps(), want);

    for denied in [true, false] {
        let (root, _report, _) = plan(0, 0, denied, true, mounts(), make_pipe).unwrap();
        let mut want = vec![(
            UNSHARE,
            Step::Unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID),
        )];
        want.extend(mounts());
        assert_eq!(root.steps(), want, "denied: {denied}");
    }
}

/// Catches a step the log cannot name, two steps under one number, or a number the parent
/// reads as a step it is not.
#[test]
fn every_step_number_has_one_name() {
    let names: Vec<&str> = STEPS.iter().map(|(_, name)| *name).collect();
    for (number, name) in STEPS {
        assert_eq!(step_name(number), Some(name));
        assert_eq!(names.iter().filter(|n| **n == name).count(), 1, "{name}");
    }
    for unknown in [OK, 6, 16, 200, u8::MAX] {
        assert_eq!(step_name(unknown), None, "{unknown}");
    }
    // Every mount step is numbered past the network namespace's steps.
    for step in [PRIVATE, HOLD, COVER, BIND, MKDIR, KEEP, SEAL, CHDIR] {
        assert!(step >= FIRST_MOUNT_STEP, "{step}");
    }
    for step in [UNSHARE, SETGROUPS, UID_MAP, GID_MAP, LOOPBACK, DUMPABLE] {
        assert!(step < FIRST_MOUNT_STEP, "{step}");
    }
}

/// Catches the parent reading a report as the wrong layers: for a Command denied the network,
/// a clean report or one that failed at a mount step means the namespace layer; a failed
/// namespace step is named with its errno; root has its own line; and a Command allowed the
/// network is open whatever the child said.
#[test]
fn a_report_is_read_as_the_layers_it_means() {
    assert_eq!(layers(true, false, Some((OK, 0))), Layers::SeccompAndNetns);
    assert_eq!(
        layers(true, false, Some((PRIVATE, 1))),
        Layers::SeccompAndNetns
    );
    assert_eq!(
        layers(true, false, Some((CHDIR, 2))),
        Layers::SeccompAndNetns
    );
    assert_eq!(
        layers(true, false, Some((UID_MAP, 13))),
        Layers::SeccompOnly {
            step: "uid_map",
            errno: 13
        }
    );
    assert_eq!(
        layers(true, false, Some((DUMPABLE, 1))),
        Layers::SeccompOnly {
            step: "dumpable",
            errno: 1
        }
    );
    assert_eq!(layers(true, false, Some((6, 1))), Layers::Unreported);
    assert_eq!(layers(true, false, None), Layers::Unreported);
    assert_eq!(
        layers(true, true, Some((OK, 0))),
        Layers::SeccompOnlyForRoot
    );
    assert_eq!(
        layers(true, true, Some((UNSHARE, 1))),
        Layers::SeccompOnlyForRoot
    );
    assert_eq!(layers(true, true, None), Layers::Unreported);
    for said in [None, Some((OK, 0)), Some((UNSHARE, 1))] {
        assert_eq!(layers(false, false, said), Layers::Open);
        assert_eq!(layers(false, true, said), Layers::Open);
    }
}

/// Catches the sandbox reported made when a step failed, or a failed step not named.
#[test]
fn a_report_is_read_as_the_sandbox_it_means() {
    assert_eq!(sandbox(Some((OK, 0))), Sandbox::Made);
    assert_eq!(
        sandbox(Some((UID_MAP, 1))),
        Sandbox::Incomplete {
            step: "uid_map",
            errno: 1
        }
    );
    assert_eq!(
        sandbox(Some((SEAL, 30))),
        Sandbox::Incomplete {
            step: "seal",
            errno: 30
        }
    );
    assert_eq!(sandbox(Some((6, 1))), Sandbox::Unreported);
    assert_eq!(sandbox(None), Sandbox::Unreported);
}

/// Catches the two bytes the child writes being read as anything else by the parent, or a
/// child that never wrote being read as one that did.
#[test]
fn what_the_child_wrote_is_what_the_parent_reads() {
    let say = |root: bool, bytes: Option<Outcome>| {
        let uid = if root { 0 } else { 1000 };
        let (plan, report, _) = plan(uid, uid, true, false, Vec::new(), make_pipe).unwrap();
        if let Some(outcome) = bytes {
            plan.tell(outcome);
        }
        drop(plan);
        let (layers, sandbox, start) = report.read();
        assert_eq!(start, Start::Started, "no relay, no start report");
        (layers, sandbox)
    };
    assert_eq!(
        say(false, Some(outcome(OK, 0))),
        (Layers::SeccompAndNetns, Sandbox::Made)
    );
    assert_eq!(
        say(false, Some(outcome(GID_MAP, 13))),
        (
            Layers::SeccompOnly {
                step: "gid_map",
                errno: 13
            },
            Sandbox::Incomplete {
                step: "gid_map",
                errno: 13
            }
        )
    );
    assert_eq!(
        say(false, Some(outcome(COVER, 1))),
        (
            Layers::SeccompAndNetns,
            Sandbox::Incomplete {
                step: "cover",
                errno: 1
            }
        )
    );
    assert_eq!(
        say(true, Some(outcome(OK, 0))),
        (Layers::SeccompOnlyForRoot, Sandbox::Made)
    );
    assert_eq!(say(false, None), (Layers::Unreported, Sandbox::Unreported));
}

#[test]
fn the_dumpable_step_sets_the_flag() {
    assert_eq!(
        in_child(|| {
            run_step(&mut Step::Dumpable(true)).unwrap();
            let on = dumpable();
            run_step(&mut Step::Dumpable(false)).unwrap();
            on * 10 + dumpable()
        }),
        10
    );
}

/// The `unshare` step ends in one of the ways a host can answer, and either way the child lives
/// to report it.
#[test]
fn the_unshare_step_succeeds_or_fails_with_an_errno() {
    let code = in_child(|| {
        match run_step(&mut Step::Unshare(
            libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWNET,
        )) {
            Ok(()) => 0,
            Err(e) => code(&e).into(),
        }
    });
    assert!(
        code == 0 || code == 1 || code == libc::EINVAL || code == libc::ENOSPC,
        "{code}"
    );
}

#[test]
fn a_map_file_is_written_in_one_write_or_the_errno_is_returned() {
    let dir = temp("write");
    let file = dir.join("file");
    let path = c(&file);
    std::fs::write(&file, b"").unwrap();
    run_step(&mut Step::Write(path.clone(), b"deny".to_vec())).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"deny");
    std::fs::remove_file(&file).unwrap();
    let missing = write_file(&path, b"x").unwrap_err();
    assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
    let full = CString::new("/dev/full").unwrap();
    let error = write_file(&full, b"x").unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
    assert_eq!(code(&error), u8::try_from(libc::ENOSPC).unwrap());
}

/// Catches the steps not stopping at the first failure, or a later step running after it.
/// (`unshare(0)` makes no namespace and succeeds, so it stands for a successful `unshare`.)
#[test]
fn steps_run_in_order_up_to_the_first_that_fails() {
    let dir = temp("steps");
    let (a, b) = (dir.join("a"), dir.join("b"));
    std::fs::write(&a, b"").unwrap();
    assert_eq!(run_steps(&mut []), outcome(OK, 0));
    let ok = (3, Step::Write(c(&a), b"1".to_vec()));
    assert_eq!(run_steps(&mut [ok]), outcome(OK, 0));
    let mut steps = [
        (3, Step::Write(c(&a), b"2".to_vec())),
        (4, Step::Write(c(&b), b"x".to_vec())),
        (5, Step::Write(c(&a), b"3".to_vec())),
    ];
    let enoent = u8::try_from(libc::ENOENT).unwrap();
    assert_eq!(run_steps(&mut steps), outcome(4, enoent));
    assert_eq!(
        std::fs::read(&a).unwrap(),
        b"2",
        "the step after the failure ran"
    );
    let mut unshared = [
        (UNSHARE, Step::Unshare(0)),
        (MKDIR, Step::Mkdir(c(&dir.join("no/such")))),
    ];
    assert_eq!(run_steps(&mut unshared), outcome(MKDIR, enoent));
    let mut before = [
        (MKDIR, Step::Mkdir(c(&dir.join("no/such")))),
        (UNSHARE, Step::Unshare(0)),
    ];
    assert_eq!(run_steps(&mut before), outcome(MKDIR, enoent));
    assert_eq!(
        run_steps(&mut [(UNSHARE, Step::Unshare(0))]),
        outcome(OK, 0)
    );
}

/// `lo` is raised where the process may, and refused with the errno where it may not.
#[test]
fn the_loopback_step_raises_lo_or_returns_the_errno() {
    match run_step(&mut Step::Loopback).and(raise_loopback()) {
        Ok(()) => {}
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EPERM)),
    }
}

/// Catches a directory on the way to a mount point not being made, or one that exists failing
/// the step.
#[test]
fn the_mkdir_step_makes_a_directory_and_accepts_one_that_exists() {
    let dir = temp("mkdir");
    let made = dir.join("made");
    run_step(&mut Step::Mkdir(c(&made))).unwrap();
    assert!(made.is_dir());
    run_step(&mut Step::Mkdir(c(&made))).unwrap();
    let error = run_step(&mut Step::Mkdir(c(&dir.join("no/such")))).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
}

/// Catches the working directory not entered again (or entered where the step did not say), and
/// a directory that is gone not refused.
#[test]
fn the_chdir_step_enters_the_directory() {
    let dir = temp("chdir");
    let real = std::fs::canonicalize(&*dir).unwrap();
    let code = in_child(|| {
        run_step(&mut Step::Chdir(c(&real))).unwrap();
        i32::from(std::env::current_dir().unwrap() != real)
    });
    assert_eq!(code, 0);
    let error = run_step(&mut Step::Chdir(c(&dir.join("gone")))).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
}

/// A descriptor of `/dev/null`, whose number a hold step is to take.
fn reserved() -> OwnedFd {
    OwnedFd::from(std::fs::File::open("/dev/null").unwrap())
}

/// Catches a bind's source opened through a symlink, not onto the reserved number, or left open
/// across `exec`: the held descriptor is the directory itself, closes on `exec`, and a symlink
/// or an absent directory is refused.
#[test]
fn the_hold_step_opens_the_directory_onto_the_reserved_number() {
    let dir = temp("hold");
    let target = dir.join("target");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, dir.join("link")).unwrap();
    let want = std::fs::metadata(&target).unwrap().ino();
    let fd = reserved();
    let number = fd.as_raw_fd();
    let mut step = Step::Hold {
        dir: c(&target),
        fd,
    };
    let code = in_child(|| {
        run_step(&mut step).unwrap();
        let Step::Hold { fd, .. } = &step else {
            return 3;
        };
        let held = rustix::fs::fstat(fd).unwrap().st_ino;
        let flags = rustix::io::fcntl_getfd(fd).unwrap();
        i32::from(held != want)
            | i32::from(!flags.contains(rustix::io::FdFlags::CLOEXEC)) << 1
            | i32::from(fd.as_raw_fd() != number) << 2
    });
    assert_eq!(code, 0);
    for (refused, errno) in [("link", libc::ELOOP), ("gone", libc::ENOENT)] {
        let error = run_step(&mut Step::Hold {
            dir: c(&dir.join(refused)),
            fd: reserved(),
        })
        .unwrap_err();
        // A symlink under O_PATH | O_DIRECTORY | O_NOFOLLOW is ELOOP or ENOTDIR by kernel.
        let got = error.raw_os_error().unwrap();
        assert!(
            got == errno || (refused == "link" && got == libc::ENOTDIR),
            "{refused}: {error}"
        );
    }
}

/// Catches a mount step that does not do what it says, where the host lets this process make a
/// mount namespace it holds `CAP_SYS_ADMIN` in (root, or a user namespace that keeps its
/// capabilities): a cover hides what was there and is read-only once sealed, a held directory
/// bound under it is visible and writable, and nothing of it is seen outside the child. Where the
/// host refuses, the plan stops at the step it refused, with `EPERM`, and nothing changed.
#[test]
fn the_mount_steps_cover_bind_and_seal_where_the_host_allows() {
    let dir = temp("mounts");
    let real = std::fs::canonicalize(&*dir).unwrap();
    let covered = real.join("covered");
    let kept = real.join("kept");
    std::fs::create_dir(&covered).unwrap();
    std::fs::write(covered.join("secret"), b"grant").unwrap();
    std::fs::create_dir(&kept).unwrap();
    let fd = reserved();
    let from = c(Path::new(&format!("/proc/self/fd/{}", fd.as_raw_fd())));
    let mount_point = covered.join("kept");
    let mounts = vec![
        (PRIVATE, Step::Private),
        (HOLD, Step::Hold { dir: c(&kept), fd }),
        (COVER, Step::Cover(c(&covered))),
        (MKDIR, Step::Mkdir(c(&mount_point))),
        (
            KEEP,
            Step::Bind {
                from,
                to: c(&mount_point),
            },
        ),
        (SEAL, Step::Seal(c(&covered))),
    ];
    let uid = rustix::process::geteuid().as_raw();
    let gid = rustix::process::getegid().as_raw();
    let (mut plan, report, _) = plan(uid, gid, false, false, mounts, make_pipe).unwrap();
    let code = in_child(|| {
        let outcome = plan.apply();
        plan.tell(outcome);
        if outcome.step != OK {
            return 0;
        }
        let hidden = !covered.join("secret").exists();
        let sealed = std::fs::create_dir(covered.join("new"))
            .is_err_and(|e| e.raw_os_error() == Some(libc::EROFS));
        let kept_writable = std::fs::write(mount_point.join("made"), b"x").is_ok();
        i32::from(!hidden) | i32::from(!sealed) << 1 | i32::from(!kept_writable) << 2
    });
    assert_eq!(code, 0);
    match report.read().1 {
        Sandbox::Made => {
            // The write through the bind landed in the kept directory itself.
            assert_eq!(std::fs::read(kept.join("made")).unwrap(), b"x");
        }
        Sandbox::Incomplete { step, errno } => {
            eprintln!("skipped the checks of a made sandbox: this host refuses {step} ({errno})");
            assert!(
                ["unshare", "setgroups", "uid_map", "gid_map", "private"].contains(&step),
                "{step}"
            );
            assert_ne!(errno, 0);
        }
        other => panic!("{other:?}"),
    }
    // Outside the child the cover never existed, nor the mount point made inside it.
    assert_eq!(std::fs::read(covered.join("secret")).unwrap(), b"grant");
    assert!(!mount_point.exists());
}

/// Catches a mount step run outside a namespace of its own changing this host: without
/// `CAP_SYS_ADMIN` each is refused. (Skipped as root, which may mount here.)
#[test]
fn outside_a_namespace_the_mount_steps_are_refused() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root may mount in the test's own namespace");
        return;
    }
    let dir = temp("refused");
    let path = c(&dir);
    for mut step in [
        Step::Private,
        Step::Cover(path.clone()),
        Step::Bind {
            from: path.clone(),
            to: path.clone(),
        },
        Step::Seal(path.clone()),
    ] {
        let error = run_step(&mut step).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(i32::from(EPERM)), "{step:?}");
    }
}

/// Catches the relay's start report read as anything but what it says, read where no relay
/// writes one, or a relay that executed the Command itself (the end of the pipe) read as a
/// failure. The relay writes on its standard input, the second write end.
#[test]
fn the_relays_start_report_is_read_after_the_childs() {
    let read = |start: Option<[u8; 2]>| {
        let (plan, report, stdin) = plan(1000, 1000, true, true, Vec::new(), make_pipe).unwrap();
        let stdin = stdin.expect("a write end for the relay");
        plan.tell(outcome(OK, 0));
        if let Some(bytes) = start {
            rustix::io::write(&stdin, &bytes).unwrap();
        }
        drop((plan, stdin));
        report.read()
    };
    let made = (Layers::SeccompAndNetns, Sandbox::Made);
    assert_eq!(
        read(Some([crate::relay::STARTED, 0])),
        (made.0.clone(), made.1.clone(), Start::Started)
    );
    assert_eq!(
        read(Some([crate::relay::NOT_STARTED, 2])),
        (made.0.clone(), made.1.clone(), Start::NotStarted(2))
    );
    assert_eq!(read(None), (made.0, made.1, Start::Started));
    // A child that never reported: no start report is waited for either.
    let (silent, report, stdin) = plan(1000, 1000, true, true, Vec::new(), make_pipe).unwrap();
    drop((silent, stdin));
    assert_eq!(
        report.read(),
        (Layers::Unreported, Sandbox::Unreported, Start::Started)
    );
    // Without a relay there is no second write end, and the `unshare` makes no PID namespace.
    let (plan, _report, stdin) = plan(1000, 1000, true, false, Vec::new(), make_pipe).unwrap();
    assert!(stdin.is_none());
    assert!(plan.steps().contains(&(
        UNSHARE,
        Step::Unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWNET)
    )));
}

#[test]
fn a_start_report_is_a_start_unless_it_says_otherwise() {
    assert_eq!(started([crate::relay::STARTED, 0]), Start::Started);
    assert_eq!(
        started([crate::relay::NOT_STARTED, 13]),
        Start::NotStarted(13)
    );
    assert_eq!(started([7, 7]), Start::Started);
}
