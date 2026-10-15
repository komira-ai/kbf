//! Files in and out of a lease: the input root written from the CAS into a directory,
//! and the action's outputs read back into the CAS.
//!
//! The input root is written by `kbf_daemon::tree`, the module the native driver uses
//! too, with its rules: Directory messages come from clients, so every name is checked
//! before it touches the host's filesystem (a name with a slash, `.`, `..` or a NUL, or
//! a name used twice in one directory, refuses the action), and files are created with
//! `O_EXCL` in directories that module created, so no write follows a symlink the input
//! tree planted. Its errors come back as this module's [`TreeError`], kind for kind.
//!
//! What is the container driver's own: the overlay's preparation
//! ([`refuse_hidden_working_directory`], [`prepare_outputs`], which moves each input an
//! output names into the upper directory) and the outputs, read by descriptor,
//! component by component from the upper directory, with no symlink followed at any
//! level, within [`OutputLimits`].

use std::path::{Path, PathBuf};

use kbf_daemon::cas::{Cas, CasError};
use kbf_daemon::cpu_pool::CpuPool;
use kbf_daemon::tree as shared;
use kbf_proto::reapi::{ActionResult, Command, Digest};
use prost::Message;
use tokio::fs;

pub use crate::outputs::OutputLimits;

/// Why the input root could not be written or the outputs read.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// The action's tree or paths break a rule: the client's error.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Cas(#[from] CasError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The action's outputs pass one of the [`OutputLimits`]: the action fails, the
    /// daemon goes on.
    #[error("{path}: the action's outputs exceed the limit on {what} ({limit})")]
    Limit {
        path: PathBuf,
        what: Exceeded,
        limit: u64,
    },
    /// An output holds a name or symlink target that is not UTF-8, which REAPI cannot
    /// record: the action fails, the daemon goes on.
    #[error(
        "{path}: the action's output has a {what} that is not UTF-8, which REAPI cannot record"
    )]
    NotUtf8 { path: PathBuf, what: &'static str },
}

/// Which of the [`OutputLimits`] an action's outputs passed, and the flag that sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exceeded {
    Depth,
    Entries,
    Bytes,
    /// The bytes of stdout or of stderr.
    Stdio,
}

impl std::fmt::Display for Exceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Depth => "directory depth, --output-max-depth",
            Self::Entries => "entries, --output-max-entries",
            Self::Bytes => "file bytes, --output-max-bytes",
            Self::Stdio => "stdout or stderr bytes, --output-max-stdio-bytes",
        })
    }
}

/// The shared module's error, kind for kind.
impl From<shared::TreeError> for TreeError {
    fn from(error: shared::TreeError) -> Self {
        match error {
            shared::TreeError::Invalid(why) => Self::Invalid(why),
            shared::TreeError::Cas(e) => Self::Cas(e),
            shared::TreeError::Io { path, source } => Self::Io { path, source },
        }
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> TreeError + '_ {
    move |source| TreeError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Fetches a blob and decodes it as `M` (`kbf_daemon::tree::fetch_message`).
pub(crate) async fn fetch_message<M: Message + Default>(
    cas: &impl Cas,
    digest: &Digest,
) -> Result<M, TreeError> {
    Ok(shared::fetch_message(cas, digest).await?)
}

/// Checks a path the Command names (`kbf_daemon::tree::check_relative`).
pub(crate) fn check_relative(what: &str, path: &str) -> Result<(), TreeError> {
    Ok(shared::check_relative(what, path)?)
}

/// Writes the tree under `root` into the empty directory `dir`
/// (`kbf_daemon::tree::materialize`).
///
/// # Errors
/// A blob is missing or corrupt, the tree breaks a naming rule, or a write fails.
pub async fn materialize(cas: &impl Cas, root: &Digest, dir: &Path) -> Result<(), TreeError> {
    Ok(shared::materialize(cas, root, dir).await?)
}

/// Writes the tree under `root` into the empty directory `dir` through the node's
/// input cache (`kbf_daemon::tree::materialize_cached`): files are hard links to
/// read-only cache entries, but those at or under one of `private` (paths from `dir`),
/// which are private, writable copies. What the inputs cost.
///
/// # Errors
/// As [`materialize`].
pub async fn materialize_cached(
    cas: &impl Cas,
    cache: &kbf_daemon::cache::Cache,
    root: &Digest,
    dir: &Path,
    private: &[String],
) -> Result<kbf_daemon::cache::InputStats, TreeError> {
    Ok(shared::materialize_cached(cas, cache, root, dir, private).await?)
}

/// The output paths a Command declares, each checked
/// (`kbf_daemon::tree::output_paths`).
///
/// # Errors
/// A path is empty, absolute, or contains `.` or `..`.
pub fn output_paths(command: &Command) -> Result<Vec<String>, TreeError> {
    Ok(shared::output_paths(command)?)
}

/// Makes the overlay's upper directory `upper` ready for the outputs: every entry of
/// the input root `root` that an output path names (seen from `working_directory`) is
/// moved, whole, from `root` into `upper`; then the working directory and each output's
/// parent are made in `upper` (REAPI: the worker makes them).
///
/// The driver reads outputs from the upper directory, which holds only what the action
/// created or changed. An output that is also an input would come back partial there:
/// an unchanged input file missing, an output directory without its unchanged inputs,
/// a deleted input as a whiteout. Moved before the overlay is mounted, the input is in
/// the upper directory alone: the action sees the same tree, its changes land in place,
/// a file it deletes is unlinked (no lower entry to hide, so no whiteout), and the
/// output is read back as the action left it, as the native driver reads it from its
/// input root.
///
/// Outputs are taken ancestors first, so an output below another one was moved with
/// it. The walk follows no symlink. A component that is absent, or that is not a
/// directory, ends it with nothing to move: the parent made in the upper directory
/// hides a lower file or symlink of the same name instead of merging with it. The
/// parents are made without following a symlink either, so an output below an input
/// file or symlink that another output moved up is refused as
/// [`TreeError::Invalid`], as the native driver refuses it.
///
/// # Errors
/// [`TreeError::Invalid`] as above; I/O failures, an unreadable input directory
/// included.
pub async fn prepare_outputs(
    root: &Path,
    upper: &Path,
    working_directory: &str,
    outputs: &[String],
) -> Result<(), TreeError> {
    // Component lists, ancestors first: a path's own list sorts before any it prefixes.
    // Collected before the walk: a closure-holding iterator kept across an await makes
    // the future not `Send` for every lifetime.
    let mut paths: Vec<Vec<&str>> = outputs
        .iter()
        .map(|output| output.split('/').collect())
        .collect();
    paths.sort();
    let base: Vec<&str> = working_directory
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    // No output is an ancestor of the working directory, so no move touches its path.
    let out_dir = shared::real_dirs(upper, working_directory).await?;
    for path in &paths {
        let mut lower = root.to_owned();
        let parts = base.iter().chain(path);
        let last = base.len() + path.len() - 1;
        for (i, part) in parts.enumerate() {
            lower.push(part);
            let meta = match fs::symlink_metadata(&lower).await {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(io(&lower)(e)),
            };
            if i == last {
                let parent = path[..path.len() - 1].join("/");
                let to = shared::real_dirs(&out_dir, &parent)
                    .await?
                    .join(path[path.len() - 1]);
                fs::rename(&lower, &to).await.map_err(io(&lower))?;
                break;
            }
            if !meta.is_dir() {
                break;
            }
        }
    }
    for output in outputs {
        let parent = output.rsplit_once('/').map_or("", |(parent, _)| parent);
        shared::real_dirs(&out_dir, parent).await?;
    }
    Ok(())
}

/// Refuses a working directory that the input root `root` has as anything but
/// directories: a symlink or a file on its path.
///
/// The driver makes the working directory in the overlay's upper directory, and an
/// upper directory hides a lower symlink or file of the same name, so the action would
/// start in an empty directory rather than among its inputs. The walk follows no
/// symlink; a component that is absent ends it (the driver makes the rest).
pub async fn refuse_hidden_working_directory(
    root: &Path,
    working_directory: &str,
) -> Result<(), TreeError> {
    let mut host = root.to_owned();
    for part in working_directory.split('/').filter(|part| !part.is_empty()) {
        host.push(part);
        match fs::symlink_metadata(&host).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(TreeError::Invalid(format!(
                    "working directory {working_directory:?} is not a directory of the \
                     input root (a symlink or file is on its path)"
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(io(&host)(e)),
        }
    }
    Ok(())
}

/// Reads each output path, under `working_directory` in the overlay's upper directory
/// `upper`, into the CAS and records it in `result`.
///
/// Nothing is followed: every component from `upper` down, the working directory's
/// included, is opened without following a symlink, and a declared output that is a
/// symlink is recorded as an `OutputSymlink` (see `outputs`). A path the action did
/// not create is left out, including one whose parent (or working directory) the
/// action replaced with a file or a symlink; an entry that is not a file, directory or
/// symlink (a socket, a FIFO, a whiteout) is left out too. A working directory or
/// output path with an empty, `.` or `..` component is refused. Every input an output
/// names was moved into the upper directory before the action ran
/// ([`prepare_outputs`]), so every output is whole there.
///
/// All the outputs together stay within `limits` (directory depth, entries, file
/// bytes), or the call fails with [`TreeError::Limit`]. An output directory is walked
/// without recursion, so no depth of tree can overflow the caller's stack. The walk
/// hashes each file exactly once, on `pool`, and closes it; the blobs (each distinct
/// digest once) are then stored with [`Cas::put_records`], which reads each file again
/// only to send it and refuses one that is not the file that was hashed; the call
/// returns once all of them are stored. A name or symlink target that is not UTF-8
/// fails the call with [`TreeError::NotUtf8`].
pub async fn collect(
    cas: &impl Cas,
    upper: &Path,
    pool: &CpuPool,
    working_directory: &str,
    paths: &[String],
    limits: OutputLimits,
    result: &mut ActionResult,
) -> Result<(), TreeError> {
    crate::outputs::collect(cas, upper, pool, working_directory, paths, limits, result).await
}
