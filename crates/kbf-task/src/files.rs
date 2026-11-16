//! The neutral file index of a one-pass object (docs/design/one-pass-task-object.md the end of section 3).
//!
//! It names every output file and symlink, and every directory, by its full path, and says for
//! a file its size, executable bit, SHA-256 and the offset of its bytes in the same object. It is
//! in no consumer's format: not REAPI's (Directory and Tree messages, which hash a folder
//! differently) and not tierstore's (tree manifests). The frontend derives both from it.
//!
//! The JSON, members in this order and no space:
//!
//! ```json
//! {"v":1,
//!  "files":[{"path":"out/d/z","size":3,"exec":false,"sha256":"<hex>","offset":20}],
//!  "symlinks":[{"path":"out/d/l","target":"z"}],
//!  "dirs":["out/d","out/d/sub"]}
//! ```
//!
//! Each list is sorted bytewise by path. `offset` is left out for the empty file, which is no
//! record. `dirs` holds every output directory and every directory below one, so an empty one
//! is not lost. The task builds the index from the output directories' Trees, which it wrote
//! itself and reads back; the top-level files and symlinks come from the manifest.

use std::collections::BTreeMap;

use kbf_proto::reapi::{Digest, Directory, Tree};
use kbf_task_pack::TaskManifest;
use kbf_types::Digest as KbfDigest;
use prost::Message as _;
use serde_json::Value;

use crate::digest;

/// Why the index cannot be made.
#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    /// A digest of a tree is not a SHA-256 digest.
    #[error("the tree names a bad digest: {0}")]
    Digest(String),
    /// A tree names a Directory it does not hold.
    #[error("the tree names the directory {0}, which it does not hold")]
    NoDirectory(String),
    /// A file is not a record of the object.
    #[error("the file {0} is not held by the object")]
    NoFile(String),
}

fn quoted(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

fn parsed(digest: Option<&Digest>) -> Result<KbfDigest, FilesError> {
    let digest = digest.ok_or_else(|| FilesError::Digest("absent".to_owned()))?;
    digest::from_reapi(digest).map_err(|e| FilesError::Digest(e.to_string()))
}

/// What the index holds so far, each list by path.
#[derive(Default)]
struct Index {
    files: BTreeMap<String, String>,
    symlinks: BTreeMap<String, String>,
    dirs: Vec<String>,
}

impl Index {
    fn file(
        &mut self,
        path: &str,
        digest: &KbfDigest,
        exec: bool,
        offset: &dyn Fn(&KbfDigest) -> Option<u64>,
    ) -> Result<(), FilesError> {
        let at = if digest.size_bytes == 0 {
            String::new()
        } else {
            let at = offset(digest).ok_or_else(|| FilesError::NoFile(digest.to_string()))?;
            format!(r#","offset":{at}"#)
        };
        let json = format!(
            r#"{{"path":{},"size":{},"exec":{exec},"sha256":"{}"{at}}}"#,
            quoted(path),
            digest.size_bytes,
            digest.hash_hex()
        );
        self.files.insert(path.to_owned(), json);
        Ok(())
    }

    fn symlink(&mut self, path: &str, target: &str) {
        let json = format!(r#"{{"path":{},"target":{}}}"#, quoted(path), quoted(target));
        self.symlinks.insert(path.to_owned(), json);
    }

    /// Adds `directory`, which is at `path`, and what is below it.
    fn directory(
        &mut self,
        path: &str,
        directory: &Directory,
        held: &BTreeMap<String, &Directory>,
        offset: &dyn Fn(&KbfDigest) -> Option<u64>,
    ) -> Result<(), FilesError> {
        self.dirs.push(path.to_owned());
        for file in &directory.files {
            let digest = parsed(file.digest.as_ref())?;
            self.file(
                &format!("{path}/{}", file.name),
                &digest,
                file.is_executable,
                offset,
            )?;
        }
        for link in &directory.symlinks {
            self.symlink(&format!("{path}/{}", link.name), &link.target);
        }
        for node in &directory.directories {
            let key = node
                .digest
                .as_ref()
                .map(|d| d.hash.clone())
                .unwrap_or_default();
            let child = held.get(&key).ok_or(FilesError::NoDirectory(key))?;
            self.directory(&format!("{path}/{}", node.name), child, held, offset)?;
        }
        Ok(())
    }

    fn json(self) -> Vec<u8> {
        let list = |items: Vec<String>| items.join(",");
        let mut dirs = self.dirs;
        dirs.sort_unstable();
        let dirs: Vec<String> = dirs.iter().map(|d| quoted(d)).collect();
        format!(
            r#"{{"v":1,"files":[{}],"symlinks":[{}],"dirs":[{}]}}"#,
            list(self.files.into_values().collect()),
            list(self.symlinks.into_values().collect()),
            list(dirs)
        )
        .into_bytes()
    }
}

/// The index of `manifest`'s outputs: its top-level files and symlinks, and for each output
/// directory the files, symlinks and directories of its Tree, read from `trees` (the Tree of
/// each output directory by its path). `offset` says where the blob of a digest starts in the
/// object.
///
/// # Errors
/// [`FilesError`] for a tree that is not a tree of records of the object.
pub fn build(
    manifest: &TaskManifest,
    trees: &[(String, Tree)],
    offset: &dyn Fn(&KbfDigest) -> Option<u64>,
) -> Result<Vec<u8>, FilesError> {
    let mut index = Index::default();
    for file in &manifest.output_files {
        index.file(&file.path, &file.digest, file.is_executable, offset)?;
    }
    for link in &manifest.output_symlinks {
        index.symlink(&link.path, &link.target);
    }
    for (path, tree) in trees {
        let held: BTreeMap<String, &Directory> = tree
            .children
            .iter()
            .map(|d| {
                (
                    hex::encode(kbf_segments::sha256(&d.encode_to_vec()).hash),
                    d,
                )
            })
            .collect();
        let root = tree.root.clone().unwrap_or_default();
        index.directory(path, &root, &held, offset)?;
    }
    Ok(index.json())
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
