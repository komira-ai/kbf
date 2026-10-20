//! The sentinel: what cleans up after a harness that is killed.
//!
//! Every farm process asks the kernel to kill it when the harness dies ([`crate::procs`]), but
//! the Buck2 daemons of the load are not the harness's children: each leads a session of its
//! own, started by a `buck2 build` client that has ended. A harness killed by SIGKILL (a
//! runner that cancels a job and gives it no time) runs no `Drop`, so those daemons, and the
//! frontend's control socket, would be left behind.
//!
//! So the harness starts, before the farm, a second process: `kbf-beta reap`. It leads a
//! session of its own and has no parent-death signal, and it reads its standard input, a pipe
//! the harness holds the write end of. When the harness ends well it writes `done` and
//! closes the pipe, and the sentinel exits having done nothing. When the harness dies by any
//! means, the kernel closes the pipe, the sentinel reads the end of its input with no `done`,
//! and it kills the process groups the roles recorded ([`crate::procs::pgid_file`]: the kernel
//! kills a role itself with the harness but not what the role started), the daemons of the
//! loops, and removes the control socket.

use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use clap::Args;

use crate::load::{Daemons, wait_bounded};

/// What the harness writes when the run ended and there is nothing to clean up.
const DONE: &str = "done";

/// How long the harness waits for a sentinel that was told `done` to exit.
const RELEASE_BOUND: Duration = Duration::from_secs(10);

/// What the sentinel cleans up (the arguments of `kbf-beta reap`).
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Reap {
    /// The pinned buck2.
    #[arg(long)]
    pub buck2: PathBuf,
    /// The project the daemons were started in.
    #[arg(long)]
    pub project: PathBuf,
    /// The number of build loops: the daemons are `beta-<run>-0` to `beta-<run>-<lanes - 1>`.
    #[arg(long)]
    pub lanes: usize,
    /// The run's id, which names its daemons.
    #[arg(long)]
    pub run_id: String,
    /// The longest one `buck2 kill` may take, in milliseconds.
    #[arg(long)]
    pub kill_timeout_ms: u64,
    /// The frontend's control socket, which a killed frontend leaves behind.
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// The directory the roles record their process groups in (the run's logs).
    #[arg(long)]
    pub pgids: Option<PathBuf>,
}

/// What the sentinel read from the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Word {
    /// `done`: the harness ended well.
    Done,
    /// The input ended with no `done`: the harness died.
    Lost,
}

impl Reap {
    /// The cleanup of `daemons` and, if there is one, `socket`.
    #[must_use]
    pub fn new(daemons: &Daemons, socket: Option<PathBuf>, pgids: Option<PathBuf>) -> Self {
        Self {
            buck2: daemons.buck2.clone(),
            project: daemons.project.clone(),
            lanes: daemons.lanes,
            run_id: daemons.run_id.clone(),
            kill_timeout_ms: u64::try_from(daemons.kill_timeout.as_millis()).unwrap_or(u64::MAX),
            socket,
            pgids,
        }
    }

    /// The daemons to kill.
    #[must_use]
    pub fn daemons(&self) -> Daemons {
        Daemons {
            buck2: self.buck2.clone(),
            project: self.project.clone(),
            lanes: self.lanes,
            run_id: self.run_id.clone(),
            kill_timeout: Duration::from_millis(self.kill_timeout_ms),
        }
    }

    /// The command line of `kbf-beta` (without the program) that runs this cleanup.
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "reap".into(),
            "--buck2".into(),
            self.buck2.clone().into(),
            "--project".into(),
            self.project.clone().into(),
            "--lanes".into(),
            self.lanes.to_string().into(),
            // One word: an id that starts with `-` (an empty CI run id) is not another option.
            format!("--run-id={}", self.run_id).into(),
            "--kill-timeout-ms".into(),
            self.kill_timeout_ms.to_string().into(),
        ];
        if let Some(socket) = &self.socket {
            args.push("--socket".into());
            args.push(socket.clone().into());
        }
        if let Some(pgids) = &self.pgids {
            args.push("--pgids".into());
            args.push(pgids.clone().into());
        }
        args
    }

    /// The sentinel's whole life: waits for the harness to say it is done, or to die, and in
    /// the second case kills the daemons and removes the control socket. Exits 0 after a
    /// `done` and 1 after the cleanup of a harness that died.
    pub fn run(&self, input: &mut dyn BufRead) -> u8 {
        match word(input) {
            Word::Done => 0,
            Word::Lost => {
                if let Some(pgids) = &self.pgids {
                    kill_groups(pgids);
                }
                self.daemons().kill_all();
                if let Some(socket) = &self.socket {
                    let _ = std::fs::remove_file(socket);
                }
                1
            }
        }
    }
}

/// SIGKILL to every process group a `*.pgid` file of `dir` names. The files are the roles'
/// ([`crate::procs::pgid_file`]), removed when a role's group is swept, so a group named here
/// belongs to a role that was running when the harness died. A name that is not a group id
/// the sentinel may signal (not a number, 0, 1) is skipped.
pub fn kill_groups(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "pgid") {
            continue;
        }
        let pgid = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .filter(|n| *n > 1)
            .and_then(rustix::process::Pid::from_raw);
        if let Some(pgid) = pgid {
            let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
        }
    }
}

/// Reads `input` until it says `done` or ends.
#[must_use]
pub fn word(input: &mut dyn BufRead) -> Word {
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => return Word::Lost,
            Ok(_) if line.trim() == DONE => return Word::Done,
            Ok(_) => {}
        }
    }
}

/// What the sentinel does between fork and exec: leads a session of its own, so a signal to the
/// harness's group or terminal does not reach it. A function of its own so the test that runs
/// it in a process of its own covers it (the child's own counters are lost at exec).
pub(crate) fn own_session() -> std::io::Result<()> {
    rustix::process::setsid()
        .map(|_| ())
        .map_err(std::io::Error::from)
}

/// The harness's end of the sentinel. Dropped, it tells the sentinel the run is over.
pub struct Sentinel {
    child: Child,
    input: Option<ChildStdin>,
}

impl Sentinel {
    /// Starts `program` as `reap` (`kbf-beta`), its input a pipe this holds the end of, its
    /// standard error in `log`.
    ///
    /// # Errors
    /// The log cannot be made or the program cannot be started.
    pub fn start(program: &Path, reap: &Reap, log: &Path) -> std::io::Result<Self> {
        let mut command = Command::new(program);
        command
            .args(reap.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(File::create(log)?);
        // SAFETY: the function runs in the child between fork and exec and makes one system
        // call (setsid), which is async-signal-safe; it allocates nothing.
        unsafe {
            command.pre_exec(own_session);
        }
        let mut child = command.spawn()?;
        let input = child.stdin.take();
        Ok(Self { child, input })
    }

    /// The sentinel's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Sentinel {
    /// Says `done`, closes the pipe, and waits (bounded) for the sentinel to exit.
    fn drop(&mut self) {
        if let Some(mut input) = self.input.take() {
            let _ = writeln!(input, "{DONE}");
        }
        let _ = wait_bounded(&mut self.child, RELEASE_BOUND);
    }
}

#[cfg(test)]
#[path = "reap_tests.rs"]
mod tests;
