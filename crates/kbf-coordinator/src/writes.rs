//! What the coordinator keeps of the writes of each service's desired state: a generation
//! that every accepted write raises, what the last change of version replaced, and the
//! caller's id of the write that made the state (`docs/design/gamma-cicd.md`, G1).
//!
//! A caller that read the state can make its write conditional on nothing having been
//! accepted since (`if_generation`), and can tell its own write from another's afterwards
//! (`write_id`). `replaced` is the baseline a failed release goes back to: it is kept by the
//! coordinator, so it is there whatever happened to the job that set the release.
//!
//! The record is kept in a file of its own beside the state file (the state file's name with
//! `.writes` added), so the state file stays in the form earlier builds read and a downgrade
//! still starts. The file names the state file it describes by its [`FileId`] (its inode and
//! its modification time): the state file is written whole to a temporary file, the record
//! file is written naming that file's id, and only then is the temporary file renamed over the
//! state file, which keeps both. Every replacement of the state file is a new file: a new
//! inode, or an inode freed and reused by a file written later, so the record file describes
//! the state file exactly when the ids match. When they do not (a stop between the two
//! writes, or any write of the state file by an earlier build, even one that left it holding
//! the state it held before), and also
//! when an entry's state (`of`) is not the one in the state file, the entries' `replaced` and
//! `write_id` are dropped and their generations raised, so a write conditional on a
//! generation read before cannot pass. A generation that cannot be raised any more stops the
//! write, or the start, instead of repeating. A key this build does not know is skipped; a
//! file that is not JSON of this shape stops the start.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use kbf_updater::version::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::desired::{Desired, RoleLimits, StoreError, io_at, write_whole};

/// The most bytes of a `write_id`.
pub(crate) const MAX_WRITE_ID: usize = 128;

/// Whether `id` is a `write_id` the coordinator keeps: 1 to [`MAX_WRITE_ID`] visible ASCII
/// characters (no space, no control character), so it prints as one plain word.
#[must_use]
pub(crate) fn is_write_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_WRITE_ID && id.bytes().all(|b| b.is_ascii_graphic())
}

/// The version and minimums a change of version replaced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replaced {
    /// The version desired before.
    pub version: Version,
    /// Its minimums, as they were set.
    pub min_up: RoleLimits,
}

/// What is kept of the writes of one service's desired state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Raised by every accepted write, including one that sets the version already desired;
    /// 0 before the first.
    #[serde(default)]
    pub generation: u64,
    /// What the last change of version replaced; none before a second version was set.
    #[serde(default)]
    pub replaced: Option<Replaced>,
    /// The caller's id of the write that made the state; empty for a write without one.
    #[serde(default)]
    pub write_id: String,
}

impl Record {
    /// The record after `next` is written over `before` (none for a first write) with
    /// `write_id`: the generation one higher, `replaced` moved to `before` when the version
    /// changes and kept when it does not. None when the generation is at its largest.
    #[must_use]
    pub(crate) fn after(
        &self,
        before: Option<&Desired>,
        next: &Desired,
        write_id: String,
    ) -> Option<Self> {
        let replaced = match before {
            Some(old) if old.version != next.version => Some(Replaced {
                version: old.version.clone(),
                min_up: old.min_up.clone(),
            }),
            _ => self.replaced.clone(),
        };
        Some(Self {
            generation: self.generation.checked_add(1)?,
            replaced,
            write_id,
        })
    }
}

/// One entry of the file: the record, and the state it describes.
#[derive(Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    record: Record,
    of: Value,
}

/// Which file a path is: its inode and its modification time. A file written whole and renamed
/// into place keeps both; a file that replaces it has another inode, or, when the inode is
/// reused, the later time of its own write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileId {
    inode: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
}

impl FileId {
    /// The id of the file `meta` describes.
    pub(crate) fn of(meta: &fs::Metadata) -> Self {
        Self {
            inode: meta.ino(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        }
    }
}

/// The file.
#[derive(Serialize, Deserialize)]
struct File {
    /// The state file the entries describe; none from a file that names none.
    #[serde(default)]
    state_file: Option<FileId>,
    services: BTreeMap<String, Entry>,
}

/// The path of the record file of the state file at `state`.
#[must_use]
pub(crate) fn path_of(state: &Path) -> PathBuf {
    let mut name = state.as_os_str().to_owned();
    name.push(".writes");
    PathBuf::from(name)
}

/// The state `desired` as the file compares it.
fn value_of(desired: &Desired) -> Value {
    serde_json::to_value(desired).unwrap_or(Value::Null)
}

/// The records of the file at `path` for the states `desired`, read from the state file at
/// `state`: a service whose entry is missing has the empty record; when the file names another
/// state file than the one at `state`, or an entry describes another state, the entry's
/// generation is raised and nothing else is kept. No file is no entry.
///
/// # Errors
/// [`StoreError`]: the file exists and cannot be read, or is not JSON of the file's shape; or
/// a generation that would be raised is at its largest.
pub(crate) fn load(
    path: &Path,
    state: &Path,
    desired: &BTreeMap<String, Desired>,
) -> Result<BTreeMap<String, Record>, StoreError> {
    let mut file = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<File>(&bytes).map_err(|e| StoreError::Io {
            path: path.to_owned(),
            source: io::Error::new(io::ErrorKind::InvalidData, e),
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => File {
            state_file: None,
            services: BTreeMap::new(),
        },
        Err(e) => return Err(io_at(path)(e)),
    };
    // A state file that cannot be looked at is not the one the record names: the record is
    // dropped, never trusted.
    let id = fs::metadata(state).ok().map(|meta| FileId::of(&meta));
    let same_file = id.is_some() && file.state_file == id;
    let mut records = BTreeMap::new();
    for (name, current) in desired {
        let record = match file.services.remove(name) {
            Some(entry) if same_file && entry.of == value_of(current) => entry.record,
            Some(entry) => Record {
                generation: entry.record.generation.checked_add(1).ok_or_else(|| {
                    StoreError::GenerationExhausted {
                        service: name.clone(),
                    }
                })?,
                ..Record::default()
            },
            None => Record::default(),
        };
        records.insert(name.clone(), record);
    }
    Ok(records)
}

/// Replaces the file at `path` with the records of `desired`, describing the state file
/// `state_file`, whole.
///
/// # Errors
/// [`StoreError::Io`]: the file could not be replaced; the old one stands.
pub(crate) fn save(
    path: &Path,
    state_file: FileId,
    desired: &BTreeMap<String, Desired>,
    records: &BTreeMap<String, Record>,
) -> Result<(), StoreError> {
    let file = File {
        state_file: Some(state_file),
        services: desired
            .iter()
            .map(|(name, state)| {
                let record = records.get(name).cloned().unwrap_or_default();
                (
                    name.clone(),
                    Entry {
                        record,
                        of: value_of(state),
                    },
                )
            })
            .collect(),
    };
    let mut text = serde_json::to_vec_pretty(&file)
        .map_err(io::Error::from)
        .map_err(io_at(path))?;
    text.push(b'\n');
    write_whole(path, &text)
}
