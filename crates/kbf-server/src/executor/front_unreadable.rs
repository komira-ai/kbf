//! A stopgap: the declared input an action could not read because its host's file of it
//! answered EIO, read from the action's own standard error
//! ([`Ingest::unreadable_input`](crate::executor::Ingest::unreadable_input)).
//!
//! The job service's result names a job's input failure (`input_failure`) only for a job that did
//! not succeed, and the task tool exits 0 whenever the action ran. So when the file system under
//! the job's tree answers EIO to the compiler opening one of its inputs, the compiler exits 1,
//! the job succeeds, and its result is the action's. This module tells that case apart from a
//! compile that failed, and only when it can do so exactly; anything else is the action's:
//!
//! - the action exited with a code other than 0;
//! - a line of its standard error is the compiler's own could-not-open diagnostic,
//!   `fatal error: cannot open file '<path>': <text>` (clang's `err_cannot_open_file`, as the
//!   toolchain's clang writes it), after nothing or a location ending `: `, with `<text>` the
//!   text of EIO exactly: `I/O error` (musl) or `Input/output error` (glibc, macOS); a
//!   missing file (`No such file or directory`) or a refused one is the action's;
//! - `<path>`, relative to the Command's working directory (or absolute under the job's exec
//!   root, `/kbf-work`), with no `..`, is a file of the action's input root, read from the CAS.
//!
//! At most [`MAX_STDERR_BYTES`] of standard error are read, and a longer one is not looked at;
//! the first [`MAX_NAMED`] such lines are checked. A read of the CAS that fails, a message that
//! does not decode, or a path that leaves the input root answers `None`.
//!
//! It goes once the host reports the failure itself: when the job service's result carries
//! `input_failure` for a job whose tree answered EIO even though the job's process exited 0,
//! [`judge`](super::outcome) reads it before the pack is ingested, and this module, its call in
//! the run and [`Ingest::unreadable_input`](crate::executor::Ingest::unreadable_input) are
//! deleted with their tests.

use kbf_front::Backend;
use kbf_proto::reapi::{Action, ActionResult, Command, Directory};
use prost::Message;

/// The most bytes of an action's standard error read to find the line.
pub(super) const MAX_STDERR_BYTES: u64 = 1 << 20;

/// The most could-not-open lines whose paths are checked against the input root.
pub(super) const MAX_NAMED: usize = 8;

/// The most components a checked path has: deeper is not looked at.
const MAX_DEPTH: usize = 256;

/// What the compiler writes before the path it could not open.
const CANNOT_OPEN: &str = "fatal error: cannot open file '";

/// The texts of EIO as the C libraries spell it: musl, then glibc and macOS.
const EIO_TEXTS: [&str; 2] = ["I/O error", "Input/output error"];

/// The job's exec root inside the job: the task tool's `kbf-work` under the tree root `/`.
const EXEC_ROOT: &str = "/kbf-work/";

/// The paths `stderr` says the compiler could not open with the text of EIO, in order, at most
/// [`MAX_NAMED`].
pub(super) fn named(stderr: &[u8]) -> Vec<String> {
    stderr
        .split(|b| *b == b'\n')
        .filter_map(|line| {
            let line = std::str::from_utf8(line).ok()?;
            cannot_open(line.strip_suffix('\r').unwrap_or(line))
        })
        .take(MAX_NAMED)
        .map(str::to_owned)
        .collect()
}

/// The path of `line` when it is the compiler's could-not-open line with the text of EIO: the
/// first place in it where [`CANNOT_OPEN`] follows nothing or `: ` and is followed by a path, `': `
/// and the text of EIO to the end of the line.
fn cannot_open(line: &str) -> Option<&str> {
    line.match_indices(CANNOT_OPEN).find_map(|(at, _)| {
        let before = &line[..at];
        if !(before.is_empty() || before.ends_with(": ")) {
            return None;
        }
        let rest = &line[at + CANNOT_OPEN.len()..];
        let path = EIO_TEXTS
            .iter()
            .find_map(|text| rest.strip_suffix(text)?.strip_suffix("': "))?;
        (!path.is_empty()).then_some(path)
    })
}

/// The components of `path`, as the action named it from `working_directory`, below the input
/// root; `None` for a path that names `..`, leaves the exec root, names nothing, or is deeper
/// than [`MAX_DEPTH`].
pub(super) fn below_root(working_directory: &str, path: &str) -> Option<Vec<String>> {
    let joined = match path.strip_prefix('/') {
        Some(_) => path.strip_prefix(EXEC_ROOT)?.to_owned(),
        None => format!("{working_directory}/{path}"),
    };
    let mut components = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            part => components.push(part.to_owned()),
        }
    }
    (!components.is_empty() && components.len() <= MAX_DEPTH).then_some(components)
}

/// The message `digest` is, read from the CAS of `backend`; `None` when it cannot be read or
/// does not decode.
async fn read_cas<T: Message + Default>(
    backend: &Backend,
    digest: &kbf_types::Digest,
) -> Option<T> {
    let bytes = backend.read_blob(digest).await.ok()?;
    T::decode(bytes).ok()
}

/// [`read_cas`] of a digest as REAPI spells it; `None` for no digest or one that is not one.
async fn read<T: Message + Default>(
    backend: &Backend,
    digest: Option<&kbf_proto::reapi::Digest>,
) -> Option<T> {
    read_cas(backend, &super::cas_digest(digest?).ok()?).await
}

/// Whether `components` name a file of the tree whose root is `root`.
async fn is_file(backend: &Backend, mut dir: Directory, components: &[String]) -> bool {
    let Some((file, parents)) = components.split_last() else {
        return false;
    };
    for name in parents {
        let child = dir.directories.iter().find(|node| node.name == *name);
        let Some(child) = child else {
            return false;
        };
        let Some(next) = read(backend, child.digest.as_ref()).await else {
            return false;
        };
        dir = next;
    }
    dir.files.iter().any(|node| node.name == *file)
}

/// The declared input of `action` that `result` says the action could not read with EIO: the
/// path as its standard error names it, or `None` (the module's header says when).
pub(super) async fn unreadable_input(
    backend: &Backend,
    action: &kbf_types::Digest,
    result: &ActionResult,
) -> Option<String> {
    if result.exit_code == 0 {
        return None;
    }
    let stderr = if result.stderr_raw.is_empty() {
        let digest = result.stderr_digest.as_ref()?;
        if u64::try_from(digest.size_bytes).ok()? > MAX_STDERR_BYTES {
            return None;
        }
        let digest = super::cas_digest(digest).ok()?;
        backend.read_blob(&digest).await.ok()?.to_vec()
    } else {
        result.stderr_raw.clone()
    };
    let named = named(&stderr);
    if named.is_empty() {
        return None;
    }
    let action: Action = read_cas(backend, action).await?;
    let command: Command = read(backend, action.command_digest.as_ref()).await?;
    let root: Directory = read(backend, action.input_root_digest.as_ref()).await?;
    for path in named {
        let Some(components) = below_root(&command.working_directory, &path) else {
            continue;
        };
        if is_file(backend, root.clone(), &components).await {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
#[path = "front_unreadable_tests.rs"]
mod tests;
