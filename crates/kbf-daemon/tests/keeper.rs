//! The keeper mode ([`kbf_daemon::keeper`]) after an update renamed a new binary over
//! the running one, through `kbf-keeper-swap` (`tests/bin/keeper_swap.rs`), and a program
//! that is no keeper.

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
