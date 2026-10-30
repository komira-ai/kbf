//! The layout `digest-v1` of the cache directory, and the scan that reads it back at start.
//!
//! ```text
//! <path>/obj/<aa>/<bb>/<hex>        the raw bytes of an object, mode 0444
//! <path>/obj/<aa>/<bb>/<hex>.meta   JSON: the key, the size, the SHA-256 of the bytes, the tag
//! <path>/.stage/                    fills in progress; emptied at start, never served
//! ```
//!
//! `<hex>` is the SHA-256 of the key's name, except for a key `cas/l/<sha256>`, which is the
//! content's own SHA-256 (the name says what the bytes must hash to); `<aa>` and `<bb>` are its
//! first two pairs of digits. The data file is created last, by a create-only link, so a name
//! that has a data file has its `.meta`.

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The directory of the objects.
const OBJECTS: &str = "obj";

/// The directory of the fills in progress.
pub(super) const STAGE: &str = ".stage";

/// The ending of a sidecar's name.
const SIDECAR: &str = ".meta";

/// The prefix of the keys that are named by the SHA-256 of their bytes.
const WHOLE_BLOBS: &str = "cas/l/";

/// What a key is called on disk, and what its bytes must hash to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Name {
    /// Sixty-four lower-case hex digits.
    pub hex: String,
    /// For a key named by its content, the SHA-256 the bytes must have; else none.
    pub content: Option<String>,
}

/// Whether `text` is the lower-case hex of a SHA-256.
pub(super) fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The on-disk name of `key`.
pub(super) fn name_of(key: &str) -> Name {
    let content = key.strip_prefix(WHOLE_BLOBS).filter(|rest| is_digest(rest));
    match content {
        Some(digest) => Name {
            hex: digest.to_owned(),
            content: Some(digest.to_owned()),
        },
        None => Name {
            hex: hex::encode(Sha256::digest(key.as_bytes())),
            content: None,
        },
    }
}

/// The directory that holds the files of `hex` under `root`.
pub(super) fn object_dir(root: &Path, hex: &str) -> PathBuf {
    let (first, rest) = hex.split_at(2);
    let (second, _) = rest.split_at(2);
    root.join(OBJECTS).join(first).join(second)
}

/// The path of the data file of `hex` under `root`.
pub(super) fn data_path(root: &Path, hex: &str) -> PathBuf {
    object_dir(root, hex).join(hex)
}

/// The path of the sidecar of `hex` under `root`.
pub(super) fn meta_path(root: &Path, hex: &str) -> PathBuf {
    let mut path = data_path(root, hex).into_os_string();
    path.push(SIDECAR);
    PathBuf::from(path)
}

/// The directory of the fills in progress.
pub(super) fn stage_dir(root: &Path) -> PathBuf {
    root.join(STAGE)
}

/// The directory of the objects under `root`.
pub(super) fn objects_dir(root: &Path) -> PathBuf {
    root.join(OBJECTS)
}

/// Makes `dir`, which is under `root`, and the directories between them, with mode 0755 for
/// each however the umask is set: the files in them are read by other users (the jobs that
/// hard-link them), and no other user may change a name in them.
///
/// # Errors
/// What the system said.
pub(super) fn make_dirs(root: &Path, dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let depth = dir
        .strip_prefix(root)
        .map_or(0, |below| below.components().count());
    for made in dir.ancestors().take(depth) {
        let mode = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(made, mode)?;
    }
    Ok(())
}

/// What the sidecar of an object holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Sidecar {
    /// The key the object was read under.
    pub key: String,
    /// The bytes in the data file.
    pub size: u64,
    /// The SHA-256 of the bytes, as the fill hashed them.
    pub sha256: String,
    /// The content tag the upstream gave the object.
    pub tag: String,
    /// The time the upstream gave the object, in milliseconds.
    pub modified: u64,
}

/// An object that the scan found whole.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Found {
    pub hex: String,
    pub sidecar: Sidecar,
    /// The time of last change of the data file, in milliseconds: the seed of its recency.
    pub used: u64,
}

/// What the scan found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Scanned {
    /// The whole objects.
    pub found: Vec<Found>,
    /// The files that were not part of a whole object and were deleted.
    pub discarded: u64,
}

/// Empties the stage directory (making it if it is not there): a fill that was running when the
/// process ended is not kept. Gives the number of entries removed; one that cannot be removed is
/// left, which costs the disk some bytes and nothing else.
///
/// # Errors
/// What the system said about making the directory.
pub(super) fn clear_stage(root: &Path) -> io::Result<u64> {
    let stage = stage_dir(root);
    make_dirs(root, &stage)?;
    let mut removed = 0;
    for path in children(&stage, |_| true) {
        let gone = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed += u64::from(gone.is_ok());
    }
    Ok(removed)
}

/// The paths in `dir` that `wanted` says yes to; none if `dir` cannot be read, and an entry that
/// cannot be read is left out.
fn children(dir: &Path, wanted: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    entries
        .map(|entry| entry.path())
        .filter(|path| wanted(path))
        .collect()
}

/// The whole object whose data file is `path` under `root`, if it is one: the name is a digest
/// in the directories of its first digits, the sidecar reads, and it says the size the file has.
fn whole(root: &Path, path: &Path) -> Option<Found> {
    let hex = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !is_digest(hex) || data_path(root, hex) != path {
        return None;
    }
    let data = std::fs::metadata(path).ok();
    let text = std::fs::read(meta_path(root, hex)).ok()?;
    let sidecar: Sidecar = serde_json::from_slice(&text).ok()?;
    let used = data
        .as_ref()
        .and_then(|data| data.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    (Some(sidecar.size) == data.map(|data| data.len())).then(|| Found {
        hex: hex.to_owned(),
        sidecar,
        used,
    })
}

/// Whether `path` is the name of a sidecar.
fn is_sidecar(path: &Path) -> bool {
    path.to_str().is_some_and(|text| text.ends_with(SIDECAR))
}

/// Reads the object directory under `root`: the objects that are whole, and the number of files
/// that are not part of one (a data file whose sidecar is missing, unreadable or says another
/// size, a sidecar with no data file, a file with a name that is not a digest in its place),
/// which are deleted (one that cannot be is not counted and is left).
///
/// # Errors
/// What the system said about making the directory.
pub(super) fn scan(root: &Path) -> io::Result<Scanned> {
    let mut scanned = Scanned::default();
    let objects = objects_dir(root);
    make_dirs(root, &objects)?;
    for first in children(&objects, Path::is_dir) {
        for second in children(&first, Path::is_dir) {
            let (sidecars, data): (Vec<PathBuf>, Vec<PathBuf>) = children(&second, Path::is_file)
                .into_iter()
                .partition(|p| is_sidecar(p));
            for path in data {
                match whole(root, &path) {
                    Some(found) => scanned.found.push(found),
                    None => scanned.discarded += u64::from(std::fs::remove_file(&path).is_ok()),
                }
            }
            // After the data files: a sidecar is kept only by a data file that is still there.
            for path in sidecars {
                if !path.with_extension("").is_file() {
                    scanned.discarded += u64::from(std::fs::remove_file(&path).is_ok());
                }
            }
        }
    }
    Ok(scanned)
}
