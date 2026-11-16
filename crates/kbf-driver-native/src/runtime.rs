//! [`NativeRuntime`]: one action, one lease directory, one process tree, all of it gone
//! when the lease ends. See the crate documentation for what a lease gets.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use crate::leader::Leader;
use kbf_daemon::cas::{Cas, CasError, Spool};
use kbf_daemon::tree::{
    TreeError, check_relative, fetch_message, materialize, output_paths, real_dirs,
};
use kbf_daemon::{Runtime, RuntimeError, Work, upload_within};
use kbf_outputs::OutputsError;
use kbf_proto::reapi::{Action, ActionResult, Command, ExecutedActionMetadata};
use kbf_types::LeaseId;
use prost_types::Timestamp;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::cas::CasStore;
use crate::config::NativeConfig;
use crate::grace::{self, Survivor};
use crate::network::{self, network_of};
use crate::procs::{self, Proc, Tracker};
use crate::program::{Refused, Resolved, resolve};
use crate::record::{Exec, KeeperFor, Spawned};

/// The driver name in the node report.
pub const DRIVER: &str = "native";
/// The lease kind this driver runs.
pub const KIND: &str = "action";

/// How long a spawn retries a program file that is still open for writing.
const BUSY_WAIT: Duration = Duration::from_secs(2);
/// The pause between two rounds of SIGKILL while ending an action's processes.
pub(crate) const KILL_PAUSE: Duration = Duration::from_millis(10);

/// A killer waiting for a lease's work to stop and its directory to be removed.
type Stop = oneshot::Sender<()>;

/// Runs `action` leases as process trees on this node.
#[derive(Debug)]
pub struct NativeRuntime<C> {
    config: NativeConfig,
    cas: Arc<C>,
    stops: Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
}

/// Everything `prepare` works out for `execute` and `attempt`.
struct Prepared {
    /// The lease's copy of the input root.
    root: PathBuf,
    work_dir: PathBuf,
    working_directory: String,
    program: PathBuf,
    /// The action's own failure in place of a run: its program could only have been
    /// in the input root and is not there, or not executable ([`crate::program`]).
    refused: Option<Refused>,
    args: Vec<String>,
    /// The lease's own directories' variables ([`crate::home`]), set before `env`.
    lease_env: Vec<(&'static str, OsString)>,
    env: Vec<(String, String)>,
    outputs: Vec<String>,
    timeout: Duration,
}

/// Why a daemon running as root does not start.
const ROOT_DAEMON: &str = "the daemon runs as root: it runs no action as root";

/// Whether the daemon may run actions as it is: never as root, which would run every
/// action as root.
///
/// # Errors
/// What is refused, and why.
fn check_daemon_user(root: bool) -> Result<(), String> {
    if root {
        Err(ROOT_DAEMON.to_owned())
    } else {
        Ok(())
    }
}

impl<C: Cas> NativeRuntime<C> {
    /// A runtime that reads and writes blobs through `cas`. Makes the scratch
    /// directory; kills every action a previous daemon recorded there and left running
    /// ([`crate::record`]); and removes every lease directory it left. All of it is done
    /// when this returns, so before the daemon says `Hello`.
    ///
    /// # Errors
    /// The scratch directory cannot be made or read, the daemon runs as root, or a
    /// recorded action's processes survived SIGKILL for the kill wait. A leftover
    /// directory that cannot be removed is moved aside instead (see the crate
    /// documentation), not an error.
    pub fn new(config: NativeConfig, cas: Arc<C>) -> std::io::Result<Self> {
        Self::with_remover(config, cas, &kbf_outputs::remove_tree)
    }

    /// [`NativeRuntime::new`], sweeping the scratch root with `remove`: a test hands
    /// it a remover that fails, as a directory an action locked would.
    fn with_remover(
        config: NativeConfig,
        cas: Arc<C>,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<Self> {
        check_daemon_user(rustix::process::geteuid().is_root()).map_err(std::io::Error::other)?;
        std::fs::create_dir_all(&config.scratch)?;
        crate::sweep::sweep(
            &config.scratch,
            remove,
            config.kill_wait,
            &crate::sweep::SYSTEM,
        )?;
        // After the sweep, which sets aside a `runs` that is not the daemon's directory.
        let mut runs = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(runs.recursive(true), 0o700)
            .create(config.scratch.join(crate::record::RUNS))?;
        make_spool_dir(&config.scratch.join(SPOOL))?;
        Ok(Self {
            config,
            cas,
            stops: Mutex::new(BTreeMap::new()),
        })
    }

    /// The node report entries this driver adds: how it keeps the network off
    /// ([`network::ISOLATION`]: not at all).
    #[must_use]
    pub fn capabilities(&self) -> Vec<(String, String)> {
        vec![(
            network::CAPABILITY.to_owned(),
            network::ISOLATION.to_owned(),
        )]
    }

    fn stops(&self) -> MutexGuard<'_, BTreeMap<LeaseId, oneshot::Sender<Stop>>> {
        // Each update is one map operation, so the map is consistent after a panic.
        self.stops.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Prepares the lease's directory. A kill during prepare (a long input fetch) stops
    /// it at once; the clean removes whatever was written.
    async fn start(
        &self,
        cas: &C::Lease,
        work: &Work,
        dir: &Path,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<Prepared, RuntimeError> {
        tokio::select! {
            prepared = self.prepare(cas, work, dir) => prepared,
            Ok(by) = &mut *stop => {
                *killer = Some(by);
                Err(RuntimeError::Killed)
            }
        }
    }

    /// Fetches and checks the action through the lease's `cas`, and writes its inputs
    /// into `dir`.
    async fn prepare(
        &self,
        cas: &C::Lease,
        work: &Work,
        dir: &Path,
    ) -> Result<Prepared, RuntimeError> {
        let action: Action = fetch_message(cas, &work.action_digest)
            .await
            .map_err(tree_error)?;
        let command_digest = action
            .command_digest
            .as_ref()
            .ok_or_else(|| RuntimeError::Invalid("the Action names no Command".to_owned()))?;
        let input_root = action
            .input_root_digest
            .as_ref()
            .ok_or_else(|| RuntimeError::Invalid("the Action names no input root".to_owned()))?;
        let command: Command = fetch_message(cas, command_digest)
            .await
            .map_err(tree_error)?;
        let Some((program, args)) = command.arguments.split_first() else {
            return Err(RuntimeError::Invalid(
                "the Command has no arguments".to_owned(),
            ));
        };
        check_relative("working directory", &command.working_directory).map_err(tree_error)?;
        let outputs = output_paths(&command).map_err(tree_error)?;
        let timeout = timeout_of(&action, self.config.default_timeout)?;
        // Checked, not acted on: the driver keeps no network off (`crate::network`).
        network_of(&action, &command)?;

        tokio::fs::create_dir(dir).await.map_err(failed(dir))?;
        let root = dir.join("root");
        tokio::fs::create_dir(&root).await.map_err(failed(&root))?;
        materialize(cas, input_root, &root)
            .await
            .map_err(tree_error)?;
        // REAPI: the worker makes the working directory and each output's parent.
        let work_dir = real_dirs(&root, &command.working_directory)
            .await
            .map_err(tree_error)?;
        for output in &outputs {
            let parent = output.rsplit_once('/').map_or("", |(parent, _)| parent);
            real_dirs(&work_dir, parent).await.map_err(tree_error)?;
        }
        let lease_env = crate::home::make(dir).await.map_err(failed(dir))?;
        let env: Vec<(String, String)> = command
            .environment_variables
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        let (program, refused) = match resolve(program, &work_dir, &env) {
            Ok(Resolved::Program(path)) => (path, None),
            Ok(Resolved::Refused(refused)) => (PathBuf::from(program), Some(refused)),
            Err(why) => return Err(RuntimeError::Failed(why)),
        };
        Ok(Prepared {
            root,
            work_dir,
            working_directory: command.working_directory.clone(),
            program,
            refused,
            args: args.to_vec(),
            lease_env,
            env,
            outputs,
            timeout,
        })
    }

    /// Runs the prepared action until it exits, times out, outgrows its memory limit
    /// or is killed, then ends every process of it. The exit code of an action that
    /// exited.
    async fn execute(
        &self,
        cas: &C::Lease,
        work: &Work,
        dir: &Path,
        prepared: &Prepared,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<i32, RuntimeError> {
        let stdout_path = dir.join("stdout");
        let stderr_path = dir.join("stderr");
        let stdout = std::fs::File::create(&stdout_path).map_err(failed(&stdout_path))?;
        let stderr = std::fs::File::create(&stderr_path).map_err(failed(&stderr_path))?;
        let program = prepared.program.clone();
        // The arguments and the whole environment, the lease's own directories first
        // and the Command's variables winning over them; the child execs these itself.
        let env = (prepared.lease_env.iter())
            .map(|(k, v)| (OsStr::new(k), v.as_os_str()))
            .chain(
                prepared
                    .env
                    .iter()
                    .map(|(k, v)| (OsStr::new(k), OsStr::new(v))),
            );
        let exec = Exec::new(&program, prepared.args.iter().map(OsStr::new), env)
            .map_err(failed(&prepared.program))?;
        let mut command = tokio::process::Command::new(&program);
        command
            .current_dir(&prepared.work_dir)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0);
        let record = crate::record::path(&self.config.scratch, &lease_name(work.lease_id));
        let Spawned {
            child,
            pid: leader,
            // Held until the group has ended (the group's kill ends it too), and
            // dropped after the group: it ends the group if the daemon dies.
            keeper,
        } = spawn(&mut command, exec, &record, KeeperFor::Group)
            .await
            .map_err(failed(&prepared.program))?;
        let mut child = Leader::new(child);
        let me = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        let mut watch =
            Group::new(Tracker::new(leader, me), record).sparing(i32::try_from(keeper.pid()).ok());
        let limit = self.config.memory.limit(work.resources.memory_bytes);
        let deadline = Instant::now() + prepared.timeout;
        let mut tick = tokio::time::interval(self.config.poll);
        // A slow measurement delays the next one rather than bunching them up.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let ended = loop {
            tokio::select! {
                got = child.wait() => break Ended::Exited(got.map(|r| r.status)),
                () = tokio::time::sleep_until(deadline) => break Ended::TimedOut,
                Ok(by) = &mut *stop => {
                    *killer = Some(by);
                    break Ended::Killed;
                }
                _ = tick.tick() => {
                    let used = watch.measure().await?;
                    if let Some(limit) = limit && used > limit {
                        break Ended::OutOfMemory { used, limit };
                    }
                }
            }
        };
        let ignored = watch
            .end(
                &mut child,
                self.config.kill_wait,
                grace_after(&ended, self.config.term_grace),
            )
            .await?;
        self.report_ignored_sigterm(work.lease_id, dir, &ignored);
        match ended {
            Ended::Exited(status) => Ok(exit_code(status.map_err(failed(&program))?)),
            Ended::TimedOut => Err(self.timed_out(cas, work.lease_id, dir).await),
            Ended::Killed => Err(RuntimeError::Killed),
            Ended::OutOfMemory { used, limit } => {
                tracing::warn!(lease = %work.lease_id, used, limit, "killed past its memory limit");
                Err(RuntimeError::OutOfMemory { used, limit })
            }
        }
    }

    /// Names the processes that outlasted SIGTERM ([`grace::note`]) in the node's log and
    /// at the end of the action's stderr: its own hermeticity failure, which does not
    /// change its exit code.
    fn report_ignored_sigterm(&self, lease: LeaseId, dir: &Path, ignored: &[Survivor]) {
        // Both are `None` for no survivor, and both are `Some` otherwise.
        let (Some(note), Some(brief)) = (grace::note(ignored), grace::brief(ignored)) else {
            return;
        };
        tracing::warn!(%lease, "{brief}");
        self.add_to_stderr(lease, dir, &note, "the processes that outlasted SIGTERM");
    }

    /// Appends `note` to the action's stderr in `dir`, unless that pushes it past the
    /// stdio limit, which would drop all of it: it stays in the node's log then. `what`
    /// names the note in that log.
    fn add_to_stderr(&self, lease: LeaseId, dir: &Path, note: &str, what: &str) {
        let stderr = dir.join("stderr");
        let max = self.config.outputs.max_stdio_bytes;
        let size = std::fs::metadata(&stderr).map_or(u64::MAX, |m| m.len());
        let note_len = u64::try_from(note.len()).unwrap_or(u64::MAX);
        if size.saturating_add(note_len) > max {
            tracing::warn!(%lease, size, max, "{what} do not fit in stderr; they are in the node's log");
            return;
        }
        let written = std::fs::OpenOptions::new()
            .append(true)
            .open(&stderr)
            .and_then(|mut file| std::io::Write::write_all(&mut file, note.as_bytes()));
        if let Err(why) = written {
            tracing::warn!(%lease, %why, "{what} could not be added to stderr");
        }
    }

    /// [`RuntimeError::TimedOut`] with what the stopped action wrote to stdout and
    /// stderr in `dir`, each stored within the stdio limit (REAPI's partial result). A
    /// log past the limit, or one that cannot be stored, is left out: the answer is
    /// still the timeout.
    async fn timed_out(&self, cas: &C::Lease, lease: LeaseId, dir: &Path) -> RuntimeError {
        let store = CasStore(cas);
        let max = self.config.outputs.max_stdio_bytes;
        let mut logs = [None, None];
        for (name, slot) in ["stdout", "stderr"].into_iter().zip(&mut logs) {
            match kbf_outputs::store_file(&store, &dir.join(name), max).await {
                Ok(digest) => *slot = Some(digest),
                Err(e) => tracing::warn!(%lease, "timed out; its {name} is left out: {e}"),
            }
        }
        let [stdout, stderr] = logs;
        RuntimeError::TimedOut { stdout, stderr }
    }

    /// Every step but the clean.
    async fn attempt(
        &self,
        work: &Work,
        dir: &Path,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<ActionResult, RuntimeError> {
        let mut clock = ExecutedActionMetadata {
            worker_start_timestamp: now(),
            input_fetch_start_timestamp: now(),
            ..ExecutedActionMetadata::default()
        };
        // The lease's own view of the CAS, for its transfer counters.
        let cas = self.cas.lease(&work.lease_id);
        let prepared = self.start(&cas, work, dir, stop, killer).await?;
        clock.input_fetch_completed_timestamp = now();
        clock.execution_start_timestamp = now();
        let exit_code = match &prepared.refused {
            Some(refused) => refuse(dir, refused).await?,
            None => {
                self.execute(&cas, work, dir, &prepared, stop, killer)
                    .await?
            }
        };
        clock.execution_completed_timestamp = now();

        clock.output_upload_start_timestamp = now();
        // The outputs are gathered in a spool, then stored as whole segments
        // (`docs/design/write-path.md`, section 4): a tree of thousands of files is a
        // few large uploads and one commit, not one upload per file.
        let spools = self.config.scratch.join(SPOOL);
        let spool = Spool::create(&spools).map_err(failed(&spools))?;
        let spool = Arc::new(spool);
        let store = CasStore(&*spool);
        let mut result = ActionResult {
            exit_code,
            ..ActionResult::default()
        };
        let upload = async {
            // From the input root: the action may have replaced its working directory.
            kbf_outputs::collect(
                &store,
                &prepared.root,
                &prepared.working_directory,
                &prepared.outputs,
                self.config.outputs,
                &mut result,
            )
            .await
            .map_err(outputs_error)?;
            let max = self.config.outputs.max_stdio_bytes;
            for (name, slot) in [
                ("stdout", &mut result.stdout_digest),
                ("stderr", &mut result.stderr_digest),
            ] {
                let stored = kbf_outputs::store_file(&store, &dir.join(name), max).await;
                *slot = Some(stored.map_err(outputs_error)?);
            }
            cas.put_spool(Arc::clone(&spool))
                .await
                .map_err(|e| RuntimeError::Failed(format!("storing the outputs: {e}")))
        };
        upload_within(self.config.upload_deadline, upload).await?;
        clock.output_upload_completed_timestamp = now();
        clock.worker_completed_timestamp = now();
        result.execution_metadata = Some(clock);
        Ok(result)
    }
}

impl<C: Cas> Runtime for NativeRuntime<C> {
    fn driver(&self) -> &'static str {
        DRIVER
    }

    fn serves(&self, kind: &str) -> bool {
        kind == KIND
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let (stop_tx, mut stop) = oneshot::channel();
        let _registered = Registered::new(self, work.lease_id, stop_tx);
        // The lease directory drops with the run, whether it ends, is dropped or panics.
        let dir = LeaseDir {
            path: self.config.scratch.join(lease_name(work.lease_id)),
            armed: true,
        };
        let mut killer = None;
        let outcome = self.attempt(&work, &dir.path, &mut stop, &mut killer).await;
        let cleaned = dir.clean().await;
        // A kill that arrived after the work ended still waits for the clean.
        if let Some(by) = killer.or_else(|| stop.try_recv().ok()) {
            let _ = by.send(());
        }
        match (outcome, cleaned) {
            (Ok(_), Err(why)) => Err(RuntimeError::Failed(why)),
            (outcome, Err(why)) => {
                tracing::error!(lease = %work.lease_id, "{why}");
                outcome
            }
            (outcome, Ok(())) => outcome,
        }
    }

    async fn kill(&self, lease_id: LeaseId) {
        let Some(stop) = self.stops().remove(&lease_id) else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        // Answered once the work has stopped and the lease is clean. If the run ended
        // without seeing the kill, `tx` is dropped with it and `rx` returns at once.
        let _ = stop.send(tx);
        let _ = rx.await;
    }
}

/// How the watch over a running action ended.
enum Ended {
    Exited(std::io::Result<ExitStatus>),
    TimedOut,
    Killed,
    OutOfMemory { used: u64, limit: u64 },
}

/// The name of a lease's directory, and of its run record.
fn lease_name(id: LeaseId) -> String {
    format!("lease-{}-{}", id.term, id.seq)
}

/// The processes of one running action, and their run record ([`crate::record`]), which
/// goes once they have all ended. Dropped while armed (the daemon dropped the run), it
/// kills them all, blocking.
struct Group {
    tracker: Arc<Mutex<Tracker>>,
    record: PathBuf,
    /// How the process table is read: [`procs::snapshot`], but for tests.
    snapshot: Snapshot,
    /// The action's keeper, which ignores SIGTERM by design ([`crate::keeper`]): not
    /// asked to end, not waited for and not reported. The SIGKILL takes it with the rest.
    keeper: Option<i32>,
    armed: bool,
}

/// A way to read the process table.
type Snapshot = fn() -> std::io::Result<Vec<Proc>>;

impl Group {
    fn new(tracker: Tracker, record: PathBuf) -> Self {
        Self::reading(tracker, record, procs::snapshot)
    }

    /// [`Group::new`], reading the process table with `snapshot`.
    fn reading(tracker: Tracker, record: PathBuf, snapshot: Snapshot) -> Self {
        Self {
            tracker: Arc::new(Mutex::new(tracker)),
            record,
            snapshot,
            keeper: None,
            armed: true,
        }
    }

    /// This group without `keeper` in the SIGTERM round of [`Group::end`].
    fn sparing(mut self, keeper: Option<i32>) -> Self {
        self.keeper = keeper;
        self
    }

    /// The live processes of the action now, on the blocking pool.
    async fn members(&self) -> Result<Vec<Proc>, RuntimeError> {
        let (tracker, snapshot) = (Arc::clone(&self.tracker), self.snapshot);
        // A task that did not finish (a panic, a runtime shutting down) is an I/O
        // failure like the call's own.
        let members = tokio::task::spawn_blocking(move || members_now(&tracker, snapshot)).await;
        members
            .map_err(std::io::Error::other)
            .and_then(|found| found)
            .map_err(failed(Path::new("process table")))
    }

    /// The memory the action's processes hold together, in bytes.
    async fn measure(&self) -> Result<u64, RuntimeError> {
        let members = self.members().await?;
        tokio::task::spawn_blocking(move || {
            members
                .iter()
                .filter_map(|p| procs::footprint(p.pid))
                .fold(0_u64, u64::saturating_add)
        })
        .await
        .map_err(std::io::Error::other)
        .map_err(failed(Path::new("memory")))
    }

    /// Ends every process of the action: SIGTERM to the group and to each process known
    /// to be the action's, and up to `grace` for them to end ([`grace::term_first`]);
    /// then SIGKILL the same way, again every [`KILL_PAUSE`] until a snapshot shows none
    /// alive, then removes the run record and reaps the leader. Processes still alive
    /// after `wait` of SIGKILL fail the lease, and keep their record for the next sweep.
    /// The processes that were alive when the grace ended.
    async fn end(
        &mut self,
        child: &mut Leader,
        wait: Duration,
        grace: Duration,
    ) -> Result<Vec<Survivor>, RuntimeError> {
        let (tracker, snapshot, spared) = (Arc::clone(&self.tracker), self.snapshot, self.keeper);
        let polite = tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let members = || {
                let mut members = members_now(&tracker, snapshot).map_err(|e| e.to_string())?;
                members.retain(|p| Some(p.pid) != spared);
                Ok::<_, String>(members)
            };
            grace::term_first(
                grace,
                &mut || {
                    let members = members()?;
                    procs::term_all(&lock(&tracker), &members);
                    Ok(())
                },
                &mut || Ok(members()?.iter().map(|p| p.pid).collect()),
                &mut |pid| Survivor::of(pid, procs::command_of(pid)),
                &mut || started.elapsed(),
                &mut || std::thread::sleep(KILL_PAUSE),
            )
        })
        .await;
        let survivors = grace::lenient(
            polite.map_err(|e| e.to_string()).and_then(|round| round),
            "the SIGTERM round",
        );
        let give_up = Instant::now() + wait;
        loop {
            let members = self.members().await?;
            if members.is_empty() {
                break;
            }
            procs::kill_all(&lock(&self.tracker), &members);
            if Instant::now() >= give_up {
                let pids: Vec<String> = members.iter().map(|p| p.pid.to_string()).collect();
                return Err(RuntimeError::Failed(format!(
                    "processes of the action survived SIGKILL for {wait:?}: {}",
                    pids.join(", ")
                )));
            }
            tokio::time::sleep(KILL_PAUSE).await;
        }
        self.armed = false;
        let _ = std::fs::remove_file(&self.record);
        // The leader is dead; reap it if it is not reaped yet.
        child
            .wait()
            .await
            .map_err(failed(Path::new("reap the action")))?;
        Ok(survivors)
    }
}

/// The grace an ending gets: `grace`, except none for a lease past its memory limit: its
/// processes must not keep growing while they are asked to end.
fn grace_after(ended: &Ended, grace: Duration) -> Duration {
    match ended {
        Ended::Exited(_) | Ended::TimedOut | Ended::Killed => grace,
        Ended::OutOfMemory { .. } => Duration::ZERO,
    }
}

fn lock(tracker: &Mutex<Tracker>) -> MutexGuard<'_, Tracker> {
    // A tracker update is a few map operations on data a panic would not leave half
    // written in a way that matters: at worst a process is followed a poll longer.
    tracker.lock().unwrap_or_else(PoisonError::into_inner)
}

fn members_now(tracker: &Mutex<Tracker>, snapshot: Snapshot) -> std::io::Result<Vec<Proc>> {
    let snapshot = snapshot()?;
    Ok(lock(tracker).members(&snapshot))
}

impl Drop for Group {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let ended = (0..100).any(|_| {
            let members = members_now(&self.tracker, self.snapshot).unwrap_or_default();
            procs::kill_all(&lock(&self.tracker), &members);
            std::thread::sleep(KILL_PAUSE);
            members.is_empty()
        });
        // Survivors keep their record, for the next start's sweep.
        let _ = ended.then(|| std::fs::remove_file(&self.record));
        tracing::warn!(ended, "run dropped; killed the action's processes");
    }
}

/// Spawns `command` running `exec`, with its run record written to `record` before
/// the program runs ([`crate::record::Gate`]), retrying while the program is busy: a
/// just-written input file is briefly held open for writing by any child another thread
/// forks before it execs (ETXTBSY). Returns the child, its pid and its keeper.
async fn spawn(
    command: &mut tokio::process::Command,
    exec: Exec,
    record: &Path,
    keeper: KeeperFor,
) -> std::io::Result<Spawned> {
    let gate = crate::record::Gate::install(command, exec);
    gate.spawn_unbusy(command, record, keeper, BUSY_WAIT).await
}

/// Answers `refused` as the action's result without running anything: empty stdout,
/// kbf's message as stderr.
async fn refuse(dir: &Path, refused: &Refused) -> Result<i32, RuntimeError> {
    tracing::info!(dir = %dir.display(), "{}", refused.message);
    for (name, text) in [
        ("stdout", String::new()),
        ("stderr", format!("{}\n", refused.message)),
    ] {
        let path = dir.join(name);
        tokio::fs::write(&path, text).await.map_err(failed(&path))?;
    }
    Ok(refused.exit_code)
}

/// The exit status, or 128 plus the signal number for a process a signal ended (the
/// shell's convention).
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// The lease's directory, removed when the lease ends; by `Drop` if the run is dropped
/// first.
struct LeaseDir {
    path: PathBuf,
    armed: bool,
}

impl LeaseDir {
    /// Removes the directory, off the async threads; what failed, named.
    async fn clean(mut self) -> Result<(), String> {
        self.armed = false;
        let path = self.path.clone();
        let cleaned = tokio::task::spawn_blocking(move || end(&path)).await;
        cleaned.map_err(clean_task_failed)?
    }
}

/// The clean's error when its blocking task did not finish (it panicked, or the
/// runtime shut down before it ran).
fn clean_task_failed(error: tokio::task::JoinError) -> String {
    format!("clean task: {error}")
}

/// Removes the lease directory at `path`; what failed, named.
fn end(path: &Path) -> Result<(), String> {
    kbf_outputs::remove_tree(path).map_err(|e| format!("clean: remove {}: {e}", path.display()))
}

impl Drop for LeaseDir {
    fn drop(&mut self) {
        if self.armed {
            let cleaned = end(&self.path);
            tracing::warn!(dir = %self.path.display(), ?cleaned, "run dropped; cleaned up");
        }
    }
}

/// A lease's entry in the kill table, removed when its run ends or is dropped.
struct Registered<'a, C> {
    runtime: &'a NativeRuntime<C>,
    id: LeaseId,
}

impl<'a, C: Cas> Registered<'a, C> {
    fn new(runtime: &'a NativeRuntime<C>, id: LeaseId, stop: oneshot::Sender<Stop>) -> Self {
        runtime.stops().insert(id, stop);
        Self { runtime, id }
    }
}

impl<C> Drop for Registered<'_, C> {
    fn drop(&mut self) {
        self.runtime
            .stops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

fn now() -> Option<Timestamp> {
    Some(Timestamp::from(SystemTime::now()))
}

/// The action's timeout, or `default` when it names none.
pub(crate) fn timeout_of(action: &Action, default: Duration) -> Result<Duration, RuntimeError> {
    let Some(timeout) = &action.timeout else {
        return Ok(default);
    };
    let (Ok(seconds), Ok(nanos)) = (u64::try_from(timeout.seconds), u32::try_from(timeout.nanos))
    else {
        return Err(RuntimeError::Invalid(
            "the action's timeout is negative".to_owned(),
        ));
    };
    let timeout = Duration::new(seconds, nanos);
    Ok(if timeout.is_zero() { default } else { timeout })
}

/// A [`TreeError`] as the lease reports it: a missing blob is the client's to upload,
/// a broken rule is the client's error, and anything else is the farm's.
pub(crate) fn tree_error(error: TreeError) -> RuntimeError {
    match error {
        TreeError::Cas(CasError::Missing(blob)) => RuntimeError::MissingBlob(blob),
        TreeError::Invalid(why) => RuntimeError::Invalid(why),
        other => RuntimeError::Failed(other.to_string()),
    }
}

/// An [`OutputsError`] as the lease reports it: a bad output path or a name that is
/// not UTF-8 is the action's error (running it again would fail the same way); a
/// limit, a failed read or a failed upload fails the lease, as the container driver
/// reports them.
pub(crate) fn outputs_error(error: OutputsError) -> RuntimeError {
    match error {
        OutputsError::Invalid(why) => RuntimeError::Invalid(why),
        other => RuntimeError::Failed(other.to_string()),
    }
}

/// The daemon's own directory under the scratch root for output spools
/// ([`kbf_daemon::cas::Spool`]), apart from the lease directories: an action that
/// takes write permission from the scratch root (or its lease directory) does not stop
/// its outputs from being stored.
pub const SPOOL: &str = "spool";

/// Makes the spool directory (mode 0700) and removes any spool file a killed daemon
/// left in it between creating and unlinking it.
fn make_spool_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    std::os::unix::fs::DirBuilderExt::mode(builder.recursive(true), 0o700).create(dir)?;
    for entry in std::fs::read_dir(dir)?.flatten() {
        if let Err(why) = std::fs::remove_file(entry.path()) {
            tracing::warn!(path = %entry.path().display(), "a spool file left behind stays: {why}");
        }
    }
    Ok(())
}

/// An I/O failure at `path` as the lease reports it: the farm's.
pub(crate) fn failed(path: &Path) -> impl FnOnce(std::io::Error) -> RuntimeError + '_ {
    move |error| RuntimeError::Failed(format!("{}: {error}", path.display()))
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
