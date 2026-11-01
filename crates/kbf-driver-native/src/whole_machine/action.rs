//! The whole-machine lease's action, in its user's GUI session: the lease directory,
//! the run through the helper, the timeout and the kill, and the outputs.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use kbf_daemon::cas::Cas;
use kbf_daemon::tree::{check_relative, fetch_message, materialize, output_paths, real_dirs};
use kbf_daemon::{RuntimeError, Work, upload_within};
use kbf_proto::reapi::{Action, ActionResult, Command, ExecutedActionMetadata};
use kbf_proto::worker::ResourceUsage;
use tokio::sync::oneshot;

use super::state::{Record, Step};
use super::{Session, Stop, WholeMachineRuntime, failed_at, io_at};
use crate::cas::CasStore;
use crate::runtime::{failed, outputs_error, timeout_of, tree_error};

/// How the action's run ended.
enum Ended {
    Exited(Result<super::Exit, String>),
    TimedOut,
    Killed(Stop),
}

impl<S: Session, C: Cas> WholeMachineRuntime<S, C> {
    /// Fetches the action, writes its inputs into the lease directory, runs it in the
    /// lease user's GUI session until it exits, times out or is killed, and reads its
    /// outputs. Records the step `started` just before the `run`. The lease directory
    /// is left for the restore to remove.
    pub(super) async fn run_in_session(
        &self,
        work: &Work,
        record: &mut Record,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<ActionResult, RuntimeError> {
        let id = work.lease_id;
        // The lease's own view of the CAS, for its transfer counters.
        let cas = &self.cas.lease(&id);
        let action: Action = fetch_message(cas, &work.action_digest)
            .await
            .map_err(tree_error)?;
        let (Some(command_digest), Some(input_root)) = (
            action.command_digest.as_ref(),
            action.input_root_digest.as_ref(),
        ) else {
            return Err(RuntimeError::Invalid(
                "the Action names no Command or no input root".to_owned(),
            ));
        };
        let command: Command = fetch_message(cas, command_digest)
            .await
            .map_err(tree_error)?;
        if command.arguments.is_empty() {
            return Err(RuntimeError::Invalid(
                "the Command has no arguments".to_owned(),
            ));
        }
        check_relative("working directory", &command.working_directory).map_err(tree_error)?;
        let outputs = output_paths(&command).map_err(tree_error)?;
        let timeout = timeout_of(&action, self.config.default_timeout)?;

        let dir = self.lease_dir(id);
        remove_dir(&dir).await.map_err(RuntimeError::Failed)?;
        tokio::fs::create_dir(&dir).await.map_err(failed(&dir))?;
        let shared = dir.clone();
        self.helper(move |s| s.share(id, &shared))
            .await
            .map_err(failed_at("sharing the lease directory"))?;
        let root = dir.join("root");
        tokio::fs::create_dir(&root).await.map_err(failed(&root))?;
        materialize(cas, input_root, &root)
            .await
            .map_err(tree_error)?;
        let work_dir = real_dirs(&root, &command.working_directory)
            .await
            .map_err(tree_error)?;
        for output in &outputs {
            let parent = output.rsplit_once('/').map_or("", |(parent, _)| parent);
            real_dirs(&work_dir, parent).await.map_err(tree_error)?;
        }

        let open = |path: &Path, create: bool| -> Result<OwnedFd, RuntimeError> {
            let file = if create {
                File::create(path)
            } else {
                File::open(path)
            };
            file.map(OwnedFd::from).map_err(failed(path))
        };
        let fds = [
            open(Path::new("/dev/null"), false)?,
            open(&dir.join("stdout"), true)?,
            open(&dir.join("stderr"), true)?,
            open(&work_dir, false)?,
        ];
        let argv = command.arguments.clone();
        let env: Vec<(String, String)> = command
            .environment_variables
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        // Kept first: a daemon that restarts from here on must not run it again.
        record.step = Step::Started;
        self.keep(record).map_err(RuntimeError::Failed)?;
        let started = Instant::now();
        let running = self
            .helper(move |s| s.run(id, &argv, &env, fds))
            .await
            .map_err(failed_at("run"))?;
        let session = Arc::clone(&self.session);
        let mut exited = tokio::task::spawn_blocking(move || session.wait(running));
        let ended = tokio::select! {
            exit = &mut exited => Ended::Exited(exit.unwrap_or_else(join_failed)),
            () = tokio::time::sleep(timeout) => Ended::TimedOut,
            Ok(by) = &mut *stop => Ended::Killed(by),
        };
        let usage = ResourceUsage {
            wall_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            ..ResourceUsage::default()
        };
        let exit = match ended {
            Ended::Exited(exit) => exit.map_err(failed_at("waiting for the action"))?,
            Ended::TimedOut => {
                self.keep_failed(id, usage);
                self.end(id, exited).await?;
                return Err(self.timed_out(cas, &dir).await);
            }
            Ended::Killed(by) => {
                *killer = Some(by);
                self.keep_failed(id, usage);
                self.end(id, exited).await?;
                return Err(RuntimeError::Killed);
            }
        };

        let store = CasStore(cas);
        let mut result = ActionResult {
            exit_code: exit.action_code(),
            ..ActionResult::default()
        };
        let upload = async {
            kbf_outputs::collect(
                &store,
                &root,
                &command.working_directory,
                &outputs,
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
            Ok(())
        };
        if let Err(error) = upload_within(self.config.upload_deadline, upload).await {
            // The action ran, but the lease has no result to carry what it used.
            self.keep_failed(id, usage);
            return Err(error);
        }
        result.execution_metadata = Some(ExecutedActionMetadata {
            auxiliary_metadata: vec![usage.to_any()],
            ..ExecutedActionMetadata::default()
        });
        Ok(result)
    }

    /// Ends every process of the lease user (the action's first) and waits for the
    /// action's run to be reaped.
    async fn end(
        &self,
        id: kbf_types::LeaseId,
        exited: tokio::task::JoinHandle<Result<super::Exit, String>>,
    ) -> Result<(), RuntimeError> {
        self.helper(move |s| s.kill_uid(id))
            .await
            .map_err(failed_at("kill-uid"))?;
        let _ = exited.await;
        Ok(())
    }

    /// The timeout, with what the action wrote to stdout and stderr in `dir` (each left
    /// out if it cannot be stored).
    async fn timed_out(&self, cas: &C::Lease, dir: &Path) -> RuntimeError {
        let store = CasStore(cas);
        let max = self.config.outputs.max_stdio_bytes;
        let stdout = kbf_outputs::store_file(&store, &dir.join("stdout"), max)
            .await
            .ok();
        let stderr = kbf_outputs::store_file(&store, &dir.join("stderr"), max)
            .await
            .ok();
        RuntimeError::TimedOut { stdout, stderr }
    }
}

/// A blocking task that did not finish (it panicked, or the runtime is shutting down).
pub(super) fn join_failed<T>(error: tokio::task::JoinError) -> Result<T, String> {
    Err(format!("the blocking call did not finish: {error}"))
}

/// Removes the lease directory at `dir` if it is there, off the async threads.
pub(super) async fn remove_dir(dir: &Path) -> Result<(), String> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || {
        if !dir.exists() {
            return Ok(());
        }
        kbf_outputs::remove_tree(&dir).map_err(io_at(format!("remove {}", dir.display())))
    })
    .await
    .unwrap_or_else(join_failed)
}
