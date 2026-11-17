//! A materialized job (`--materialize-work=TREE`): the job of a system with no root file system
//! to replace (a Mac), which runs natively in its own directory, the job root.
//!
//! The action's tree is mounted read-only (`TREE`, which may itself be a symlink to it, and is
//! followed; nothing under it is) and holds what [`crate::tree`] reads: `.kbf/action`,
//! `.kbf/command` and `kbf-work` (`work` in a tree an older frontend wrote; the name found is the
//! name used below). The task reads and checks the Action and the Command as a tree job
//! does, requires `TREE/kbf-work` to be a directory, and only then touches the job root, so a
//! refusal leaves nothing behind. It then:
//!
//! 1. makes `<root>/kbf-work` (an existing one is refused) and copies `TREE/kbf-work` into it, walking
//!    depth first without following symlinks, with an explicit stack, so a hostile tree cannot
//!    overflow the stack. Each directory is made mode 0755; each regular file is copied with
//!    [`std::fs::copy`], which on macOS first tries an APFS clone (`fclonefileat`) and falls back
//!    to copying the bytes, so a file is cloned where source and destination share an APFS volume
//!    and copied elsewhere; the copy gets mode 0755 if the source has any execute bit, else 0644;
//!    each symlink is recreated with the same target, which is neither checked nor followed, as
//!    in the tree. A fifo, socket or device is refused ([`FetchError::Invalid`] naming its path
//!    relative to `kbf-work`). Every copy is private: the Command may write an input, and an output
//!    may name an input path, without touching the tree. These failures are exit 10;
//! 2. makes the run directory `<root>/.kbf-run` as a tree job does (exit 11);
//! 3. runs the Command in `<root>/kbf-work`, collects its outputs and commits the one object into
//!    the put directory, exactly as [`crate::tree::execute_out_dir`] does.
//!
//! The root is a parameter of every function here (the binary passes the current directory), so
//! tests never change directory. The manifest's advisory `inputs` is `from: fetch` (no new
//! source is named, which would change the manifest's format) with `files` the regular files
//! copied (symlinks are not counted), `directories` the directories made under `kbf-work`
//! (`kbf-work` itself is not counted), and `bytes_written_scratch` the bytes of the regular files.

use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use kbf_task_pack::{InputStats, InputsFrom, TaskManifest};

use crate::inputs::Inputs;
use crate::message::FetchError;
use crate::out_dir::OutDir;
use crate::task::{Cli, Failure, check_lease, collected, exit_code, millis};
use crate::tree::{input_root, input_root_name, make_run_dir, read_tree};

/// What a copy of the tree's `kbf-work` counted.
#[derive(Debug, Default, PartialEq, Eq)]
struct Copied {
    /// Regular files copied.
    files: u64,
    /// Directories made under `kbf-work`.
    directories: u64,
    /// Bytes of the regular files.
    bytes: u64,
}

/// A filesystem error at `path`.
fn io_at(path: &Path) -> impl FnOnce(std::io::Error) -> FetchError + '_ {
    move |source| FetchError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Makes the directory `path`, mode 0755 (the mode is set after the umask has had its say).
fn make_dir(path: &Path) -> Result<(), FetchError> {
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create(path)
        .map_err(io_at(path))?;
    set_mode(path, 0o755)
}

/// Sets the permission bits of `path` to `mode`.
fn set_mode(path: &Path, mode: u32) -> Result<(), FetchError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(io_at(path))
}

/// Copies the directory `from` into the new directory `to`, which is made here, and says what was
/// copied.
fn copy_tree(from: &Path, to: &Path) -> Result<Copied, FetchError> {
    let mut copied = Copied::default();
    make_dir(to)?;
    // (source, destination, path relative to the input root for a message), each a directory already made.
    let mut pending = vec![(from.to_owned(), to.to_owned(), PathBuf::new())];
    while let Some((src, dst, rel)) = pending.pop() {
        for entry in std::fs::read_dir(&src).map_err(io_at(&src))? {
            let name = entry.map_err(io_at(&src))?.file_name();
            let (src, dst, rel) = (src.join(&name), dst.join(&name), rel.join(&name));
            let meta = std::fs::symlink_metadata(&src).map_err(io_at(&src))?;
            let kind = meta.file_type();
            if kind.is_dir() {
                make_dir(&dst)?;
                copied.directories += 1;
                pending.push((src, dst, rel));
            } else if kind.is_file() {
                copied.bytes += std::fs::copy(&src, &dst).map_err(io_at(&src))?;
                let executable = meta.permissions().mode() & 0o111 != 0;
                set_mode(&dst, if executable { 0o755 } else { 0o644 })?;
                copied.files += 1;
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&src).map_err(io_at(&src))?;
                std::os::unix::fs::symlink(target, &dst).map_err(io_at(&dst))?;
            } else {
                return Err(FetchError::Invalid(format!(
                    "{}: not a regular file, directory or symlink",
                    rel.display()
                )));
            }
        }
    }
    Ok(copied)
}

/// Reads the Action and the Command from `tree`, checks its `kbf-work`, copies it to
/// `<root>/kbf-work` and makes the run directory, in this order, so a refused Action, Command or
/// `kbf-work` leaves
/// nothing behind.
fn read(cli: &Cli, tree: &Path, root: &Path) -> Result<Inputs, Failure> {
    let reading = Instant::now();
    let (action, command, network) = read_tree(cli, tree)?;
    let name = input_root_name(tree);
    input_root(&tree.join(name))?;
    let work = root.join(name);
    let copied = copy_tree(&tree.join(name), &work)?;
    let run_dir = make_run_dir(root)?;
    Ok(Inputs {
        action,
        command,
        network,
        root: work,
        hidden: Vec::new(),
        run_dir,
        fetch_ms: millis(reading.elapsed()),
        stats: Some(InputStats {
            from: Some(InputsFrom::Fetch),
            files: copied.files,
            directories: copied.directories,
            bytes_written_scratch: copied.bytes,
            ..InputStats::default()
        }),
    })
}

/// Runs the materialized job `cli` names, over the tree at `tree`, in the job root `root`,
/// writing its one object into the put directory `out`, and returns the manifest committed.
///
/// # Errors
/// A [`Failure`], whose [`Failure::exit_code`] names the step that failed.
pub async fn execute(
    cli: &Cli,
    tree: &Path,
    root: &Path,
    out: &OutDir,
) -> Result<TaskManifest, Failure> {
    let inputs = read(cli, tree, root)?;
    let object = out.object()?;
    let mut manifest = collected(cli, inputs, object.spool()).await?;
    object.finish(&mut manifest).await?;
    Ok(manifest)
}

/// The end of [`crate::task::main_with`] for `--materialize-work`: checks the lease and that
/// `--out-dir` is a directory before anything is read, runs the job and turns the outcome into
/// the exit code.
pub(crate) async fn finish(cli: &Cli, tree: &Path, root: &Path) -> ExitCode {
    let out = check_lease(cli).and_then(|()| OutDir::open(cli.out_dir()));
    let outcome = match out {
        Ok(out) => execute(cli, tree, root, &out).await,
        Err(failure) => Err(failure),
    };
    exit_code(outcome)
}

#[cfg(test)]
#[path = "materialize_tests.rs"]
mod tests;
