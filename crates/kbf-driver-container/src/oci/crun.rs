//! `crun`, driven through the `runc` crate (containerd/rust-extensions, 0.3, the async
//! client): the OCI runtime command line `crun run --bundle`, `kill` and `delete`.
//!
//! What the crate does and does not give this driver, checked in its source (0.3.0):
//! - `GlobalOpts::command(path)` sets the binary every call runs, so `crun` is used as
//!   it is for `runc` (`crun` takes the same global options `--root`, `--log` and
//!   `--log-format`, and the same `run`/`kill`/`delete` verbs). `--rootless` is left
//!   unset: `crun` finds out itself, and has no such option.
//! - It has no `--cgroup-manager` option; `crun`'s default is `cgroupfs`, which the
//!   daemon's delegated subtree needs.
//! - `Runc::run` waits for the process and returns `Error::CommandFailed { status, .. }`
//!   for a non-zero exit, so the container's exit status is `crun run`'s own: no
//!   `state` query and no window in which the container is stopped but its exit code is
//!   not recorded (kbf#658).
//! - It runs its commands with `tokio::process`, without `kill_on_drop`; [`KillOnDrop`]
//!   is the spawner that adds it, so a `run` that is dropped or aborted does not leave
//!   `crun` behind.
//! - Its `Io` hook sets the child's stdin/stdout/stderr; [`FileIo`] points the
//!   container's stdout and stderr at the lease's files instead of the crate's pipes
//!   (an action's output is not held in memory).
//! - The synchronous client is the same crate without its `async` feature, so the two
//!   cannot be built together: the blocking paths (a dropped lease's clean) use
//!   `std::process` with the same arguments ([`Crun::delete_blocking`]).

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use runc::error::Error;
use runc::options::{CreateOpts, GlobalOpts};
use runc::{Command, Io, Runc, Spawner};

use crate::credential::without_credentials;

/// How one `crun run` ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// `crun run` exited with this status: the container's exit code (`128 + n` if it
    /// was killed by signal `n`), or `crun`'s own failure, told apart by the log.
    Exited(i32),
    /// `crun` could not be run, or its task did not finish.
    Failed(String),
}

/// The exit status a shell would report for `status`.
pub(crate) fn code_of(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// How long a dropped child is waited for after its `SIGKILL`. A killed process exits
/// once it leaves the system call it is in; a child still there at the limit is left
/// to tokio's orphan queue (`kill_on_drop`).
const REAP_ON_DROP: Duration = Duration::from_secs(5);

/// A running `crun`, killed and reaped when dropped. tokio's `kill_on_drop` alone sends
/// the `SIGKILL` and leaves the reaping to its orphan queue, so an aborted `crun run`
/// could still be there, dying or a zombie, when the lease clean went on to `crun
/// delete` its container: the delete then raced the run's own removal of its state.
/// Dropping blocks (microseconds for a process that has the signal pending) on the
/// thread that polled the future.
struct Reaped(tokio::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
        wait_exited(&mut self.0, REAP_ON_DROP);
    }
}

/// Polls `child` until it has exited (and so is reaped) or `limit` has passed.
fn wait_exited(child: &mut tokio::process::Child, limit: Duration) {
    let deadline = Instant::now() + limit;
    // `Ok(None)`: not exited yet. An error is not a process to wait for.
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The `Spawner` of this driver: [`runc::DefaultExecutor`], with `kill_on_drop` and a
/// child that is reaped when its future is dropped ([`Reaped`]).
#[derive(Debug)]
struct KillOnDrop;

#[async_trait]
impl Spawner for KillOnDrop {
    async fn execute(&self, mut cmd: Command) -> runc::Result<(ExitStatus, u32, String, String)> {
        cmd.kill_on_drop(true);
        without_credentials(cmd.as_std_mut());
        let mut child = cmd.spawn().map_err(Error::ProcessSpawnFailed)?;
        let pid = child.id().unwrap_or(0);
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let mut child = Reaped(child);
        let waiting = child.0.wait();
        let (status, out, err) = tokio::join!(waiting, read_all(stdout), read_all(stderr));
        let status = status.map_err(Error::InvalidCommand)?;
        Ok((
            status,
            pid,
            String::from_utf8_lossy(&out.map_err(Error::InvalidCommand)?).into_owned(),
            String::from_utf8_lossy(&err.map_err(Error::InvalidCommand)?).into_owned(),
        ))
    }
}

/// Everything `pipe` says until it closes; nothing for a stream that was not piped.
async fn read_all<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}

/// `Io` that gives `crun run` the lease's stdout and stderr files, and as stdin the file
/// the container's init reports a program it could not start in (`oci::init`).
#[derive(Debug)]
struct FileIo {
    stdin: std::fs::File,
    stdout: std::fs::File,
    stderr: std::fs::File,
}

#[async_trait]
impl Io for FileIo {
    async fn set(&self, cmd: &mut Command) -> std::io::Result<()> {
        cmd.stdin(self.stdin.try_clone()?);
        cmd.stdout(self.stdout.try_clone()?);
        cmd.stderr(self.stderr.try_clone()?);
        Ok(())
    }

    async fn close_after_start(&self) {}
}

/// The `crun` program and how the driver calls it.
#[derive(Clone, Debug)]
pub(crate) struct Crun {
    program: PathBuf,
}

/// The state directory of a lease's `crun` (`--root`), inside the lease directory: no
/// state is shared between actions, and removing the lease removes it.
pub(crate) fn state_dir(lease: &Path) -> PathBuf {
    lease.join("crun")
}

/// `crun`'s log of a lease (`--log`, JSON).
pub(crate) fn log_path(lease: &Path) -> PathBuf {
    lease.join("crun.log")
}

impl Crun {
    pub(crate) fn new(program: PathBuf) -> Self {
        Self { program }
    }

    /// The client for the lease in `lease`: its own state directory and log.
    fn client(&self, lease: &Path) -> Result<Runc, String> {
        let mut opts = GlobalOpts::new()
            .command(&self.program)
            .root(state_dir(lease))
            .log(log_path(lease))
            .log_json();
        opts.custom_spawner(Arc::new(KillOnDrop));
        opts.build().map_err(|e| self.refused(&e))
    }

    /// Runs the container `id` of the bundle in `bundle` to its end (`crun run`: create,
    /// start and delete in one process), its stdout and stderr in the given files. The
    /// result is `crun`'s exit status. Dropping the future kills `crun`.
    pub(crate) async fn run(
        &self,
        lease: &Path,
        id: &str,
        bundle: &Path,
        stdin: std::fs::File,
        stdout: std::fs::File,
        stderr: std::fs::File,
    ) -> RunEnd {
        let client = match self.client(lease) {
            Ok(client) => client,
            Err(e) => return RunEnd::Failed(e),
        };
        let opts = CreateOpts::new().io(Arc::new(FileIo {
            stdin,
            stdout,
            stderr,
        }));
        match client.run(id, bundle, Some(&opts)).await {
            Ok(()) => RunEnd::Exited(0),
            Err(Error::CommandFailed { status, .. }) => RunEnd::Exited(code_of(status)),
            Err(e) => RunEnd::Failed(format!("crun run: {e}")),
        }
    }

    /// Runs `bundle` as a helper (`crun run`, output captured, nothing in a log file):
    /// `Ok` once it exits 0, otherwise what it printed.
    pub(crate) async fn run_helper(
        &self,
        state: &Path,
        id: &str,
        bundle: &Path,
    ) -> Result<(), String> {
        let log = state.with_extension("log");
        let mut opts = GlobalOpts::new()
            .command(&self.program)
            .root(state)
            .debug(true)
            .log(&log)
            .log_json();
        opts.custom_spawner(Arc::new(KillOnDrop));
        let client = opts.build().map_err(|e| self.refused(&e))?;
        match client.run(id, bundle, None).await {
            Ok(()) => Ok(()),
            Err(Error::CommandFailed {
                status,
                stdout,
                stderr,
            }) => {
                // What `crun` itself logged: the reason it failed, as opposed to the
                // helper's own words on stderr.
                let logged = std::fs::read_to_string(&log).unwrap_or_default();
                let tail = logged.len().saturating_sub(3000);
                Err(format!(
                    "crun run (helper {id}): {status}: {} {} {} [log: {}]",
                    stdout.trim(),
                    stderr.trim(),
                    crate::program::crun_words(&logged),
                    logged.get(tail..).unwrap_or_default().replace('\n', " ")
                ))
            }
            Err(e) => Err(format!("crun run (helper {id}): {e}")),
        }
    }

    /// Sends signal `signal` (a number) to the container's main process.
    pub(crate) async fn kill(&self, lease: &Path, id: &str, signal: u32) -> Result<(), String> {
        let client = self.client(lease)?;
        client
            .kill(id, signal, None)
            .await
            .map_err(|e| format!("crun kill: {e}"))
    }

    /// Deletes the container `id`, killing it first if it runs (`delete --force`); one
    /// `crun` does not know is not an error with `--force`. `crun run` deletes its
    /// container as it exits, so this finds nothing on the normal path; it is what removes
    /// the container, its cgroup and its state after a `crun` that was killed. Blocking:
    /// a dropped lease cleans up from `Drop`.
    pub(crate) fn delete_blocking(&self, lease: &Path, id: &str) -> Result<(), String> {
        let state = state_dir(lease);
        let deleted = self.blocking(&state, &["delete", "--force", id], "delete");
        // A `crun run` killed with its container (a sweep's case) deletes its own state as
        // it exits, and a `crun delete` that lost that race fails on what it listed or
        // loaded (the directory, or the `config.json` the other removed first, with the
        // directory itself still there for a moment): once the state is gone there is
        // nothing left to delete. The state of a delete that failed for another reason
        // stays, and that failure is returned after the wait.
        if deleted.is_err() && wait_gone(&state.join(id), STATE_REMOVAL_WAIT) {
            return Ok(());
        }
        deleted
    }

    /// Ends every `crun run` of the lease in `lease` that is still there, and returns once
    /// each has ended. A `crun run` is found by its command line (`--root` the lease's
    /// state directory): the sweep of a restarted daemon is not its parent, so it cannot
    /// reap it as a lease's own clean does ([`Running::reap`]), and left alone it ends on
    /// its own after its container is gone and writes into the lease directory on the way
    /// out, recreating the state directory the sweep removed. Killed, it cannot. Blocking.
    pub(crate) fn end_runs_blocking(&self, lease: &Path) -> Result<(), String> {
        let pids = runs_with_root(&state_dir(lease));
        for pid in &pids {
            // Already gone, or not ours to signal: the wait below says which.
            let _ = rustix::process::kill_process(*pid, rustix::process::Signal::KILL);
        }
        wait_ended(&pids, RUN_END_LIMIT, RUN_END_PAUSE).map_err(|left| {
            let shown: Vec<_> = left
                .iter()
                .map(|p| {
                    let raw = p.as_raw_nonzero();
                    let status = std::fs::read_to_string(format!("/proc/{raw}/status"));
                    let state = (status.as_deref().unwrap_or_default().lines())
                        .find(|l| l.starts_with("State:"))
                        .map_or("", |l| l.trim_start_matches("State:").trim());
                    format!("{raw} ({state})")
                })
                .collect();
            format!(
                "crun run (pid {}) still runs {RUN_END_LIMIT:?} after it was killed",
                shown.join(", ")
            )
        })
    }

    /// Runs the helper bundle blocking: for the clean of a lease that has files only a
    /// user namespace can delete.
    pub(crate) fn run_helper_blocking(
        &self,
        state: &Path,
        id: &str,
        bundle: &Path,
    ) -> Result<(), String> {
        let bundle = bundle.to_string_lossy();
        self.blocking(state, &["run", "--bundle", &bundle, id], "run (helper)")
    }

    /// Runs `crun --root <state> <args>` and waits: `Ok` if it exits 0, else its status
    /// and what it said on stderr.
    fn blocking(&self, state: &Path, args: &[&str], what: &str) -> Result<(), String> {
        let mut command = std::process::Command::new(&self.program);
        let output = without_credentials(&mut command)
            .arg("--root")
            .arg(state)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(format!(
                "crun {what}: {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Err(format!("run {}: {e}", self.program.display())),
        }
    }

    /// The words for a client `crun` could not be found for.
    fn refused(&self, error: &Error) -> String {
        format!("crun {}: {error}", self.program.display())
    }
}

/// How long a killed `crun run` is waited for, and how often it is looked at. A killed
/// process ends once it leaves the system call it is in.
const RUN_END_LIMIT: Duration = Duration::from_secs(5);
const RUN_END_PAUSE: Duration = Duration::from_millis(2);

/// The processes whose command line has `--root <root>` (or `--root=<root>`): the
/// `crun` calls on one lease's state directory, whatever their verb.
fn runs_with_root(root: &Path) -> Vec<rustix::process::Pid> {
    use std::os::unix::ffi::OsStrExt as _;
    let root = root.as_os_str().as_bytes();
    let entries = std::fs::read_dir("/proc").into_iter().flatten().flatten();
    let pids = entries.filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok());
    pids.filter_map(rustix::process::Pid::from_raw)
        .filter(|pid| pid.as_raw_nonzero().get().cast_unsigned() != std::process::id())
        .filter(|pid| {
            let argv = std::fs::read(format!("/proc/{}/cmdline", pid.as_raw_nonzero())).ok();
            argv.is_some_and(|argv| has_root(&argv, root))
        })
        .collect()
}

/// Whether the command line `argv` (arguments separated by NUL) has `--root <root>` or
/// `--root=<root>`.
fn has_root(argv: &[u8], root: &[u8]) -> bool {
    let args: Vec<&[u8]> = argv.split(|b| *b == 0).collect();
    let mut equals = b"--root=".to_vec();
    equals.extend_from_slice(root);
    args.windows(2).any(|w| w[0] == b"--root" && w[1] == root) || args.contains(&equals.as_slice())
}

/// Whether process `pid` has ended: gone, or a zombie (or dead) waiting to be reaped.
fn ended(pid: rustix::process::Pid) -> bool {
    std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_nonzero()))
        .ok()
        .and_then(|stat| Some(stat.rsplit_once(") ")?.1.starts_with(['Z', 'X', 'x'])))
        .unwrap_or(true)
}

/// Polls until every process in `pids` has ended, or `limit` has passed: the ones that
/// have not, then.
fn wait_ended(
    pids: &[rustix::process::Pid],
    limit: Duration,
    pause: Duration,
) -> Result<(), Vec<rustix::process::Pid>> {
    let deadline = Instant::now() + limit;
    loop {
        let left: Vec<_> = pids.iter().copied().filter(|p| !ended(*p)).collect();
        if left.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(left);
        }
        std::thread::sleep(pause);
    }
}

/// How long a failed `crun delete` waits for the container's state to be removed by
/// the `crun run` that owns it. That run is removing it already; the wait ends the moment
/// the directory is gone.
const STATE_REMOVAL_WAIT: Duration = Duration::from_secs(3);

/// Whether `path` is gone, polling until `limit` has passed.
fn wait_gone(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if !path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Catches `crun`'s status being read as anything but a shell reads it: an exit code
    /// as it is, a death by signal `n` as `128 + n` (what the OOM classification looks for
    /// at 137), so a killed `crun` is not an exit 0.
    #[test]
    fn a_status_is_the_exit_code_or_128_plus_the_signal() {
        assert_eq!(code_of(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(code_of(ExitStatus::from_raw(0)), 0);
        assert_eq!(code_of(ExitStatus::from_raw(9)), 137);
        assert_eq!(code_of(ExitStatus::from_raw(15)), 143);
    }

    /// A process that follows `/dev/null` for ever and names `args` after it as files that
    /// do not exist, so its command line carries them: the shape of a `crun run` on a
    /// lease's state directory. (The coreutils may be one multi-call binary that reads
    /// its own name, so the name cannot be set instead.)
    fn sleeper(args: &[&str]) -> std::process::Child {
        use std::process::Stdio;
        std::process::Command::new("tail")
            .args(["-f", "/dev/null", "--"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn")
    }

    fn pid_of(child: &std::process::Child) -> rustix::process::Pid {
        rustix::process::Pid::from_raw(i32::try_from(child.id()).expect("pid")).expect("pid")
    }

    /// Catches a command line read wrongly: `--root` and the path as two arguments or one,
    /// not a longer path with the same prefix, not the path as another option's value, and
    /// not the path alone.
    #[test]
    fn a_command_line_names_a_root_as_the_option_and_its_value() {
        let root = b"/scratch/kbf-lease-1-1/crun";
        assert!(has_root(
            b"crun\0--root\0/scratch/kbf-lease-1-1/crun\0run\0",
            root
        ));
        assert!(has_root(
            b"crun\0--root=/scratch/kbf-lease-1-1/crun\0run",
            root
        ));
        assert!(!has_root(
            b"crun\0--root\0/scratch/kbf-lease-1-1/crun2\0run",
            root
        ));
        assert!(!has_root(
            b"crun\0--root=/scratch/kbf-lease-1-1/crun2\0run",
            root
        ));
        assert!(!has_root(
            b"crun\0--log\0/scratch/kbf-lease-1-1/crun\0run",
            root
        ));
        assert!(!has_root(
            b"crun\0/scratch/kbf-lease-1-1/crun\0--root",
            root
        ));
    }

    /// Catches a sweep that leaves a dead daemon's `crun run` to end on its own (it writes
    /// into the lease directory on the way out, after the sweep removed it), one that
    /// returns before the process has ended, and one that kills a `crun` of another lease
    /// (a longer path of the same prefix) or a process with the path as another option's
    /// value. A stopped process is the straggler: it is not ended until it is killed.
    #[test]
    fn the_runs_of_one_lease_are_killed_and_waited_for_and_no_other() {
        let dir = Path::new("/scratch/kbf-lease-1-1");
        let root = state_dir(dir);
        let root = root.to_str().expect("utf-8");
        let mut run = sleeper(&["--root", root]);
        let mut joined = sleeper(&[&format!("--root={root}")]);
        let mut other = sleeper(&["--root", &format!("{root}2")]);
        let mut value = sleeper(&["--log", root]);
        for child in [&run, &joined] {
            rustix::process::kill_process(pid_of(child), rustix::process::Signal::STOP)
                .expect("stop");
        }
        Crun::new(PathBuf::from("crun"))
            .end_runs_blocking(dir)
            .expect("ended");
        assert!(ended(pid_of(&run)), "a crun run with --root DIR was left");
        assert!(
            ended(pid_of(&joined)),
            "a crun run with --root=DIR was left"
        );
        assert!(!ended(pid_of(&other)), "another lease's crun was killed");
        assert!(
            !ended(pid_of(&value)),
            "a process with the path elsewhere was killed"
        );
        for child in [&mut run, &mut joined, &mut other, &mut value] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Catches a wait that reports a process that still runs as ended, and one that
    /// waits for ever: a limit of zero returns after one look.
    #[test]
    fn a_process_that_still_runs_after_the_limit_is_reported() {
        let mut child = sleeper(&[]);
        let pid = pid_of(&child);
        let left = wait_ended(&[pid], Duration::ZERO, Duration::from_millis(1));
        assert_eq!(left, Err(vec![pid]));
        child.kill().expect("kill");
        child.wait().expect("wait");
        assert_eq!(
            wait_ended(&[pid], Duration::from_secs(5), Duration::from_millis(1)),
            Ok(())
        );
    }

    /// Catches a lease with no `crun run` left (the usual sweep) being an error.
    #[test]
    fn a_lease_with_no_run_has_none_to_end() {
        Crun::new(PathBuf::from("crun"))
            .end_runs_blocking(Path::new("/scratch/kbf-lease-no-such-lease"))
            .expect("nothing to end");
    }

    /// Catches a `crun` that is aborted (its future dropped, which `Running::force` does
    /// when the grace after `cgroup.kill` runs out) and is still there, alive or a
    /// zombie, when the drop returns: the clean then ran `crun delete` beside a
    /// `crun run` that had not been reaped. Each round polls `execute` once (which
    /// spawns `sleep`, no timer: a zero timeout polls its future first), finds the new
    /// child in the thread's `children` list, drops the future, and looks for the pid at
    /// once. Rounds repeat so a child that dies fast enough on its own cannot hide a
    /// driver that does not wait.
    #[test]
    fn a_dropped_crun_is_reaped_before_the_drop_returns() {
        let children = format!(
            "/proc/self/task/{}/children",
            rustix::thread::gettid().as_raw_nonzero()
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut seen = std::collections::HashSet::new();
        for round in 0..25 {
            let mut command = Command::new("sleep");
            command.arg("600");
            let mut run = Box::pin(KillOnDrop.execute(command));
            let polled =
                runtime.block_on(async { tokio::time::timeout(Duration::ZERO, &mut run).await });
            assert!(polled.is_err(), "round {round}: sleep 600 ended");
            let listed = std::fs::read_to_string(&children).expect("the thread's children");
            let new: Vec<_> = listed
                .split_whitespace()
                .filter(|p| seen.insert((*p).to_owned()))
                .collect();
            assert_eq!(
                new.len(),
                1,
                "round {round}: new children {new:?} of {listed:?}"
            );
            let pid = new[0];
            drop(run);
            assert!(
                !Path::new("/proc").join(pid).exists(),
                "round {round}: pid {pid} is still there after its future was dropped"
            );
        }
    }

    /// Catches a wait for a killed child that returns early or never returns: with the
    /// limit passed it gives up on a child that still runs (and leaves it running), and
    /// with room it returns once the killed child has been reaped.
    #[test]
    fn the_wait_for_a_child_is_bounded_and_ends_when_it_is_reaped() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut child = Command::new("sleep").arg("600").spawn().expect("sleep");
            wait_exited(&mut child, Duration::from_millis(20));
            assert!(
                matches!(child.try_wait(), Ok(None)),
                "a child that was not killed is gone"
            );
            child.start_kill().expect("kill");
            wait_exited(&mut child, Duration::from_secs(30));
            assert!(
                matches!(child.try_wait(), Ok(Some(_))),
                "the killed child is not reaped"
            );
        });
    }

    /// The `Io` hook `crun run` is given has nothing to close once the child has the
    /// files (the crate calls it for `create` and `exec`, not `run`).
    #[tokio::test]
    async fn the_file_io_has_nothing_to_close_after_the_start() {
        let null = || std::fs::File::open("/dev/null").expect("/dev/null");
        FileIo {
            stdin: null(),
            stdout: null(),
            stderr: null(),
        }
        .close_after_start()
        .await;
    }
}
