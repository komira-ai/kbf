//! The order spool: the directory the node writes order records into and the root
//! `kbf-installer` reads (`docs/design/pull-updates.md` section 7). It is owned by the
//! service user with mode 0700, inside a root-owned parent; the node keeps its own small
//! state file in it too. There are no release bytes here, only records.
//!
//! A record becomes visible by one rename out of a `.partial-` file, after it is written
//! and synced, so the installer never reads half of one; a start removes what a crash left.

use std::collections::hash_map::RandomState;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hash::BuildHasher;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::record::{OrderRecord, RecordError, file_name};

/// The prefix of a file that is not complete yet.
pub const PARTIAL: &str = ".partial-";

/// The spool under a directory.
#[derive(Clone, Debug)]
pub struct Spool {
    root: PathBuf,
}

/// A name no other call (or process) draws: 16 hex digits.
fn unique() -> String {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:016x}",
        RandomState::new().hash_one((std::process::id(), call))
    )
}

impl Spool {
    /// The spool at `root`, made (mode 0700) if it is not there, with every `.partial-`
    /// file a crash left removed.
    ///
    /// # Errors
    /// The directory cannot be made or listed.
    pub fn open(root: &Path) -> io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(root)?;
        let mut removed = 0;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(PARTIAL) {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        if removed > 0 {
            tracing::info!(removed, "partial order records of an earlier run removed");
        }
        Ok(Self {
            root: root.to_owned(),
        })
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Whether the record for (`epoch`, `order_id`) is in the spool.
    #[must_use]
    pub fn has(&self, epoch: u64, order_id: u64) -> bool {
        self.root.join(file_name(epoch, order_id)).is_file()
    }

    /// Writes `record`, visible by one rename.
    ///
    /// # Errors
    /// The record is too large, or the file cannot be written, synced or renamed.
    pub fn write(&self, record: &OrderRecord) -> io::Result<()> {
        let bytes = record
            .to_bytes()
            .map_err(|e: RecordError| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = self.root.join(format!("{PARTIAL}{}.json", unique()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, self.root.join(record.file_name()))?;
        File::open(&self.root)?.sync_all()
    }
}
