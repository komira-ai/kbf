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
//! still starts. Each entry holds the state it describes (`of`): an entry whose state is not
//! the one in the state file (a crash between the two writes, or an earlier build that wrote
//! the state file alone) has its `replaced` and `write_id` dropped and its generation raised,
//! so a write conditional on a generation read before cannot pass. A key this build does not
//! know is skipped; a file that is not JSON of this shape stops the start.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use kbf_updater::version::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::desired::{Desired, RoleLimits, StoreError, io_at, write_whole};

/// The most bytes of a `write_id`.
pub const MAX_WRITE_ID: usize = 128;

/// Whether `id` is a `write_id` the coordinator keeps: 1 to [`MAX_WRITE_ID`] visible ASCII
/// characters (no space, no control character), so it prints as one plain word.
#[must_use]
pub fn is_write_id(id: &str) -> bool {
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
    /// changes and kept when it does not.
    #[must_use]
    pub fn after(&self, before: Option<&Desired>, next: &Desired, write_id: String) -> Self {
        let replaced = match before {
            Some(old) if old.version != next.version => Some(Replaced {
                version: old.version.clone(),
                min_up: old.min_up.clone(),
            }),
            _ => self.replaced.clone(),
        };
        Self {
            generation: self.generation.saturating_add(1),
            replaced,
            write_id,
        }
    }
}

/// One entry of the file: the record, and the state it describes.
#[derive(Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    record: Record,
    of: Value,
}

/// The file.
#[derive(Serialize, Deserialize)]
struct File {
    services: BTreeMap<String, Entry>,
}

/// The path of the record file of the state file at `state`.
#[must_use]
pub fn path_of(state: &Path) -> PathBuf {
    let mut name = state.as_os_str().to_owned();
    name.push(".writes");
    PathBuf::from(name)
}

/// The state `desired` as the file compares it.
fn value_of(desired: &Desired) -> Value {
    serde_json::to_value(desired).unwrap_or(Value::Null)
}

/// The records of the file at `path` for the states `desired`: a service whose entry is
/// missing has the empty record; one whose entry describes another state has its generation
/// raised and nothing else. No file is no entry.
///
/// # Errors
/// [`StoreError`]: the file exists and cannot be read, or is not JSON of the file's shape.
pub fn load(
    path: &Path,
    desired: &BTreeMap<String, Desired>,
) -> Result<BTreeMap<String, Record>, StoreError> {
    let mut file = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<File>(&bytes).map_err(|e| StoreError::Io {
            path: path.to_owned(),
            source: io::Error::new(io::ErrorKind::InvalidData, e),
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => File {
            services: BTreeMap::new(),
        },
        Err(e) => return Err(io_at(path)(e)),
    };
    Ok(desired
        .iter()
        .map(|(name, state)| {
            let record = match file.services.remove(name) {
                Some(entry) if entry.of == value_of(state) => entry.record,
                Some(entry) => Record {
                    generation: entry.record.generation.saturating_add(1),
                    ..Record::default()
                },
                None => Record::default(),
            };
            (name.clone(), record)
        })
        .collect())
}

/// Replaces the file at `path` with the records of `desired`, whole.
///
/// # Errors
/// [`StoreError::Io`]: the file could not be replaced; the old one stands.
pub fn save(
    path: &Path,
    desired: &BTreeMap<String, Desired>,
    records: &BTreeMap<String, Record>,
) -> Result<(), StoreError> {
    let file = File {
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
