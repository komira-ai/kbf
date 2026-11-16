//! [`publish`]: the frontend's start writes the kbf base tree of its release to the object store
//! (docs/design/actions-as-jobs.md, 3.3, "Published by the frontend").
//!
//! The release installs the frontend with its bundles, one per Linux platform
//! (`kbf-base-tree-<commit>-<platform>.tree.tar`, [`kbf_tree_bundle`]) and the release's
//! `SHA256SUMS` in one directory. [`publish`] takes the bundle of one platform and
//!
//! 1. **checks it against `SHA256SUMS`**: the sums file lists the bundle's name exactly once and
//!    the file's SHA-256 is the listed one. The file is hashed as a stream on a blocking
//!    thread, never held;
//! 2. **reads it** on a blocking thread, one object at a time, handing each over a channel of
//!    one place: an object being read, one waiting and one being written are all of it that is in
//!    memory. Every object is checked against its name as it is read. The facts must be of the
//!    asked commit and platform;
//! 3. **writes each object the store lacks** to `cas/l/<sha256>` ([`large_key`]), in the
//!    bundle's order, which has every object after the ones it names and the root manifest
//!    last: the length of the key is asked first (an object of its length is held, nothing is
//!    sent; one of another length is refused, not replaced), and an absent key gets a
//!    create-only write, where an existing key ([`ObjectStoreError::AlreadyExists`]) is success,
//!    since the key is bound to its content;
//! 4. **checks the root**: the object the facts name is read by [`BaseTree::new`] before it is
//!    written, so a root tierstore cannot read or that holds a name an action tree keeps
//!    ([`BaseTreeError::Reserved`]) is never written, and a bundle with no such object is
//!    refused. The tree is named by the root alone, so none of the objects written before it
//!    is reachable from anything.
//!
//! A second release on the same pin writes only its `kbf-task`, its `.kbf-tools` manifest and
//! its root: every other object costs one length check.
//!
//! Every refusal is a [`PublishError`] that names what failed. Nothing is deleted; objects
//! written before a refusal stay, which is harmless: they are content-named and nothing names
//! them.

use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use kbf_objstore::ObjectStoreError;
use kbf_tree_bundle::{BundleError, Facts, Item, ReadError, file_name};
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;

use crate::action_tree::{BaseFacts, BaseTree, BaseTreeError};
use crate::large_backfill::{LargeStore, large_key};

/// The release's sums file, in the directory of its bundles: `sha256sum`'s lines.
pub const SUMS: &str = "SHA256SUMS";

/// The objects a reader hands the writer ahead of it: one waiting, so at most three objects are
/// in memory (read, waiting, written).
const CHANNEL: usize = 1;

/// The bytes the bundle is hashed in at a time.
const READ: usize = 1 << 16;

/// What [`publish`] made.
#[derive(Clone, Debug)]
pub struct Published {
    /// The base tree: its root manifest's children, and its facts.
    pub base: BaseTree,
    /// The root manifest's digest: the base tree's name.
    pub root: Digest,
    /// What the bundle says.
    pub facts: Facts,
    /// The objects this call wrote.
    pub written: u64,
    /// The objects the store held already (or whose key was taken while this call wrote).
    pub held: u64,
}

/// Why the base tree was not published. Each names what failed.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// The build does not know its commit, so it has no bundle to name.
    #[error("this build does not know its commit, so it has no base tree")]
    NoCommit,
    /// The sums file is not there.
    #[error("{path}: the release's sums file is missing")]
    NoSums {
        /// Where it should be.
        path: PathBuf,
    },
    /// A line of the sums file is not `<sha256>  <file name>`.
    #[error("{path}: line {line} is not `<sha256 in lowercase hex>  <file name>` and a newline")]
    SumsLine {
        /// The sums file.
        path: PathBuf,
        /// Its number, from 1.
        line: usize,
    },
    /// The sums file does not list the bundle.
    #[error("{path} does not list {name}")]
    NotListed {
        /// The sums file.
        path: PathBuf,
        /// The bundle's name.
        name: String,
    },
    /// The sums file lists the bundle twice.
    #[error("{path} lists {name} more than once")]
    ListedTwice {
        /// The sums file.
        path: PathBuf,
        /// The bundle's name.
        name: String,
    },
    /// The bundle is not there.
    #[error("{path}: the release's bundle for this platform is missing")]
    NoBundle {
        /// Where it should be.
        path: PathBuf,
    },
    /// A file could not be read.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What failed.
        #[source]
        source: io::Error,
    },
    /// The bundle's bytes are not the listed ones.
    #[error("{path}: its SHA-256 is {actual}, but {sums} lists {listed}")]
    Digest {
        /// The bundle.
        path: PathBuf,
        /// The sums file.
        sums: PathBuf,
        /// The digest the sums file lists.
        listed: String,
        /// The digest of the file.
        actual: String,
    },
    /// The bundle is not one.
    #[error("{path}: {source}")]
    Bundle {
        /// The bundle.
        path: PathBuf,
        /// What is wrong with it.
        #[source]
        source: BundleError,
    },
    /// The bundle gave no facts.
    #[error("{path}: the bundle gave no facts")]
    NoFacts {
        /// The bundle.
        path: PathBuf,
    },
    /// The facts are of another commit than the one asked.
    #[error("{path}: the bundle is of commit {got:?}, not {want:?}")]
    Commit {
        /// The bundle.
        path: PathBuf,
        /// The commit it says.
        got: String,
        /// The commit asked.
        want: String,
    },
    /// The facts are of another platform than the one asked.
    #[error("{path}: the bundle is of platform {got:?}, not {want:?}")]
    Platform {
        /// The bundle.
        path: PathBuf,
        /// The platform it says.
        got: String,
        /// The platform asked.
        want: String,
    },
    /// The store failed a call.
    #[error("{key}: the store failed the {call}: {source}")]
    Store {
        /// The key of the object.
        key: String,
        /// `length check` or `write`.
        call: &'static str,
        /// What the store said.
        #[source]
        source: ObjectStoreError,
    },
    /// An object of another length than its content is held at its key.
    #[error("{key}: the store holds {held} bytes where the object is {want}")]
    Length {
        /// The key of the object.
        key: String,
        /// The length held.
        held: u64,
        /// The object's length.
        want: u64,
    },
    /// The bundle holds no object the facts name as the root.
    #[error("{path}: the bundle holds no object {root}, the root its facts name")]
    NoRoot {
        /// The bundle.
        path: PathBuf,
        /// The root the facts name.
        root: String,
    },
    /// The root manifest is not a base tree.
    #[error("{path}: {source}")]
    Base {
        /// The bundle.
        path: PathBuf,
        /// What is wrong with the root.
        #[source]
        source: BaseTreeError,
    },
    /// A blocking task did not finish.
    #[error("a task of the publish stopped: {0}")]
    Task(String),
}

/// Publishes the kbf base tree of the release `commit` for `platform` (`linux-x86_64` or
/// `linux-arm64`) from the bundle in `dir`, into `store`.
///
/// # Errors
/// A [`PublishError`], which names what failed: the commit is not known, the sums file or the
/// bundle is missing, the sums file does not list the bundle or lists other bytes, the bundle is
/// not one or is of another commit or platform, an object's length at its key is not its own,
/// the store failed, or the root is missing or not a base tree.
pub async fn publish(
    dir: &Path,
    commit: &str,
    platform: &str,
    store: Arc<dyn LargeStore>,
) -> Result<Published, PublishError> {
    if commit.is_empty() {
        return Err(PublishError::NoCommit);
    }
    let name = file_name(commit, platform);
    let sums = dir.join(SUMS);
    let bundle = dir.join(&name);
    let listed = {
        let (sums, name) = (sums.clone(), name.clone());
        blocking(move || listed_digest(&sums, &name)).await?
    };
    let file = {
        let (bundle, sums) = (bundle.clone(), sums.clone());
        blocking(move || checked_file(&bundle, &sums, &listed)).await?
    };
    let (send, receive) = mpsc::channel(CHANNEL);
    let reading = {
        let bundle = bundle.clone();
        blocking(move || hand_over(file, &bundle, &send))
    };
    let writing = write_all(receive, &*store, &bundle, commit, platform);
    let (read, written) = futures::future::join(reading, writing).await;
    // A bundle that is not one is the cause of whatever the writer then found missing.
    read?;
    written
}

/// The name of the bundle of `commit` for `platform` in a release's directory.
#[must_use]
pub fn bundle_name(commit: &str, platform: &str) -> String {
    file_name(commit, platform)
}

/// Whether `dir` holds what [`publish`] reads for `commit` and `platform`: a sums file that
/// lists the bundle once, and a file of the bundle's name. Nothing is hashed (a cheap look, for
/// a cache's hit; [`publish`] hashes the bundle before it reads it).
#[must_use]
pub fn is_staged(dir: &Path, commit: &str, platform: &str) -> bool {
    let name = file_name(commit, platform);
    listed_digest(&dir.join(SUMS), &name).is_ok() && dir.join(name).is_file()
}

/// Checks the bundle of `commit` for `platform` in `dir` against the sums file there: listed
/// once and hashing to the listed digest. Blocks while it hashes.
///
/// # Errors
/// A [`PublishError`]: the sums file or the bundle is missing, the sums file does not list the
/// bundle or lists other bytes.
pub fn verify(dir: &Path, commit: &str, platform: &str) -> Result<(), PublishError> {
    let name = file_name(commit, platform);
    let sums = dir.join(SUMS);
    let listed = listed_digest(&sums, &name)?;
    checked_file(&dir.join(name), &sums, &listed).map(drop)
}

/// Runs `work`, which blocks, on the runtime's blocking pool.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, PublishError> + Send + 'static,
) -> Result<T, PublishError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(done) => done,
        Err(stopped) => Err(PublishError::Task(stopped.to_string())),
    }
}

/// The digest `dir`'s sums file lists for `name`, in lowercase hex.
fn listed_digest(sums: &Path, name: &str) -> Result<String, PublishError> {
    let text = std::fs::read_to_string(sums).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => PublishError::NoSums {
            path: sums.to_owned(),
        },
        _ => PublishError::Io {
            path: sums.to_owned(),
            source,
        },
    })?;
    let mut listed = Vec::new();
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let parsed = line
            .strip_suffix('\n')
            .and_then(|line| line.split_once("  "))
            .filter(|(digest, file)| is_sha256(digest) && !file.is_empty() && !file.contains('/'));
        let Some((digest, file)) = parsed else {
            return Err(PublishError::SumsLine {
                path: sums.to_owned(),
                line: index + 1,
            });
        };
        if file == name {
            listed.push(digest);
        }
    }
    let (path, name) = (sums.to_owned(), name.to_owned());
    match listed.as_slice() {
        [] => Err(PublishError::NotListed { path, name }),
        [one] => Ok((*one).to_owned()),
        _ => Err(PublishError::ListedTwice { path, name }),
    }
}

/// Whether `text` is 64 lowercase hex digits.
fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The bundle, opened, checked to hash to `listed` (`sums` is where it is listed) and rewound.
fn checked_file(bundle: &Path, sums: &Path, listed: &str) -> Result<File, PublishError> {
    let io_error = |source: io::Error| match source.kind() {
        io::ErrorKind::NotFound => PublishError::NoBundle {
            path: bundle.to_owned(),
        },
        _ => PublishError::Io {
            path: bundle.to_owned(),
            source,
        },
    };
    let mut file = File::open(bundle).map_err(io_error)?;
    let mut hasher = Sha256::new();
    let mut reader = BufReader::with_capacity(READ, &mut file);
    loop {
        let chunk = reader.fill_buf().map_err(io_error)?;
        if chunk.is_empty() {
            break;
        }
        hasher.update(chunk);
        let hashed = chunk.len();
        reader.consume(hashed);
    }
    let actual = Digest::new(DigestFunction::Sha256, hasher.finalize().into(), 0).hash_hex();
    if actual != listed {
        return Err(PublishError::Digest {
            path: bundle.to_owned(),
            sums: sums.to_owned(),
            listed: listed.to_owned(),
            actual,
        });
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    Ok(file)
}

/// Reads `file` (the bundle at `path`) and sends each item to `send`, until the bundle ends or
/// the writer has gone (it stopped on an error of its own, which it reports).
fn hand_over(file: File, path: &Path, send: &mpsc::Sender<Item>) -> Result<(), PublishError> {
    let read = kbf_tree_bundle::read(BufReader::new(file), |item| {
        send.blocking_send(item).map_err(|_| ())
    });
    match read {
        Ok(()) | Err(ReadError::Stopped(())) => Ok(()),
        Err(ReadError::Bundle(source)) => Err(PublishError::Bundle {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Takes the facts and then the objects from `items`, checks the facts against `commit` and
/// `platform` and writes the objects to `store`.
async fn write_all(
    mut items: mpsc::Receiver<Item>,
    store: &dyn LargeStore,
    path: &Path,
    commit: &str,
    platform: &str,
) -> Result<Published, PublishError> {
    let Some(Item::Facts(facts)) = items.recv().await else {
        return Err(PublishError::NoFacts {
            path: path.to_owned(),
        });
    };
    if facts.commit != commit {
        return Err(PublishError::Commit {
            path: path.to_owned(),
            got: facts.commit,
            want: commit.to_owned(),
        });
    }
    if facts.platform != platform {
        return Err(PublishError::Platform {
            path: path.to_owned(),
            got: facts.platform,
            want: platform.to_owned(),
        });
    }
    let (mut written, mut held) = (0, 0);
    let mut root = None;
    while let Some(Item::Object { sha256, bytes }) = items.recv().await {
        let digest = Digest::new(DigestFunction::Sha256, hash_of(&sha256), bytes.len() as u64);
        let bytes = Bytes::from(bytes);
        if sha256 == facts.root {
            // Before it is written: a root that is no base tree is never named in the store.
            let facts = BaseFacts {
                entries: facts.entries,
                // A depth no platform's `usize` holds is one no tree passes the limits with.
                depth: usize::try_from(facts.depth).unwrap_or(usize::MAX),
            };
            let base = BaseTree::new(&bytes, facts).map_err(|source| PublishError::Base {
                path: path.to_owned(),
                source,
            })?;
            root = Some((base, digest));
        }
        if put_new(store, &digest, bytes).await? {
            written += 1;
        } else {
            held += 1;
        }
    }
    let Some((base, root)) = root else {
        return Err(PublishError::NoRoot {
            path: path.to_owned(),
            root: facts.root,
        });
    };
    Ok(Published {
        base,
        root,
        facts,
        written,
        held,
    })
}

/// Writes the object `bytes` (of `digest`) to its key unless the store holds it; whether it was
/// written.
async fn put_new(
    store: &dyn LargeStore,
    digest: &Digest,
    bytes: Bytes,
) -> Result<bool, PublishError> {
    let key = large_key(digest);
    let failed = |call| {
        let key = key.to_string();
        move |source| PublishError::Store { key, call, source }
    };
    match store.len(&key).await.map_err(failed("length check"))? {
        Some(held) if held == digest.size_bytes => Ok(false),
        Some(held) => Err(PublishError::Length {
            key: key.to_string(),
            held,
            want: digest.size_bytes,
        }),
        None => {
            let body = stream::once(async move { Ok(bytes) }).boxed();
            match store.put(&key, digest.size_bytes, body).await {
                Ok(()) => Ok(true),
                Err(ObjectStoreError::AlreadyExists(_)) => Ok(false),
                Err(source) => Err(failed("write")(source)),
            }
        }
    }
}

/// The 32 bytes of the 64 lowercase hex digits `hex` (a name [`kbf_tree_bundle::read`] checked).
fn hash_of(hex: &str) -> [u8; 32] {
    let nibble = |digit: u8| match digit {
        b'0'..=b'9' => digit - b'0',
        _ => digit - b'a' + 10,
    };
    let mut hash = [0; 32];
    for (byte, pair) in hash.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        *byte = pair
            .iter()
            .fold(0, |sum, digit| (sum << 4) | nibble(*digit));
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{PublishError, blocking};

    /// Asserts that a blocking task that panics is an error naming it, not a panic of the start.
    /// Catches: a join failure unwrapped.
    #[tokio::test]
    async fn a_blocking_task_that_panics_is_an_error() {
        let stopped = blocking::<()>(|| panic!("the work")).await;

        let Err(PublishError::Task(why)) = stopped else {
            panic!("not a task error: {stopped:?}");
        };
        assert!(why.contains("panic"), "{why}");
    }
}
