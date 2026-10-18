use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use kbf_host::testkit::reply;
use kbf_host::{Cmd, HostPath, Root};

use super::fake::ServiceRecorder;
use super::*;
use crate::fixtures::{FakeRunner, ran};
use crate::testkit::{A, B, C, c, host_root};

const UNIT: &str = "kbf.service";

fn systemd() -> Restart {
    Restart::Systemd(UNIT.to_owned())
}

fn pending(commit: &str, deadline: Option<u64>) -> PendingConfirm {
    PendingConfirm {
        commit: c(commit),
        previous: Some(c(A)),
        activation: "1-5".to_owned(),
        deadline_wall: deadline,
        deadline_mono_s: None,
        starts_left: Some(2),
    }
}

/// A scratch host whose service runs `exe` (relative to the root), and its executor.
fn world(
    name: &str,
    exe: &str,
) -> (
    Layout,
    PathBuf,
    ServiceRecorder,
    kbf_host::testkit::ScratchHost,
) {
    let (layout, dir) = host_root(name);
    let svc = ServiceRecorder::running(77, dir.join(exe));
    let mut host = kbf_host::testkit::ScratchHost::new(&format!("{name}-exec"));
    host.answer = svc.answer();
    (layout, dir, svc, host)
}

/// Catches `ps` lines that are not an executable path (a kernel thread, a bare name) being
/// taken for one, and a path with spaces being cut.
///
/// Mutant planted: the `starts_with('/')` filter dropped.
#[test]
fn ps_output_keeps_only_absolute_paths() {
    let out = b"/sbin/launchd\n  /var/lib/kbf-installer/releases/x y/kbf-daemon \nkernel_task\n\n";
    assert_eq!(
        parse_ps(out),
        vec![
            PathBuf::from("/sbin/launchd"),
            PathBuf::from("/var/lib/kbf-installer/releases/x y/kbf-daemon")
        ]
    );
}

/// Catches a unit read as running when systemd says otherwise: only `active` with a main
/// process is a service that can confirm. The question is one fixed command, the unit after
/// `--`, so a unit name can never be read as an option.
///
/// Mutants planted: the `ActiveState` test dropped (a failed unit with a stale MainPID
/// passes); MainPID 0 accepted; the `--` removed.
#[test]
fn the_unit_must_be_active_with_a_main_process() {
    let (_, _, svc, host) = world("svc-systemd", "x");
    assert_eq!(main_pid(&host, &systemd()), Ok(77));
    assert_eq!(
        svc.asked(),
        vec![format!(
            "/usr/bin/systemctl show --property=ActiveState --property=MainPID -- {UNIT}"
        )]
    );
    svc.0.borrow_mut().active = "failed".to_owned();
    assert_eq!(
        main_pid(&host, &systemd()),
        Err("kbf.service is failed, not active".to_owned())
    );
    svc.0.borrow_mut().active = "active".to_owned();
    svc.0.borrow_mut().main_pid = 0;
    assert_eq!(
        main_pid(&host, &systemd()),
        Err("kbf.service is active and has no main process".to_owned())
    );
}

/// Catches answers the parser must not guess at: systemctl failing, saying nothing about
/// the state, an unparsable pid, a manager that cannot be started, and (launchd) a job that
/// is not loaded or loaded but not running.
///
/// Mutants planted: a non-zero exit read as success; the launchd `pid = 0` accepted.
#[test]
fn an_answer_that_does_not_say_active_is_no() {
    let (_, _, _, mut host) = world("svc-odd", "x");
    fn answer(host: &mut kbf_host::testkit::ScratchHost, text: &'static str, code: i32) {
        host.answer = Box::new(move |_| Ok(reply(code, text, "")));
    }
    answer(&mut host, "ActiveState=active\nMainPID=5\n", 1);
    assert_eq!(
        main_pid(&host, &systemd()),
        Err("systemctl cannot show kbf.service".to_owned())
    );
    answer(&mut host, "Description=x\n", 0);
    assert_eq!(
        main_pid(&host, &systemd()),
        Err("systemctl did not say whether kbf.service is active".to_owned())
    );
    answer(&mut host, "ActiveState=active\nMainPID=lots\n", 0);
    assert!(main_pid(&host, &systemd()).is_err());
    host.answer = Box::new(|_| Err(io::Error::other("no systemctl")));
    assert!(
        main_pid(&host, &systemd())
            .unwrap_err()
            .contains("cannot ask the service manager")
    );

    let launchd = Restart::Launchd("com.example.kbf".to_owned());
    answer(
        &mut host,
        "system/com.example.kbf = {\n\tstate = running\n\tpid = 812\n}\n",
        0,
    );
    assert_eq!(main_pid(&host, &launchd), Ok(812));
    answer(
        &mut host,
        "system/com.example.kbf = {\n\tstate = waiting\n}\n",
        0,
    );
    assert_eq!(
        main_pid(&host, &launchd),
        Err("com.example.kbf is loaded and not running".to_owned())
    );
    answer(&mut host, "system/com.example.kbf = {\n\tpid = 0\n}\n", 0);
    assert!(main_pid(&host, &launchd).is_err());
    answer(&mut host, "", 113);
    assert_eq!(
        main_pid(&host, &launchd),
        Err("com.example.kbf is not loaded".to_owned())
    );
}

/// Catches a confirm accepted from a service that is not running the new release. Each of
/// these is refused: the service started a file of another release; of a directory whose
/// name only begins like the release's; one the kernel reports as deleted; the first boot
/// check not yet run (Linux); the unit not active; the process gone. The right release, with
/// the window started, passes. (macOS arms at the swap and has no boot check.)
///
/// Mutants planted: string prefix instead of path components (the sibling directory
/// passes); the " (deleted)" test dropped; the boot-check test dropped.
#[test]
fn a_confirm_needs_the_new_release_running_and_started_once() {
    let release = format!("var/lib/kbf-installer/releases/{B}/kbf");
    let (layout, dir, svc, host) = world("svc-accept", &release);
    let ok = |svc: &ServiceRecorder, deadline| {
        let _ = svc;
        accept_confirm(&host, svc, &layout, &systemd(), &pending(B, deadline))
    };
    assert_eq!(ok(&svc, Some(1)), Ok(()));
    assert!(
        ok(&svc, None)
            .unwrap_err()
            .contains("first boot check has not run")
    );
    // A Mac keeps its releases elsewhere and has no boot check: the service runs from there.
    let mac = Layout::new(Root::new(&dir).unwrap(), Platform::MacOs);
    svc.0.borrow_mut().exes = vec![(
        77,
        dir.join(format!("usr/local/kbf-installer/releases/{B}/kbf")),
    )];
    assert_eq!(
        accept_confirm(&host, &svc, &mac, &systemd(), &pending(B, None)),
        Ok(()),
        "macOS has no boot check"
    );
    svc.0.borrow_mut().exes = vec![(77, dir.join(&release))];

    for (exe, why) in [
        (
            format!("var/lib/kbf-installer/releases/{A}/kbf"),
            "another release",
        ),
        (
            format!("var/lib/kbf-installer/releases/{B}0/kbf"),
            "a sibling directory",
        ),
        (
            format!("var/lib/kbf-installer/releases/{B}/kbf (deleted)"),
            "a deleted file",
        ),
        ("usr/bin/kbf".to_owned(), "a file outside"),
    ] {
        svc.0.borrow_mut().exes = vec![(77, dir.join(&exe))];
        let err = ok(&svc, Some(1)).unwrap_err();
        assert!(err.contains("not a file in"), "{why}: {err}");
    }
    svc.0.borrow_mut().exes.clear();
    assert!(
        ok(&svc, Some(1))
            .unwrap_err()
            .contains("cannot read the executable of process 77")
    );
    svc.0.borrow_mut().active = "inactive".to_owned();
    assert!(ok(&svc, Some(1)).unwrap_err().contains("not active"));
}

/// Catches a release root reached through a link being compared by its spelling: the kernel
/// reports resolved paths, so a root that is itself a link still matches.
///
/// Mutant planted: `resolved` returning its argument.
#[test]
fn paths_are_compared_resolved() {
    let (layout, dir) = host_root("svc-resolved");
    let via = dir.parent().unwrap().join(format!(
        "{}-link",
        dir.file_name().unwrap().to_str().unwrap()
    ));
    symlink(&dir, &via).unwrap();
    let release = dir.join(format!("var/lib/kbf-installer/releases/{B}"));
    fs::create_dir_all(&release).unwrap();
    fs::write(release.join("kbf"), b"x").unwrap();
    let svc = ServiceRecorder::running(
        77,
        via.join(format!("var/lib/kbf-installer/releases/{B}/kbf")),
    );
    let mut host = kbf_host::testkit::ScratchHost::new("svc-resolved-exec");
    host.answer = svc.answer();
    assert_eq!(
        accept_confirm(&host, &svc, &layout, &systemd(), &pending(B, Some(1))),
        Ok(())
    );
    fs::remove_file(&via).unwrap();
}

/// Catches `gc` being told a release is idle when a process runs from it: every release some
/// process executes from is named, including one reached through a link and one whose file
/// was deleted; processes elsewhere are not releases; a host whose processes cannot be listed
/// is an error (the caller then removes nothing).
///
/// Mutants planted: the " (deleted)" suffix refused (a running, replaced release is
/// collected); the error swallowed into an empty set.
#[test]
fn releases_in_use_are_the_ones_processes_execute_from() {
    let (layout, dir) = host_root("svc-in-use");
    let svc = ServiceRecorder::default();
    let rel = |c: &str, f: &str| dir.join(format!("var/lib/kbf-installer/releases/{c}/{f}"));
    svc.0.borrow_mut().exes = vec![
        (1, rel(B, "kbf")),
        (2, rel(C, "bin/helper (deleted)")),
        (3, dir.join("usr/bin/other")),
        (
            4,
            dir.join("var/lib/kbf-installer/releases/not-a-commit/kbf"),
        ),
    ];
    assert_eq!(
        in_use(&svc, &layout).unwrap(),
        [c(B), c(C)].into_iter().collect()
    );
    svc.0.borrow_mut().blind = true;
    assert!(in_use(&svc, &layout).is_err());
}

/// Catches the macOS process table being read wrong: `ps` is run by absolute path with no
/// environment, its paths become executables, and a failed or hung `ps` is an error, not an
/// empty table (which would let `gc` remove a running release).
///
/// Mutants planted: a non-zero exit accepted; the timeout flag ignored.
#[test]
fn the_macos_table_comes_from_ps() {
    let runner = FakeRunner::new(|spawn| {
        Ok(match spawn.args[0].to_str().unwrap() {
            "-ax" => ran(
                0,
                "/sbin/launchd\nkernel_task\n/var/lib/kbf-installer/releases/x/kbf-daemon\n",
                "",
            ),
            "-p" => ran(0, "/var/lib/kbf-installer/releases/x/kbf-daemon\n", ""),
            other => panic!("unexpected ps {other}"),
        })
    });
    let procs = HostProcs::with_runner(Platform::MacOs, Box::new(runner));
    assert_eq!(
        procs.executables().unwrap(),
        vec![
            PathBuf::from("/sbin/launchd"),
            PathBuf::from("/var/lib/kbf-installer/releases/x/kbf-daemon")
        ]
    );
    assert_eq!(
        procs.exe_of(812).unwrap(),
        PathBuf::from("/var/lib/kbf-installer/releases/x/kbf-daemon")
    );
}

#[test]
fn a_failed_or_hung_ps_is_an_error() {
    for (code, timed_out) in [(1, false), (0, true)] {
        let runner = FakeRunner::new(move |_| {
            Ok(crate::run::Ran {
                timed_out,
                ..ran(code, "/sbin/launchd\n", "")
            })
        });
        let procs = HostProcs::with_runner(Platform::MacOs, Box::new(runner));
        assert!(procs.executables().is_err(), "{code} {timed_out}");
    }
    // A process that is gone has no executable.
    let runner = FakeRunner::new(|_| Ok(ran(0, "", "")));
    let procs = HostProcs::with_runner(Platform::MacOs, Box::new(runner));
    assert_eq!(procs.exe_of(1).unwrap_err().kind(), io::ErrorKind::NotFound);
}

/// Catches the Linux process table being read from the wrong place: this test process's own
/// executable is in the table and is what `exe_of` says for its pid.
#[cfg(target_os = "linux")]
#[test]
fn the_linux_table_is_proc() {
    let procs = HostProcs::new(Platform::Linux);
    let me = std::env::current_exe().unwrap();
    assert_eq!(procs.exe_of(std::process::id()).unwrap(), me);
    assert!(procs.executables().unwrap().contains(&me));
}

/// The recorder answers a command it does not know with success, and records it.
#[test]
fn the_recorder_records_and_answers_unknown_commands() {
    let svc = ServiceRecorder::default();
    let answer = svc.answer();
    let cmd = Cmd::new(HostPath::new("/usr/bin/systemctl").unwrap())
        .arg("restart")
        .arg(UNIT);
    assert!(answer(&cmd).unwrap().status.success());
    assert_eq!(svc.asked(), vec!["/usr/bin/systemctl restart kbf.service"]);
}
