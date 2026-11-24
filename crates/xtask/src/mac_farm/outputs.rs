//! The outputs of a farm action (`--output`): the paths the Command declares (`output_paths`,
//! relative to the checkout's root), and their download from the `ActionResult` once the
//! command has passed, into a directory of the runner at the same paths.
//!
//! Every declared path must come back as a file or a directory, and nothing else may: a path
//! the command did not leave, a path the farm names that was not declared, and a symlink (an
//! answered one, or one inside an output directory) each fail the run. A file is written as its
//! bytes arrive, hashed on the way, and fails the run when its hash or size is not its digest's;
//! a file that failed is removed. Nothing is overwritten: a file or directory already at an output's place fails the run.

#[cfg(test)]
#[path = "outputs_tests.rs"]
mod tests;

use std::collections::BTreeMap;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use kbf_proto::reapi::{ActionResult, Digest, Directory, Tree};
use prost::Message as _;

use super::Problem;
use super::client::{Farm, FarmError};
use super::tree::{components, digest_of};

/// Why the outputs of a passed command could not be written.
#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("the command left no output {0:?}")]
    Missing(String),
    #[error("the farm answered an output {0:?} the command did not declare")]
    Undeclared(String),
    #[error("the output {0:?} is a symlink: mac-farm writes files and directories only")]
    Symlink(String),
    #[error("the output directory {dir:?} has an entry {name:?}, which is not a plain name")]
    BadName { dir: String, name: String },
    #[error("the tree of the output directory {0:?} lacks a directory it names")]
    NoDirectory(String),
    #[error("the tree of the output directory {path:?} does not decode: {why}")]
    Garbage { path: String, why: String },
    #[error(
        "the output {path:?} is {got_size} bytes of {got}, not the {size} bytes of {hash} the farm named"
    )]
    Corrupt {
        path: String,
        hash: String,
        size: i64,
        got: String,
        got_size: i64,
    },
    #[error("write the output {path:?}: {why}")]
    Write { path: String, why: String },
    #[error(transparent)]
    Farm(#[from] FarmError),
}

/// What [`download`] wrote.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub files: usize,
    pub bytes: i64,
}

/// The `--output` paths as the Command declares them: sorted.
///
/// # Errors
/// [`Problem`] for a path that is not a relative path of plain components, one given twice,
/// and one inside another (the farm answers each declared path once, as one file or one
/// directory).
pub fn declared(outputs: &[String]) -> Result<Vec<String>, Problem> {
    let mut sorted = outputs.to_vec();
    sorted.sort();
    for (n, path) in sorted.iter().enumerate() {
        if components(path).is_err() {
            return Err(Problem::OutputPath(path.clone()));
        }
        for other in &sorted[..n] {
            if other == path {
                return Err(Problem::OutputTwice(path.clone()));
            }
            if path.starts_with(&format!("{other}/")) {
                return Err(Problem::OutputInside {
                    inner: path.clone(),
                    outer: other.clone(),
                });
            }
        }
    }
    Ok(sorted)
}

/// Writes the outputs `result` answers for the paths `declared` into `into`, each at its path.
///
/// # Errors
/// [`OutputError`] for the first output that is missing, undeclared, a symlink, not its digest,
/// or that cannot be read or written; what was written before it stays.
pub async fn download(
    farm: &Farm,
    result: &ActionResult,
    declared: &[String],
    into: &Path,
) -> Result<Written, OutputError> {
    if let Some(link) = result.output_symlinks.first() {
        return Err(OutputError::Symlink(link.path.clone()));
    }
    let answered: Vec<&String> = (result.output_files.iter().map(|f| &f.path))
        .chain(result.output_directories.iter().map(|d| &d.path))
        .collect();
    if let Some(path) = answered.iter().find(|p| !declared.contains(p)) {
        return Err(OutputError::Undeclared((*path).clone()));
    }
    if let Some(path) = declared.iter().find(|p| !answered.contains(p)) {
        return Err(OutputError::Missing(path.clone()));
    }
    let mut written = Written::default();
    for file in &result.output_files {
        parent(into, &file.path)?;
        let digest = file.digest.clone().unwrap_or_default();
        let to = into.join(&file.path);
        write(
            farm,
            &digest,
            &to,
            file.is_executable,
            &file.path,
            &mut written,
        )
        .await?;
    }
    for dir in &result.output_directories {
        parent(into, &dir.path)?;
        let digest = dir.tree_digest.clone().unwrap_or_default();
        let tree = Tree::decode(farm.read(&digest).await?.as_slice()).map_err(|e| {
            OutputError::Garbage {
                path: dir.path.clone(),
                why: e.to_string(),
            }
        })?;
        // Keyed by the digest computed here, so a tree names only directories it holds.
        let children: BTreeMap<String, Directory> = (tree.children.into_iter())
            .map(|c| (digest_of(&c.encode_to_vec()).hash, c))
            .collect();
        let mut todo = vec![(tree.root.unwrap_or_default(), dir.path.clone())];
        while let Some((node, at)) = todo.pop() {
            let place = into.join(&at);
            std::fs::create_dir(&place).map_err(|e| OutputError::Write {
                path: at.clone(),
                why: e.to_string(),
            })?;
            if let Some(link) = node.symlinks.first() {
                return Err(OutputError::Symlink(format!("{at}/{}", link.name)));
            }
            for file in &node.files {
                plain(&at, &file.name)?;
                let shown = format!("{at}/{}", file.name);
                let digest = file.digest.clone().unwrap_or_default();
                let to = place.join(&file.name);
                write(farm, &digest, &to, file.is_executable, &shown, &mut written).await?;
            }
            for sub in &node.directories {
                plain(&at, &sub.name)?;
                let hash = sub.digest.as_ref().map(|d| d.hash.as_str()).unwrap_or("");
                let child = children
                    .get(hash)
                    .ok_or_else(|| OutputError::NoDirectory(dir.path.clone()))?;
                todo.push((child.clone(), format!("{at}/{}", sub.name)));
            }
        }
    }
    Ok(written)
}

/// Makes the directories above the output `path` in `into`.
fn parent(into: &Path, path: &str) -> Result<(), OutputError> {
    let above = path.rsplit_once('/').map(|(d, _)| d).unwrap_or_default();
    std::fs::create_dir_all(into.join(above)).map_err(|e| OutputError::Write {
        path: path.to_owned(),
        why: e.to_string(),
    })
}

/// `name` is one plain component.
fn plain(dir: &str, name: &str) -> Result<(), OutputError> {
    match components(name) {
        Ok(parts) if parts.len() == 1 => Ok(()),
        _ => Err(OutputError::BadName {
            dir: dir.to_owned(),
            name: name.to_owned(),
        }),
    }
}

/// Writes the blob `digest` as a new file `to` (`shown` in messages), executable or not.
async fn write(
    farm: &Farm,
    digest: &Digest,
    to: &Path,
    executable: bool,
    shown: &str,
    written: &mut Written,
) -> Result<(), OutputError> {
    let failed = |e: std::io::Error| OutputError::Write {
        path: shown.to_owned(),
        why: e.to_string(),
    };
    let mode = if executable { 0o755 } else { 0o644 };
    // Unbuffered: the farm's messages are the writes, so nothing is left to flush.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(to)
        .map_err(failed)?;
    let got = match farm.read_into(digest, &mut file).await {
        Ok(got) if got == *digest => got,
        // A file that is not its digest's whole bytes is not left under the output's name.
        failed => {
            let _ = std::fs::remove_file(to);
            return Err(match failed {
                Ok(got) => OutputError::Corrupt {
                    path: shown.to_owned(),
                    hash: digest.hash.clone(),
                    size: digest.size_bytes,
                    got: got.hash,
                    got_size: got.size_bytes,
                },
                Err(e) => e.into(),
            });
        }
    };
    written.files += 1;
    written.bytes += got.size_bytes;
    Ok(())
}
