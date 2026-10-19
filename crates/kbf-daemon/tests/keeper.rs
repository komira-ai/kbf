//! The keeper mode ([`kbf_daemon::keeper`]) after an update renamed a new binary over
//! the running one, through `kbf-keeper-swap` (`tests/bin/keeper_swap.rs`), and a program
//! that is no keeper.

use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_daemon::keeper::{self, Mode};

/// An empty directory for this test, on the file system that holds the test's
/// binaries (a hard link must reach them).
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-daemon-keeper")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

/// What an update puts at the binary's path. On Linux, a file that is no program
/// (mode 0644, so it cannot be run at all): only the file the process was started
/// from can be the keeper. On macOS, a copy of the same binary, as a new version:
/// there the path is all a process knows of its binary.
fn replacement(binary: &Path, at: &Path) {
    if cfg!(target_os = "linux") {
        std::fs::write(at, "not a program\n").expect("write the replacement");
    } else {
        std::fs::copy(binary, at).expect("copy the replacement");
    }
}

/// Catches a keeper that cannot be started once an update has renamed a new file over
/// the daemon's binary, an update's swap. On Linux the keeper must come from the
/// file the daemon runs (`/proc/self/exe`): `current_exe` then names
/// `<path> (deleted)`, and nothing can be started from it. On macOS it comes from the
/// path, which names the new binary. The keeper must start (its ready line), and
/// write `cgroup.kill` once its pipe closes.
#[test]
fn a_keeper_starts_after_the_binary_is_renamed_over() {
    let dir = scratch("swap");
    let binary = Path::new(env!("CARGO_BIN_EXE_kbf-keeper-swap"));
    // A hard link, not a copy: a copy still open for writing in a sibling test's fork
    // would refuse to run (ETXTBSY). The rename then replaces the link, not the
    // binary Cargo built.
    let swap = dir.join("kbf-keeper-swap");
    std::fs::hard_link(binary, &swap).expect("link the binary");
    let new = dir.join("new");
    replacement(binary, &new);
    let kill = dir.join("cgroup.kill");
    std::fs::write(&kill, "").expect("cgroup.kill");

    let out = Command::new(&swap)
        .arg(&new)
        .arg(&kill)
        .output()
        .expect("run kbf-keeper-swap");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{}: {stderr}", out.status);
    assert!(
        !new.exists(),
        "the replacement was not renamed over the binary"
    );
    assert_eq!(
        std::fs::read_to_string(&kill).expect("read cgroup.kill"),
        "1\n",
        "the keeper did not kill the cgroup"
    );
}

/// Catches a spawn taken for a keeper's although the program is no keeper: one that
/// ends at once, and one that prints something else (this test binary, which has no
/// hook: the test harness takes `__keeper group --exact` as a filter that names no
/// test, and prints its summary), and a command with no stdout pipe to read from.
#[test]
fn a_program_that_is_no_keeper_is_refused() {
    let this = std::env::current_exe().expect("the test binary");
    for program in [Path::new("/usr/bin/true"), &this] {
        let mut command = keeper::command(program, &Mode::Group);
        let mut child = command.arg("--exact").spawn().expect("spawn");
        let error = keeper::started(&mut child).expect_err("not a keeper");
        assert!(
            error.to_string().contains("did not answer as a keeper"),
            "{error}"
        );
        drop(child.stdin.take());
        child.wait().expect("reap");
    }
    let mut child = Command::new("/usr/bin/true").spawn().expect("spawn");
    assert!(keeper::started(&mut child).is_err());
    child.wait().expect("reap");
}

/// Catches a `group` keeper that does not end its group when its stdin reaches
/// end-of-file, or ends it with a signal that can be caught (the keeper is the leader
/// of its own group here, as the native driver's is a member of its action's): it must
/// stay up while the pipe is open, and die of SIGKILL, with the group's other member,
/// once it closes. The kill takes the keeper's counters with it, so this test cannot
/// show the lines run; the unit test of the arm does.
#[test]
fn a_group_keeper_kills_its_group_when_stdin_closes() {
    let binary = Path::new(env!("CARGO_BIN_EXE_kbf-keeper-swap"));
    let mut keeper = keeper::command(binary, &Mode::Group)
        .process_group(0)
        .spawn()
        .expect("spawn the keeper");
    keeper::started(&mut keeper).expect("the keeper is ready");
    assert!(
        keeper.try_wait().expect("poll").is_none(),
        "the keeper ended with its stdin open"
    );
    let group = i32::try_from(keeper.id()).expect("a pid");
    // A second member of the keeper's group, which only the group kill can end.
    let mut member = Command::new("/bin/sleep")
        .arg("3600")
        .process_group(group)
        .spawn()
        .expect("spawn a member");
    drop(keeper.stdin.take());
    let status = keeper.wait().expect("reap the keeper");
    if status.signal() != Some(libc::SIGKILL) {
        // Do not leave the member running for the hour.
        member.kill().expect("kill the member");
        member.wait().expect("reap the member");
        panic!("the keeper did not die of SIGKILL: {status}");
    }
    let status = member.wait().expect("reap the member");
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{status}");
}
