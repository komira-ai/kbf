//! The swap's [`RootOrders`] for a real host: the three orders it writes go through the
//! one [`OrderSpool`] (the same counter the update API writes with), and the two facts it
//! reads back come from the root half's `status.json`.
//!
//! # What is read
//!
//! `status.json` is read whole on every call, never cached: the swap asks precisely
//! because the answer may have changed (`installed` after a `confirm`). The file is
//! replaced by a rename, so a read sees one complete version.
//!
//! - [`staged`](RootOrders::staged): `staged_commit` joined with `staged{}`
//!   (`protocol`, `min_protocol`, `state_schema`, and `profile` when the writer sets it:
//!   today `helper/status.rs` does not, and it reads as `None`). `None` when either half is
//!   absent, because the facts are the root half's word that a release was fetched,
//!   verified and probed.
//! - [`installed`](RootOrders::installed): `installed_commit`.
//!
//! A missing file is `None` for both (no root half has written yet). A file over
//! [`MAX_STATUS`] bytes, one that cannot be read, or one that does not parse is a
//! [`PortError`]: a guess about what the root half said could confirm the wrong commit.
//! Unknown fields are ignored, as the update API's reader does.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::api::traits::InstallerStatus;
use super::model::{Locator, PortError, StagedFacts};
use super::orders::OrderSpool;
use super::ports::RootOrders;
use crate::CommitId;

/// The most bytes of `status.json` that are read. The writer's file is a few hundred bytes;
/// the cap keeps a file that grew without bound from being pulled into memory.
pub const MAX_STATUS: u64 = 64 * 1024;

/// [`RootOrders`] on disk: writes through a clone of the spool, reads `status.json`.
pub struct SpoolRootOrders {
    spool: OrderSpool,
    status: PathBuf,
}

impl SpoolRootOrders {
    /// An adapter that writes through `spool` (a clone shares its counter) and reads the
    /// status file at `status`.
    #[must_use]
    pub fn new(spool: OrderSpool, status: impl Into<PathBuf>) -> SpoolRootOrders {
        SpoolRootOrders {
            spool,
            status: status.into(),
        }
    }

    fn read(&self) -> Result<Option<InstallerStatus>, PortError> {
        let file = match File::open(&self.status) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(unreadable(&self.status, &e)),
        };
        let mut bytes = Vec::new();
        file.take(MAX_STATUS + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| unreadable(&self.status, &e))?;
        if bytes.len() as u64 > MAX_STATUS {
            return Err(PortError(format!(
                "{} is over {MAX_STATUS} bytes",
                self.status.display()
            )));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| PortError(format!("{} does not parse: {e}", self.status.display())))
    }
}

fn unreadable(path: &Path, e: &io::Error) -> PortError {
    PortError(format!("cannot read {}: {e}", path.display()))
}

impl RootOrders for SpoolRootOrders {
    fn frontend_update(&self, locator: &Locator) -> Result<(), PortError> {
        self.spool.frontend_update(locator)
    }

    fn staged(&self) -> Result<Option<StagedFacts>, PortError> {
        Ok(self.read()?.and_then(|status| status.staged_facts()))
    }

    fn installed(&self) -> Result<Option<CommitId>, PortError> {
        Ok(self.read()?.and_then(|status| status.installed_commit))
    }

    fn confirm(&self, commit: &CommitId) -> Result<(), PortError> {
        self.spool.confirm(commit)
    }

    fn revert(&self, commit: &CommitId) -> Result<(), PortError> {
        self.spool.revert(commit)
    }
}
