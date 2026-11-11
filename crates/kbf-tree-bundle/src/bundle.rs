//! The bundle of the kbf base tree: one release asset per Linux platform,
//! `kbf-base-tree-<commit>-<platform>.tree.tar` ([`file_name`]).
//!
//! A plain tar whose first entry is `facts.json` ([`Facts`]) and whose every other entry is
//! one object, `objects/<sha256>`: a file's bytes or a directory's manifest, under the
//! SHA-256 of its bytes in lowercase hex. Each object is once. The writer puts the files
//! first and the manifests after them, each manifest after the ones it names and the root's
//! last, so that an object is published after everything it names. Every entry is a regular
//! file of mode 0644, owner 0, time 0: the same tree gives the same bytes.
//!
//! [`read`] refuses, by name, a bundle whose first entry is not the facts, an entry that is
//! neither the facts nor an object, an object whose bytes are not its name's, and facts that
//! are not JSON of their shape. It does not check the facts against the objects: the bundle is
//! checked against the release's `SHA256SUMS` before it is read, and the root manifest is
//! read by tierstore's own reader when it is used.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The name of the asset before its commit.
pub const NAME: &str = "kbf-base-tree";

/// The end of the asset's file name.
pub const SUFFIX: &str = ".tree.tar";

/// The name of the facts in the bundle: its first entry.
pub const FACTS: &str = "facts.json";

/// The directory of the objects in the bundle.
pub const OBJECTS: &str = "objects/";

/// The file name of the bundle of the release `commit` for `platform` (`linux-x86_64` or
/// `linux-arm64`).
#[must_use]
pub fn file_name(commit: &str, platform: &str) -> String {
    format!("{NAME}-{commit}-{platform}{SUFFIX}")
}

/// What the bundle says of its tree, beside the objects. A reader ignores a member it does not
/// know, so members are only added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    /// The commit of the release that built it.
    pub commit: String,
    /// The platform whose base image it is (`linux-x86_64`, `linux-arm64`).
    pub platform: String,
    /// The SHA-256 of the root manifest, in lowercase hex: the base tree's name.
    pub root: String,
    /// The entries below the root, across all its directories, as tierstore counts them.
    pub entries: u64,
    /// The deepest entry: the number of names on its path from the root.
    pub depth: u64,
    /// Every path whose mode the tree does not keep (a tree has modes 0644 and 0755 only), in
    /// path order.
    pub normalised: Vec<Normalised>,
}

/// A path whose mode was changed to one a tree holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Normalised {
    /// The path, from the root (`/etc/shadow`).
    pub path: String,
    /// Its mode in the image, as four octal digits (`0640`).
    pub from: String,
    /// Its mode in the tree (`0644` or `0755`).
    pub to: String,
}

/// Why a bundle cannot be written or read.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// Writing or reading the tar failed.
    #[error("the bundle: {0}")]
    Io(#[from] io::Error),
    /// The bundle holds no entry.
    #[error("the bundle is empty: its first entry must be {FACTS}")]
    Empty,
    /// The first entry is not the facts.
    #[error("the bundle's first entry is {0:?}, not {FACTS}")]
    FactsNotFirst(String),
    /// The facts are not JSON of their shape.
    #[error("the bundle's {FACTS}: {0}")]
    Facts(#[source] serde_json::Error),
    /// An entry that is neither the facts (first) nor `objects/<sha256>`.
    #[error("the bundle holds {0:?}, which is neither {FACTS} first nor {OBJECTS}<sha256>")]
    Stray(String),
    /// An entry that is not a regular file.
    #[error("the bundle's entry {0:?} is not a regular file")]
    NotAFile(String),
    /// An object whose bytes hash to another digest than its name.
    #[error("the bundle's object {name} holds bytes whose SHA-256 is {actual}")]
    Digest {
        /// The object's name (its digest, as the bundle says).
        name: String,
        /// The digest of its bytes.
        actual: String,
    },
}

/// What a bundle holds, in its order: its facts, then its objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// The facts: the first entry.
    Facts(Facts),
    /// An object, checked to be the bytes its name is the SHA-256 of.
    Object {
        /// The SHA-256 of the bytes, in lowercase hex.
        sha256: String,
        /// The bytes.
        bytes: Vec<u8>,
    },
}

/// Why [`read`] stopped: the bundle, or what the caller's function said.
#[derive(Debug, thiserror::Error)]
pub enum ReadError<E> {
    /// The bundle is not one.
    #[error(transparent)]
    Bundle(#[from] BundleError),
    /// The caller's function refused an item.
    #[error("{0}")]
    Stopped(E),
}

/// The writer of a bundle.
pub struct Writer<W: Write> {
    tar: tar::Builder<W>,
    written: BTreeSet<String>,
}

/// The header of an entry of `size` bytes: a regular file of mode 0644, owner 0, time 0.
fn header(size: u64) -> tar::Header {
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header
}

impl<W: Write> Writer<W> {
    /// A bundle written to `out`, which begins with `facts`.
    ///
    /// # Errors
    /// [`BundleError::Io`]: `out` failed.
    pub fn new(out: W, facts: &Facts) -> Result<Self, BundleError> {
        let mut tar = tar::Builder::new(out);
        let json = serde_json::to_vec(facts).map_err(BundleError::Facts)?;
        tar.append_data(&mut header(json.len() as u64), FACTS, json.as_slice())?;
        Ok(Self {
            tar,
            written: BTreeSet::new(),
        })
    }

    /// Adds the object `bytes` under its SHA-256, unless it is already in the bundle; gives
    /// that digest in hex.
    ///
    /// # Errors
    /// [`BundleError::Io`]: the output failed.
    pub fn object(&mut self, bytes: &[u8]) -> Result<String, BundleError> {
        let sha256 = hex::encode(Sha256::digest(bytes));
        if self.written.insert(sha256.clone()) {
            let path = format!("{OBJECTS}{sha256}");
            self.tar
                .append_data(&mut header(bytes.len() as u64), path, bytes)?;
        }
        Ok(sha256)
    }

    /// Ends the tar, and gives back the output.
    ///
    /// # Errors
    /// [`BundleError::Io`]: the output failed.
    pub fn finish(self) -> Result<W, BundleError> {
        Ok(self.tar.into_inner()?)
    }
}

/// Whether `text` is 64 lowercase hex digits.
fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Reads the bundle `bundle`, handing `visit` its facts and then each object, checked, in the
/// bundle's order. Nothing more is read once `visit` refuses an item.
///
/// # Errors
/// [`ReadError::Bundle`]: the bundle is not one (above); [`ReadError::Stopped`]: what `visit`
/// said.
pub fn read<R: Read, E>(
    bundle: R,
    mut visit: impl FnMut(Item) -> Result<(), E>,
) -> Result<(), ReadError<E>> {
    let mut archive = tar::Archive::new(bundle);
    let mut first = true;
    for entry in archive.entries().map_err(BundleError::Io)? {
        let mut entry = entry.map_err(BundleError::Io)?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(BundleError::NotAFile(name).into());
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(BundleError::Io)?;
        let item = if first {
            if name != FACTS {
                return Err(BundleError::FactsNotFirst(name).into());
            }
            first = false;
            Item::Facts(serde_json::from_slice(&bytes).map_err(BundleError::Facts)?)
        } else {
            let Some(sha256) = name.strip_prefix(OBJECTS).filter(|s| is_sha256(s)) else {
                return Err(BundleError::Stray(name).into());
            };
            let actual = hex::encode(Sha256::digest(&bytes));
            if actual != sha256 {
                return Err(BundleError::Digest { name, actual }.into());
            }
            Item::Object {
                sha256: sha256.to_owned(),
                bytes,
            }
        };
        visit(item).map_err(ReadError::Stopped)?;
    }
    if first {
        return Err(BundleError::Empty.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
