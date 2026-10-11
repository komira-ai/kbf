//! The keeper: a process for each lease that kills the lease cgroup when the daemon dies
//! (issue #155).
//!
//! A daemon killed by SIGKILL, the kernel's OOM killer or a panic that aborts cannot
//! end its containers: each runs under conmon, outside the daemon's process tree. The
//! start-up sweep removes them only when a daemon is started again on the node. Until
//! then the action would run, for as long as the work takes, beside the copy the
//! scheduler starts elsewhere once the handover grace has passed. So each lease gets a
//! keeper once its cgroup is made: `/bin/sh` running [`SCRIPT`], whose stdin is a pipe
//! only the daemon holds the other end of. The daemon never writes it; when the daemon
//! dies the kernel closes that end, the keeper reads end-of-file and writes the lease
//! cgroup's `cgroup.kill`, which kills every process in the lease, the container's
//! among them. It writes it again every second for [`KILL_TRIES`] seconds: a
//! `podman start` the dead daemon left in flight can still put the container's first
//! process in the lease after the first write. It stops early once the cgroup is gone
//! (the next start's sweep removed it). The container, the lease cgroup and the scratch
//! directory stay for that sweep; nothing in them runs.
//!
//! The keeper ignores SIGHUP, SIGINT and SIGTERM, so a service manager that stops the
//! daemon's unit with SIGTERM does not stop it before it has read end-of-file. The
//! daemon removes the keeper (SIGKILL, then reaps it) once the lease is cleaned.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// How many times, a second apart, the keeper writes `cgroup.kill`.
pub const KILL_TRIES: u32 = 30;

/// What the keeper runs, with `$1` the lease cgroup's `cgroup.kill` and `$2`
/// [`KILL_TRIES`]: it waits for end-of-file on stdin (anything read is ignored), then
/// kills the cgroup while its `cgroup.kill` exists. It never creates the file.
pub const SCRIPT: &str = "trap '' HUP INT TERM
while read -r line; do :; done
n=0
while [ \"$n\" -lt \"$2\" ] && [ -e \"$1\" ]; do
  echo 1 > \"$1\"
  n=$((n + 1))
  sleep 1
done
";

/// The keeper's name in the process table (its `$0`).
pub const NAME: &str = "kbf-keeper";

/// One lease's keeper. Dropped, it is SIGKILLed and reaped without writing anything:
/// the pipe it reads closes only after it is dead.
#[derive(Debug)]
pub struct Keeper {
    child: Child,
}

impl Keeper {
    /// Starts the keeper of the cgroup whose `cgroup.kill` is `kill`, in a process
    /// group of its own.
    ///
    /// # Errors
    /// `/bin/sh` could not be started.
    pub fn watch(kill: &Path) -> io::Result<Self> {
        Self::with(Path::new("/bin/sh"), kill)
    }

    /// [`Keeper::watch`], with `shell` for `/bin/sh`.
    fn with(shell: &Path, kill: &Path) -> io::Result<Self> {
        let child = Command::new(shell)
            .arg("-c")
            .arg(SCRIPT)
            .arg(NAME)
            .arg(kill)
            .arg(KILL_TRIES.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir("/")
            .process_group(0)
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("start the lease's keeper: {e}")))?;
        Ok(Self { child })
    }

    /// What a daemon's death does: closes the daemon's end of the keeper's stdin.
    /// The keeper stays this process's child, reaped on drop.
    #[cfg(test)]
    pub fn orphan(&mut self) {
        drop(self.child.stdin.take());
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        // SIGKILL to the keeper alone (it is this process's unreaped child, so the pid
        // is still its own), then reap it. Its stdin closes after, with the struct.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-container-unit")
            .join(format!("keeper-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// Waits up to five seconds for `file` to hold `text`.
    fn holds(file: &Path, text: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::fs::read_to_string(file).is_ok_and(|t| t == text) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Catches a keeper that does not kill the lease cgroup when the daemon's end of
    /// its pipe closes (the bug: a killed daemon's container runs on), one that writes
    /// once only (a `podman start` in flight escapes it), and one that kills while the
    /// daemon holds the pipe. A plain file stands in for `cgroup.kill`.
    #[test]
    fn a_keeper_kills_the_cgroup_again_and_again_once_its_pipe_closes() {
        let dir = dir("kill");
        let kill = dir.join("cgroup.kill");
        std::fs::write(&kill, "").expect("cgroup.kill");
        let mut keeper = Keeper::watch(&kill).expect("keeper");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            std::fs::read_to_string(&kill).expect("read"),
            "",
            "killed while held"
        );
        keeper.orphan();
        assert!(holds(&kill, "1\n"), "no cgroup.kill once its pipe closed");
        std::fs::write(&kill, "").expect("as the kernel reads it");
        assert!(holds(&kill, "1\n"), "cgroup.kill written once only");
        // Gone, as after the sweep: the keeper stops and makes no file.
        std::fs::remove_file(&kill).expect("rm");
        let deadline = Instant::now() + Duration::from_secs(5);
        while keeper.child.try_wait().expect("wait").is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            keeper.child.try_wait().expect("wait").is_some(),
            "the keeper went on"
        );
        assert!(!kill.exists(), "the keeper made cgroup.kill");
    }

    /// Catches a keeper that cannot be started taken for one that runs, and an error
    /// that does not say what failed.
    #[test]
    fn a_keeper_that_cannot_start_is_an_error() {
        let error = Keeper::with(Path::new("/nonexistent-kbf-155/sh"), Path::new("/k"))
            .expect_err("no shell");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(
            error.to_string().starts_with("start the lease's keeper: "),
            "{error}"
        );
    }

    /// Catches a keeper whose drop (the daemon removing it once the lease is cleaned)
    /// writes `cgroup.kill`: dropped, it is killed before its pipe closes.
    #[test]
    fn a_dropped_keeper_kills_nothing() {
        let dir = dir("drop");
        let kill = dir.join("cgroup.kill");
        std::fs::write(&kill, "").expect("cgroup.kill");
        drop(Keeper::watch(&kill).expect("keeper"));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(std::fs::read_to_string(&kill).expect("read"), "");
    }
}
