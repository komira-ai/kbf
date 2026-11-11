//! The bytes of a tree manifest (tierstore's `tree_manifest`, version 1), and the check of each
//! by tierstore's own reader before it is stored.
//!
//! A child is written with its members in one order, `name`, `kind`, then the kind's own
//! (`sha256`, `size`, `exec` and `source` for a file, `sha256` and `size` for a directory,
//! `target` for a symlink), with no space, so the same tree gives the same bytes and the same
//! digest. A file always says `exec`; `source` only when its bytes are a span of a pack.

use bytes::Bytes;
use kbf_types::Digest;
use serde_json::Value;
use tierstore::{EntryKind, TreeChild, TreeDigest, TreeError, parse_manifest};

/// A child of a manifest: its name, and its JSON object.
pub(super) type Child = (String, String);

/// A JSON string of `text`.
fn quoted(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// A file child: its contents' digest, and the pack span `source` its bytes are, if any.
pub(super) fn file(name: &str, digest: &Digest, exec: bool, source: Option<(&str, u64)>) -> Child {
    file_hex(name, &digest.hash_hex(), digest.size_bytes, exec, source)
}

fn file_hex(name: &str, sha256: &str, size: u64, exec: bool, source: Option<(&str, u64)>) -> Child {
    let source = source.map_or_else(String::new, |(key, offset)| {
        format!(r#","source":{{"key":{},"offset":{offset}}}"#, quoted(key))
    });
    let json = format!(
        r#"{{"name":{},"kind":"file","sha256":"{sha256}","size":{size},"exec":{exec}{source}}}"#,
        quoted(name)
    );
    (name.to_owned(), json)
}

/// A directory child: the digest of its manifest, whose size is the manifest's length.
pub(super) fn dir(name: &str, manifest: &Digest) -> Child {
    dir_hex(name, &manifest.hash_hex(), manifest.size_bytes)
}

fn dir_hex(name: &str, sha256: &str, size: u64) -> Child {
    let json = format!(
        r#"{{"name":{},"kind":"dir","sha256":"{sha256}","size":{size}}}"#,
        quoted(name)
    );
    (name.to_owned(), json)
}

/// A symlink child.
pub(super) fn symlink(name: &str, target: &str) -> Child {
    let json = format!(
        r#"{{"name":{},"kind":"symlink","target":{}}}"#,
        quoted(name),
        quoted(target)
    );
    (name.to_owned(), json)
}

/// A child of a manifest tierstore's reader parsed, as it is.
pub(super) fn copied(child: &TreeChild) -> Child {
    match &child.kind {
        EntryKind::Dir { digest, size } => dir_hex(&child.name, &digest.to_string(), *size),
        EntryKind::File {
            digest,
            size,
            exec,
            source,
        } => {
            let source = source.as_ref().map(|s| (&*s.key, s.offset));
            file_hex(&child.name, &digest.to_string(), *size, *exec, source)
        }
        EntryKind::Symlink { target } => symlink(&child.name, target),
    }
}

/// A manifest checked by tierstore's reader, with its digest.
pub(super) struct Manifest {
    /// The manifest's digest: the SHA-256 of its bytes, and their length.
    pub(super) digest: Digest,
    /// The same digest, as tierstore names a tree by it.
    pub(super) tree: TreeDigest,
    /// The bytes.
    pub(super) bytes: Bytes,
}

/// The manifest of `children`, sorted bytewise by name, checked by tierstore's reader
/// ([`parse_manifest`]), so that nothing is stored that tierstore would refuse to read.
///
/// # Errors
/// What the reader says: a name it refuses (`.wh.`, `..`, ...), a name twice, a manifest over
/// its size limit, a span that runs past 2^64.
pub(super) fn manifest(mut children: Vec<Child>) -> Result<Manifest, TreeError> {
    children.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let listed: Vec<String> = children.into_iter().map(|(_, json)| json).collect();
    let bytes = format!(r#"{{"v":1,"children":[{}]}}"#, listed.join(",")).into_bytes();
    let tree = TreeDigest::of(&bytes);
    parse_manifest(&bytes, &tree)?;
    Ok(Manifest {
        digest: kbf_segments::sha256(&bytes),
        tree,
        bytes: Bytes::from(bytes),
    })
}
