//! A tree job (`--tree`): the job's root file system is the action's tree, so the task fetches
//! nothing and holds no read grant. The other kind of job, `--materialize-work`, is
//! [`crate::materialize`]; it shares this module's reading of the Action and the Command.
//!
//! The frontend writes one tree per action, and the job's root is that tree, read-only, under a
//! writable scratch layer. Under the root (`--tree`'s value, `/` in a job) the task finds:
//!
//! - `.kbf/action` and `.kbf/command`: the Action and its Command, each a file whose bytes are
//!   the blob. The Action is checked against `--action` (its size, then its SHA-256), so a job
//!   never runs another action's tree; the Command against the Action's `command_digest`. Each
//!   is held in memory, at most [`MAX_MESSAGE_BYTES`](crate::message::MAX_MESSAGE_BYTES), and a
//!   file longer than its digest says is refused at the first byte past the size;
//! - `kbf-work` ([`INPUT_ROOT`]; `work` in a tree an older frontend wrote): the input root, the
//!   Command's exec root, `/kbf-work` in a job. Its files are not hashed again: the tree is
//!   the frontend's, written from the input root it checked, under keys that are the manifests'
//!   own SHA-256.
//!
//! The task makes `.kbf-run`, its own run directory, beside them (an existing one is refused):
//! the Command's `stdout`, `stderr` and `private/` (its `HOME`, `TMPDIR` and caches) are there,
//! outside `kbf-work`, so nothing written there is an output. Then the working
//! directory and the parent of every output path are made under `kbf-work` before the Command
//! starts, the Command is started directly in the job, which is its sandbox (no namespace, filter,
//! cover or relay of the task's: [`crate::run::Job`]), its outputs are collected from `kbf-work`, and
//! the run's one object is written into the put directory `--out-dir` and committed there
//! ([`crate::out_dir`]), which the Command can see (the job's own output path). `--out-dir` must
//! be a directory before anything is read (exit 14). The manifest's advisory `inputs` is
//! `{"from":"tree"}` with every number 0 (nothing was fetched), which makes it version 2.
//!
//! A failure to read the Action, the Command or `kbf-work` is exit 10, as a fetch failure is; a run
//! directory that cannot be made is exit 11, as the private directories are.

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use kbf_proto::reapi::{Action, Command};
use kbf_task_pack::{InputStats, InputsFrom, TaskManifest};

use crate::digest;
use crate::inputs::{Inputs, network_of};
use crate::message::{FetchError, file_message};
use crate::out_dir::OutDir;
use crate::run::RunError;
use crate::task::{Cli, Failure, check_lease, collected, exit_code, millis, named};

/// The Action's file, under the tree's root.
pub const ACTION_FILE: &str = ".kbf/action";
/// The Command's file, under the tree's root.
pub const COMMAND_FILE: &str = ".kbf/command";
/// The input root, under the tree's root: `/kbf-work` in a job, the Command's exec root. Its
/// path is 9 bytes, so a tool that rewrites the exec root in place for a path of at least 8
/// bytes (a debug-info relocation of a coverage build) can, where it could not with `/work`.
pub const INPUT_ROOT: &str = "kbf-work";
/// The input root of a tree an older frontend wrote, read when the tree has no [`INPUT_ROOT`].
pub(crate) const OLDER_INPUT_ROOT: &str = "work";
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

/// Reads the Action and the Command from the tree under `root`, checks the network property, and
/// requires the Action to name an input root: the part of a read every kind of job shares, which
/// leaves nothing behind.
pub(crate) fn read_tree(
    cli: &Cli,
    root: &Path,
) -> Result<(Action, Command, crate::network::Network), Failure> {
    let action: Action = file_message(&root.join(ACTION_FILE), &digest::to_reapi(&cli.action))?;
    let command: Command = file_message(
        &root.join(COMMAND_FILE),
        &named("command", action.command_digest.as_ref())?,
    )?;
    let network = network_of(cli, &action, &command)?;
    // The tree's `kbf-work` is the input root; the Action must still name one, as REAPI requires.
    named("input root", action.input_root_digest.as_ref())?;
    Ok((action, command, network))
}

/// The name of the input root of the tree under `tree`: [`INPUT_ROOT`], or [`OLDER_INPUT_ROOT`]
/// when the tree has no entry of the first name and has one of the second (a tree an older
/// frontend wrote). With neither, [`INPUT_ROOT`], which [`input_root`] then refuses by name.
#[must_use]
pub(crate) fn input_root_name(tree: &Path) -> &'static str {
    let has = |name: &str| std::fs::symlink_metadata(tree.join(name)).is_ok();
    if !has(INPUT_ROOT) && has(OLDER_INPUT_ROOT) {
        OLDER_INPUT_ROOT
    } else {
        INPUT_ROOT
    }
}

/// Makes the run directory `<root>/.kbf-run`, mode 0700; an existing one is refused.
pub(crate) fn make_run_dir(root: &Path) -> Result<PathBuf, RunError> {
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
    Ok(run_dir)
}

/// Reads the Action and the Command from the tree under `root`, checks the input root, and makes
/// the run directory: in this order, so a refused Action, Command or network property leaves
/// nothing behind.
fn read(cli: &Cli, root: &Path) -> Result<Inputs, Failure> {
    let reading = Instant::now();
    let (action, command, network) = read_tree(cli, root)?;
    let work = root.join(input_root_name(root));
    input_root(&work)?;
    let run_dir = make_run_dir(root)?;
    Ok(Inputs {
        action,
        command,
        network,
        root: work,
        // Nothing is hidden: the Command is started directly, in the job (`Run::job`).
        hidden: Vec::new(),
        run_dir,
        fetch_ms: millis(reading.elapsed()),
        stats: Some(input_stats()),
    })
}

/// Refuses an input root that is not a directory: a file or a symlink (not followed), or one
/// that cannot be looked at (missing).
pub(crate) fn input_root(work: &Path) -> Result<(), FetchError> {
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

/// Runs the tree job `cli` names over the tree at `root`, writing its one object into the put
/// directory `out` ([`crate::out_dir`]), and returns the manifest that was committed.
///
/// # Errors
/// A [`Failure`], whose [`Failure::exit_code`] names the step that failed.
pub async fn execute_out_dir(
    cli: &Cli,
    root: &Path,
    out: &OutDir,
) -> Result<TaskManifest, Failure> {
    // One object, written as the outputs are read.
    let inputs = read(cli, root)?;
    let object = out.object()?;
    let mut manifest = collected(cli, inputs, object.spool()).await?;
    object.finish(&mut manifest).await?;
    Ok(manifest)
}

/// The end of [`crate::task::main_with`]: checks the lease and that `--out-dir` is a directory
/// before anything runs, runs the job and turns the outcome into the exit code.
pub(crate) async fn finish_out_dir(cli: &Cli, root: &Path) -> ExitCode {
    let out = check_lease(cli).and_then(|()| OutDir::open(cli.out_dir()));
    let outcome = match out {
        Ok(out) => execute_out_dir(cli, root, &out).await,
        Err(failure) => Err(failure),
    };
    exit_code(outcome)
}

#[cfg(test)]
#[path = "tree_tests.rs"]
pub(crate) mod tests;
