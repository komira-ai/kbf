//! A real process, its output in files so a failed run can say why.
//!
//! Each process leads a process group of its own and asks the kernel to kill it if the
//! harness dies, so a harness that is killed (a CI job cancelled, a timeout) leaves no
//! frontend, node, daemon or Envoy behind; a process that is ended takes its group (what it
//! started, such as the actions of a native daemon) with it.

use std::fs::File;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};

/// How long a run waits for what should happen promptly: it ends a hang only.
pub const HANG: Duration = Duration::from_secs(60);

/// The instant `d` from now; far in the future, not a panic, when `d` is too long for an
/// `Instant` (a bound a caller computed from a command line).
#[must_use]
pub fn after(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(d)
        .unwrap_or_else(|| now + Duration::from_secs(10 * 365 * 24 * 3600))
}

/// What starts one process: the program, its arguments and environment, and the
/// directory it runs in. A [`Spec`] is kept so a role can be started again with the
/// identical command line.
#[derive(Clone, Debug)]
pub struct Spec {
    pub name: String,
    pub program: PathBuf,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// A running process.
pub struct Proc {
    pub name: String,
    child: Child,
    /// How it ended, once it was found ended (and its group swept).
    status: Option<ExitStatus>,
    out: PathBuf,
    err: PathBuf,
}

/// The log files of generation `generation` of process `name` in `logs`: the first start
/// is `<name>.out` and `.err`, a later one `<name>.<generation>.out`, so a restart
/// keeps what the process before it printed.
#[must_use]
pub fn log_files(logs: &Path, name: &str, generation: usize) -> (PathBuf, PathBuf) {
    let stem = if generation == 0 {
        name.to_owned()
    } else {
        format!("{name}.{generation}")
    };
    (
        logs.join(format!("{stem}.out")),
        logs.join(format!("{stem}.err")),
    )
}

/// What the child does between fork and exec: asks the kernel to kill it with SIGKILL when the
/// thread that started it ends, which a harness killed by SIGKILL (no `Drop` runs) does. A
/// function of its own so the test that runs it in a process of its own covers it (the
/// child's own counters are lost at exec).
fn die_with_the_harness() -> std::io::Result<()> {
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).map_err(Into::into)
}

impl Proc {
    /// Starts `spec`, its stdout and stderr in the files [`log_files`] names, as the leader of
    /// a process group of its own that the kernel kills with SIGKILL when the harness
    /// thread that started it ends.
    ///
    /// # Panics
    /// When the log files cannot be made or the program cannot be spawned: a harness
    /// that cannot start its process has nothing to report.
    #[must_use]
    pub fn start(spec: &Spec, logs: &Path, generation: usize) -> Self {
        let (out, err) = log_files(logs, &spec.name, generation);
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.argv)
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(File::create(&out).expect("stdout file")))
            .stderr(Stdio::from(File::create(&err).expect("stderr file")))
            .process_group(0);
        // SAFETY: the function runs in the child between fork and exec and makes one system
        // call (prctl), which is async-signal-safe; it allocates nothing.
        unsafe {
            command.pre_exec(die_with_the_harness);
        }
        let child = command
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", spec.program.display()));
        Self {
            name: spec.name.clone(),
            child,
            status: None,
            out,
            err,
        }
    }

    /// The process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// What the process printed, for a failure message.
    #[must_use]
    pub fn log(&self) -> String {
        let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
        format!(
            "--- {} stdout ---\n{}--- {} stderr ---\n{}",
            self.name,
            read(&self.out),
            self.name,
            read(&self.err)
        )
    }

    /// The process's id, which is its process group's too.
    fn pid_as_group(&self) -> Pid {
        Pid::from_child(&self.child)
    }

    /// SIGKILL to the process group. Sent while the leader is not yet reaped (so its id is
    /// still its own: a zombie keeps it) or in the moment after it was, when only a member
    /// still alive could keep the group's id; an error (no such group) is the point.
    fn sweep(&self) {
        let _ = rustix::process::kill_process_group(self.pid_as_group(), Signal::KILL);
    }

    /// Its exit status, if it has ended. The first time it is found ended, what it left
    /// running in its group is killed. If the operating system cannot say, it has not
    /// ended: a run that waits for it ends at its deadline.
    pub fn exited(&mut self) -> Option<ExitStatus> {
        if self.status.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.status = Some(status);
            self.sweep();
        }
        self.status
    }

    /// The first stdout line that starts with `prefix`, once the process printed it.
    #[must_use]
    pub fn line(&self, prefix: &str) -> Option<String> {
        let text = std::fs::read_to_string(&self.out).ok()?;
        text.lines()
            .find(|line| line.starts_with(prefix))
            .map(str::to_owned)
    }

    /// SIGTERM, and the exit status if the process has drained within `limit`. A process
    /// that has ended already is reported as it ended, not signalled: until it is reaped
    /// its id is its own, and afterwards it is not signalled at all.
    pub fn term_within(&mut self, limit: Duration) -> Option<ExitStatus> {
        if let Some(status) = self.exited() {
            return Some(status);
        }
        // Not reaped, so the id is still this process's (a zombie keeps it); a signal
        // that finds the process gone is no error.
        let _ = rustix::process::kill_process(self.pid_as_group(), Signal::TERM);
        let deadline = after(limit);
        loop {
            if let Some(status) = self.exited() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGKILL to the process and its group: the process is gone, as a host losing power
    /// would take it, and so is what it started.
    pub fn kill(&mut self) {
        if self.status.is_none() {
            self.sweep();
            // The leader too, should the group have been refused the signal.
            let _ = self.child.kill();
            self.status = self.child.wait().ok();
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The value of `key` in a start line such as `kbf-frontend 1.0 reapi=127.0.0.1:80 ...`.
///
/// # Panics
/// When the line has no such field.
#[must_use]
pub fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split(' ')
        .find_map(|word| word.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} in {line:?}"))
}

#[cfg(test)]
#[path = "procs_tests.rs"]
mod tests;
