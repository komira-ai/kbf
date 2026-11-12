//! A tree job (`--tree`): the job's root file system is the action's tree, so the task fetches
//! nothing and holds no read grant.
//!
//! The frontend writes one tree per action, and the job's root is that tree, read-only, under a
//! writable scratch layer. Under the root (`--tree`'s value, `/` in a job) the task finds:
//!
//! - `.kbf/action` and `.kbf/command`: the Action and its Command, each a file whose bytes are
//!   the blob. The Action is checked against `--action` (its size, then its SHA-256), so a job
//!   never runs another action's tree; the Command against the Action's `command_digest`. Each
//!   is held in memory, at most [`MAX_MESSAGE_BYTES`](crate::fetch::MAX_MESSAGE_BYTES), and a
//!   file longer than its digest says is refused at the first byte past the size;
//! - `work`: the input root, the Command's exec root. Its files are not hashed again: the tree is
//!   the frontend's, written from the input root it checked, under keys that are the manifests'
//!   own SHA-256.
//!
//! The task makes `.kbf-run`, its own run directory, beside them (an existing one is refused):
//! the Command's `stdout`, `stderr` and `private/` (its `HOME`, `TMPDIR` and caches) are there,
//! outside `work`, so nothing written there is an output. Then the run is today's: the working
//! directory and the parent of every output path are made under `work` before the Command
//! starts, the Command runs in the same confinement and sandbox ([`crate::sandbox`]), which here
//! hides the job's grants (`/run/js-files`) and `.kbf-run` (its `private/` bound again) rather
//! than the parent of `--work-dir`, its outputs are collected from `work`, and
//! the manifest and outputs are stored through the same sink. The manifest's advisory `inputs`
//! is `{"from":"tree"}` with every number 0 (nothing was fetched), which makes it version 2.
//!
//! With `--stage-and-commit` there is no sink: the packs and large objects are written into the
//! put directory `--out-dir` and committed there ([`crate::out_dir`]), which the sandbox also
//! hides from the Command. `--out-dir` must be a directory before anything is read (exit 14).
//!
//! A failure to read the Action, the Command or `work` is exit 10, as a fetch failure is; a run
//! directory that cannot be made is exit 11, as the private directories are.

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use kbf_proto::reapi::{Action, Command};
use kbf_task_pack::{InputStats, InputsFrom, TaskManifest};

use crate::digest;
use crate::fetch::{FetchError, file_message};
use crate::inputs::{Inputs, network_of};
use crate::out_dir::OutDir;
use crate::plan::Limits;
use crate::run::RunError;
use crate::sandbox::GRANTS_DIR;
use crate::task::{
    Cli, Failure, HttpSink, PackSink, check_argv_secrets, check_lease, check_slots, collected,
    complete, exit_code, http_sink, millis, named, put_urls, sink_limits, trusted_roots,
};

/// The Action's file, under the tree's root.
pub const ACTION_FILE: &str = ".kbf/action";
/// The Command's file, under the tree's root.
pub const COMMAND_FILE: &str = ".kbf/command";
/// The input root, under the tree's root.
pub const INPUT_ROOT: &str = "work";
/// The task's run directory, made under the tree's root.
pub const RUN_DIR: &str = ".kbf-run";

/// What the manifest of a tree job says of its inputs: they came from the tree, and nothing was
/// fetched.
#[must_use]
pub fn input_stats() -> InputStats {
    InputStats {
        from: Some(InputsFrom::Tree),
        ..InputStats::default()
    }
}

/// Runs the tree job `cli` names over the tree at `root` (the value of `--tree`), storing
/// through `sink`, and returns the manifest that was stored.
///
/// # Errors
/// A [`Failure`], whose [`Failure::exit_code`] names the step that failed.
pub async fn execute(cli: &Cli, root: &Path, sink: &dyn PackSink) -> Result<TaskManifest, Failure> {
    let limits = sink_limits(sink);
    check_slots(&limits)?;
    let inputs = read(cli, root)?;
    complete(cli, inputs, sink, &limits).await
}

/// Reads the Action and the Command from the tree under `root`, checks the input root, and makes
/// the run directory: in this order, so a refused Action, Command or network property leaves
/// nothing behind.
fn read(cli: &Cli, root: &Path) -> Result<Inputs, Failure> {
    let reading = Instant::now();
    let action: Action = file_message(&root.join(ACTION_FILE), &digest::to_reapi(&cli.action))?;
    let command: Command = file_message(
        &root.join(COMMAND_FILE),
        &named("command", action.command_digest.as_ref())?,
    )?;
    let network = network_of(cli, &action, &command)?;
    // The tree's `work` is the input root; the Action must still name one, as REAPI requires.
    named("input root", action.input_root_digest.as_ref())?;
    let work = root.join(INPUT_ROOT);
    input_root(&work)?;
    let run_dir = root.join(RUN_DIR);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&run_dir)
        .map_err(|e| RunError::Io {
            path: run_dir.clone(),
            source: std::io::Error::new(
                e.kind(),
                format!("cannot make the run directory (an existing one is refused): {e}"),
            ),
        })?;
    // The job's grants and the task's run directory, out of the Command's sight (its `private/`
    // is bound again by the sandbox); and the put directory, which only the task writes, once
    // the Command has ended.
    let mut hidden = vec![PathBuf::from(GRANTS_DIR), run_dir.clone()];
    if cli.stage_and_commit {
        hidden.push(cli.out_dir().to_owned());
    }
    Ok(Inputs {
        action,
        command,
        network,
        root: work,
        hidden,
        run_dir,
        fetch_ms: millis(reading.elapsed()),
        stats: Some(input_stats()),
    })
}

/// Refuses an input root that is not a directory: a file or a symlink (not followed), or one
/// that cannot be looked at (missing).
fn input_root(work: &Path) -> Result<(), FetchError> {
    match std::fs::symlink_metadata(work) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(FetchError::Invalid(format!(
            "{}: the tree's input root is not a directory",
            work.display()
        ))),
        Err(source) => Err(FetchError::Io {
            path: work.to_owned(),
            source,
        }),
    }
}

/// The real sink of a tree job (`--tree`), after the checks of today's job that do not concern
/// the read grant: a tree job has none.
pub(crate) fn connect(cli: &Cli) -> Result<HttpSink, Failure> {
    check_lease(cli)?;
    check_argv_secrets(cli)?;
    let roots = trusted_roots(cli)?;
    let urls = put_urls(cli)?;
    Ok(http_sink(cli, urls, &roots))
}

/// Runs the tree job `cli` names over the tree at `root`, within `limits`, writing its packs and
/// large objects into the put directory `out` ([`crate::out_dir`]) instead of a sink, and
/// returns the manifest that was committed. With `--stage-and-commit` the put directory
/// `--out-dir` is hidden from the Command with the grants and the run directory.
///
/// # Errors
/// A [`Failure`], whose [`Failure::exit_code`] names the step that failed.
pub async fn execute_out_dir(
    cli: &Cli,
    root: &Path,
    out: &OutDir,
    limits: &Limits,
) -> Result<TaskManifest, Failure> {
    check_slots(limits)?;
    let inputs = read(cli, root)?;
    let (mut manifest, blobs) = collected(cli, inputs, limits).await?;
    out.store(&mut manifest, &blobs, limits)?;
    Ok(manifest)
}

/// The end of [`crate::task::main_with`] for a tree job with `--stage-and-commit`: checks the
/// lease and that `--out-dir` is a directory before anything runs, runs the job with a run's real
/// slots ([`Limits::real`]) and turns the outcome into the exit code.
pub(crate) async fn finish_out_dir(cli: &Cli, root: &Path) -> ExitCode {
    let out = check_lease(cli).and_then(|()| OutDir::open(cli.out_dir()));
    let outcome = match out {
        Ok(out) => execute_out_dir(cli, root, &out, &Limits::real()).await,
        Err(failure) => Err(failure),
    };
    exit_code(outcome)
}

/// The end of [`crate::task::main_with`] for a tree job rooted at `root`: runs it over `sink` (what [`connect`] made,
/// or why it could not) and turns the outcome into the exit code.
pub(crate) async fn finish(
    cli: &Cli,
    root: &Path,
    sink: Result<Box<dyn PackSink>, Failure>,
) -> ExitCode {
    let outcome = match sink {
        Ok(sink) => execute(cli, root, sink.as_ref()).await,
        Err(failure) => Err(failure),
    };
    exit_code(outcome)
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
