use std::ffi::OsString;
use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;

use super::*;
use crate::in_child::in_child;

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// A pipe for a start report: the write end to hand out, and what was written once every write
/// end is closed.
struct Pipe {
    read: OwnedFd,
    write: Option<OwnedFd>,
}

impl Pipe {
    fn new() -> Self {
        let (read, write) = rustix::pipe::pipe().unwrap();
        Self {
            read,
            write: Some(write),
        }
    }

    fn write_end(&self) -> &OwnedFd {
        self.write.as_ref().unwrap()
    }

    fn said(mut self) -> Vec<u8> {
        drop(self.write.take());
        let mut bytes = Vec::new();
        std::fs::File::from(self.read)
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }
}

/// The `unshare` of a user and PID namespace, as the child's step makes it; whether it worked.
fn unshare_user_and_pid() -> bool {
    let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWPID;
    crate::child::run_step(&mut crate::child::Step::Unshare(flags)).is_ok()
}

fn mode(relay: bool, userns: bool, command: &[OsString]) -> Mode<'_> {
    Mode {
        relay,
        group: None,
        userns,
        command,
    }
}

/// Catches a mode, a flag or the Command read wrongly: the relay's flags, the first process's,
/// and each malformed form refused.
#[test]
fn the_arguments_of_each_mode_are_read_and_a_wrong_one_refused() {
    let relay = os(&[
        RELAY,
        "--group",
        "42",
        "--userns",
        "--",
        "/bin/prog",
        "a",
        "--",
    ]);
    let parsed = parse(&relay).unwrap();
    assert!(parsed.relay);
    assert_eq!(parsed.group, Pid::from_raw(42));
    assert!(parsed.userns);
    assert_eq!(parsed.command, os(&["/bin/prog", "a", "--"]).as_slice());
    let init = os(&[INIT, "--", "prog"]);
    let parsed = parse(&init).unwrap();
    assert!(!parsed.relay);
    assert_eq!(parsed.group, None);
    assert!(!parsed.userns);
    assert_eq!(parsed.command, os(&["prog"]).as_slice());
    for wrong in [
        vec![],
        os(&[RELAY]),
        os(&[RELAY, "--"]),
        os(&[RELAY, "--group", "x", "--", "p"]),
        os(&[RELAY, "--group", "0", "--", "p"]),
        os(&[RELAY, "--group"]),
        os(&[RELAY, "p"]),
    ] {
        assert!(parse(&wrong).is_err(), "{wrong:?}");
    }
    let pipe = Pipe::new();
    assert_eq!(run_mode(pipe.write_end(), Path::new("/x"), &os(&[INIT])), 2);
    assert!(pipe.said().is_empty());
    // Each mode, run: the first process starts its Command, the relay (no PID namespace here)
    // executes it.
    let pipe = Pipe::new();
    let init = os(&[INIT, "--", "/usr/bin/false"]);
    assert_eq!(
        in_child(|| run_mode(pipe.write_end(), Path::new("/x"), &init)),
        1
    );
    assert_eq!(pipe.said(), [STARTED, 0]);
    let relay = os(&[RELAY, "--group", "1", "--", "/usr/bin/false"]);
    assert_eq!(
        in_child(|| run_mode(Pipe::new().write_end(), Path::new("/x"), &relay)),
        1
    );
    // A relay that cannot execute its Command returns, and says why.
    let pipe = Pipe::new();
    let missing = os(&[RELAY, "--group", "1", "--", "/no/such/program"]);
    assert_eq!(
        run_mode(pipe.write_end(), Path::new("/x"), &missing),
        CANNOT_START
    );
    assert_eq!(
        pipe.said(),
        [NOT_STARTED, u8::try_from(libc::ENOENT).unwrap()]
    );
}

/// Catches the relay started with arguments it does not read back: the tool's arguments for it
/// are its own form.
#[test]
fn the_relays_arguments_are_read_back_as_given() {
    let group = Pid::from_raw(7).unwrap();
    let args = [OsString::from("a"), OsString::from("--")];
    for userns in [true, false] {
        let argv = argv(group, userns, Path::new("/bin/prog"), &args);
        let parsed = parse(&argv).unwrap();
        assert!(parsed.relay);
        assert_eq!(parsed.group, Some(group));
        assert_eq!(parsed.userns, userns);
        assert_eq!(parsed.command, os(&["/bin/prog", "a", "--"]).as_slice());
    }
    assert!(selected(Some(std::ffi::OsStr::new(RELAY))));
    assert!(selected(Some(std::ffi::OsStr::new(INIT))));
    assert!(!selected(Some(std::ffi::OsStr::new("--lease"))));
    assert!(!selected(None));
}

/// Catches the relay, where no PID namespace is pending (the `unshare` failed), not executing
/// the Command itself, or a Command it cannot execute not reported: the process becomes the
/// Command (its exit code is the Command's), and a missing one is reported with its errno.
#[test]
fn without_a_pid_namespace_the_relay_executes_the_command_itself() {
    let pipe = Pipe::new();
    let command = os(&["/usr/bin/false"]);
    let code = in_child(|| {
        relay(
            pipe.write_end(),
            Path::new("/no/init"),
            &mode(true, false, &command),
        )
    });
    assert_eq!(code, 1);
    assert!(
        pipe.said().is_empty(),
        "an executed Command writes no report"
    );
    let pipe = Pipe::new();
    let missing = os(&["/no/such/program"]);
    let code = in_child(|| {
        relay(
            pipe.write_end(),
            Path::new("/no/init"),
            &mode(true, false, &missing),
        )
    });
    assert_eq!(code, CANNOT_START);
    assert_eq!(
        pipe.said(),
        [NOT_STARTED, u8::try_from(libc::ENOENT).unwrap()]
    );
}

/// Catches the relay, where a PID namespace is pending, not starting the first process, not
/// passing its exit up, or a first process it cannot start not reported. The first process is
/// `/usr/bin/false` here (it ignores its arguments). Where this host refuses even the `unshare`
/// of a user and PID namespace, the case says so.
#[test]
fn with_a_pid_namespace_the_relay_starts_the_first_process_and_passes_its_exit_up() {
    let command = os(&["/usr/bin/true"]);
    let case = |init: &'static str, userns: bool| {
        let pipe = Pipe::new();
        let code = in_child(|| {
            if !unshare_user_and_pid() {
                return 200;
            }
            relay(
                pipe.write_end(),
                Path::new(init),
                &mode(true, userns, &command),
            )
        });
        (code, pipe.said())
    };
    let (code, said) = case("/usr/bin/false", false);
    if code == 200 {
        eprintln!("skipped: this host refuses the unshare of a user and PID namespace");
        return;
    }
    assert_eq!((code, said.as_slice()), (1, &[][..]));
    // The first process is told the Command has a user namespace of its own.
    assert_eq!(case("/usr/bin/false", true), (1, Vec::new()));
    let (code, said) = case("/no/such/init", false);
    assert_eq!(code, CANNOT_START);
    assert_eq!(said, [NOT_STARTED, u8::try_from(libc::ENOENT).unwrap()]);
}

/// Catches a pending PID namespace read wrongly from the two links: one not shown yet (no first
/// process) or another than the process's own is pending; the same, or no `/proc`, is not.
#[test]
fn a_pending_pid_namespace_is_read_from_the_two_links() {
    let link = |s: &str| Some(std::path::PathBuf::from(s));
    assert!(!pending(link("pid:[1]"), link("pid:[1]")));
    assert!(pending(link("pid:[1]"), link("pid:[2]")));
    assert!(pending(link("pid:[1]"), None));
    assert!(!pending(None, None));
    assert!(!pending(None, link("pid:[2]")));
}

/// Catches a PID namespace for the children taken as none, or the process's own taken as a new
/// one.
#[test]
fn a_pending_pid_namespace_is_seen_only_after_the_unshare() {
    assert!(!pid_namespace_pending());
    let code = in_child(|| {
        if unshare_user_and_pid() {
            i32::from(!pid_namespace_pending())
        } else {
            200
        }
    });
    assert!(code == 0 || code == 200, "{code}");
}

/// Catches the first process not reporting a start, not passing the Command's exit up, starting
/// a Command it was not given, or keeping a capability for one in a user namespace: it starts
/// the Command, reports it, and leaves with its code; a missing one is reported with its errno.
#[test]
fn the_first_process_starts_the_command_reports_it_and_passes_its_exit_up() {
    for userns in [false, true] {
        let pipe = Pipe::new();
        let command = os(&["/usr/bin/false"]);
        let code = in_child(|| init(pipe.write_end(), &mode(false, userns, &command)));
        assert_eq!(code, 1, "userns {userns}");
        assert_eq!(pipe.said(), [STARTED, 0], "userns {userns}");
    }
    let pipe = Pipe::new();
    let missing = os(&["/no/such/program"]);
    let code = in_child(|| init(pipe.write_end(), &mode(false, false, &missing)));
    assert_eq!(code, CANNOT_START);
    assert_eq!(
        pipe.said(),
        [NOT_STARTED, u8::try_from(libc::ENOENT).unwrap()]
    );
}

/// Catches either mode going on without a Command to start (the parser never gives one, but the
/// functions take any mode).
#[test]
fn a_mode_without_a_command_starts_nothing() {
    let pipe = Pipe::new();
    assert_eq!(
        relay(pipe.write_end(), Path::new("/x"), &mode(true, false, &[])),
        CANNOT_START
    );
    assert!(pipe.said().is_empty());
    // The first process (whose setup changes this process: a forked child) reports it.
    let pipe = Pipe::new();
    let code = in_child(|| init(pipe.write_end(), &mode(false, false, &[])));
    assert_eq!(code, CANNOT_START);
    assert_eq!(
        pipe.said(),
        [NOT_STARTED, u8::try_from(libc::EINVAL).unwrap()]
    );
}

/// Catches a Command's process keeping a capability or running without `no_new_privs`: after
/// the drop it has neither a capability in any set nor the right to gain one.
#[test]
fn dropping_the_capabilities_leaves_none_and_sets_no_new_privs() {
    let code = in_child(|| {
        drop_capabilities().unwrap();
        let caps = rustix::thread::capabilities(None).unwrap();
        let none = rustix::thread::CapabilitySet::empty();
        let empty = caps.effective == none && caps.permitted == none && caps.inheritable == none;
        i32::from(!empty) | i32::from(!rustix::thread::no_new_privs().unwrap()) << 1
    });
    assert_eq!(code, 0);
}

/// Catches the first process returning before the Command's end, or not reaping a process
/// handed to it: it waits for every child until the one it was told of ends. The other child
/// ends first; the Command (`cat`) ends when its input closes after that.
#[test]
#[allow(clippy::zombie_processes)] // Both are reaped by `wait_until`'s `waitpid`, which is the subject.
fn the_first_process_reaps_every_child_until_the_commands_ends() {
    let code = in_child(|| {
        let mut command = std::process::Command::new("/usr/bin/cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let other = std::process::Command::new("/usr/bin/false")
            .spawn()
            .unwrap();
        // The other has ended (seen without reaping it) before the Command's input closes.
        let ended = rustix::process::waitid(
            rustix::process::WaitId::Pid(Pid::from_child(&other)),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
        );
        assert!(matches!(ended, Ok(Some(_))), "{ended:?}");
        let target = Pid::from_child(&command);
        drop(command.stdin.take());
        let code = wait_until(target, true);
        // Nothing is left to reap: the other child was reaped on the way.
        let left = rustix::process::waitpid(None, WaitOptions::NOHANG);
        i32::from(code != 0) | i32::from(!matches!(left, Err(rustix::io::Errno::CHILD))) << 1
    });
    assert_eq!(code, 0);
}

/// Catches a wait that cannot succeed looping or being taken for the Command's exit: it is
/// reported as a kill by `SIGKILL`.
#[test]
fn a_wait_for_no_child_is_a_kill() {
    let nobody = Pid::from_raw(i32::MAX).unwrap();
    assert_eq!(wait_until(nobody, false), 128 + libc::SIGKILL);
    assert_eq!(in_child(|| wait_until(nobody, true)), 128 + libc::SIGKILL);
}

/// Catches an exit status passed up as anything but its code, or 128 plus the signal that ended
/// the process, whichever way it was reaped.
#[test]
fn an_exit_status_is_its_code_or_128_plus_its_signal() {
    for code in [0, 1, 255] {
        assert_eq!(in_child(|| code), code);
        assert_eq!(status_code(ExitStatus::from_raw(code << 8)), code);
    }
    for signal in [libc::SIGKILL, libc::SIGTERM, libc::SIGUSR1] {
        let ended = in_child(|| {
            let signal = rustix::process::Signal::from_named_raw(signal).unwrap();
            // The signal ends this child.
            rustix::process::kill_process(rustix::process::getpid(), signal).unwrap();
            0
        });
        assert_eq!(ended, 128 + signal);
        assert_eq!(status_code(ExitStatus::from_raw(signal)), 128 + signal);
    }
}
