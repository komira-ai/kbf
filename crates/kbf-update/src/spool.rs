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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{COMMIT, scratch};
    use crate::record::Locator;
    use crate::types::{Order, Verb};

    fn confirm(order_id: u64) -> OrderRecord {
        let order = Order {
            order_id,
            epoch: 1,
            generation: 1,
            verb: Some(Verb::Confirm),
            commit: COMMIT.to_owned(),
            window_s: 600,
        };
        OrderRecord::for_order(&order, Verb::Confirm, None).expect("record")
    }

    /// Catches a start that leaves a half-written record of a crashed run in the spool (the
    /// installer ignores dot names, but they would pile up), one that removes a real record or
    /// the node's own state file with them, a spool that is not made 0700 when missing, and an
    /// `open` that cannot say where the spool is.
    ///
    /// Mutants planted: the prefix test inverted (the record is removed, the partial left);
    /// the mode 0o755 (the mode assertion fails); the removal skipped (the partial remains).
    #[test]
    fn a_start_removes_the_partial_files_of_a_crash_and_nothing_else() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("spool-open").join("orders");
        let spool = Spool::open(&root).expect("made");
        assert_eq!(spool.path(), root);
        let mode = fs::metadata(&root).expect("dir").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        spool.write(&confirm(4)).expect("written");
        fs::write(root.join(".partial-dead.json"), b"{").expect("partial");
        fs::write(root.join("node-state.json"), b"{}").expect("state");
        let spool = Spool::open(&root).expect("reopened");
        let mut names: Vec<_> = fs::read_dir(&root)
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [file_name(1, 4), "node-state.json".to_owned()],
            "the partial is gone, the record and the state file stay"
        );
        assert!(spool.has(1, 4));
        assert!(!spool.has(1, 5));
    }

    /// Catches a record the installer would refuse being written (the writer must hold the
    /// reader's rules), and a spool that is not a directory being accepted: a record over the 1 MiB cap is an
    /// `InvalidData` error that leaves nothing behind, and a file in the place of the spool is
    /// an error of `open`.
    ///
    /// Mutant planted: the size cap skipped in `to_bytes` (the 2 MiB record is written).
    #[test]
    fn a_record_the_installer_would_refuse_is_not_written_and_a_file_is_no_spool() {
        let dir = scratch("spool-bad");
        let root = dir.join("orders");
        let spool = Spool::open(&root).expect("made");
        let mut bad = confirm(1);
        bad.components = vec![Locator {
            name: "kbf".to_owned(),
            sha256: "ab".repeat(32),
            size: 1,
            attestation: "a".repeat(2 << 20),
        }];
        let err = spool.write(&bad).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_dir(&root).expect("dir").count(), 0);
        let file = dir.join("file");
        fs::write(&file, b"x").expect("file");
        assert!(Spool::open(&file).is_err());
    }
}
