//! The leak scan's verbs (S4.2; fleet-updates.md 10.2, layer L3), over the items of
//! [`crate::scan`]:
//!
//! - `scan` collects the items and diffs them against the recorded baseline. It is
//!   dirty when anything was added or removed, or an item could not be read (a scan
//!   that could not look is no proof of a clean node). Refused when no baseline was
//!   recorded.
//! - `baseline` records the items as the baseline, only in its window: the root tool
//!   that applied a software set or erased the Mac opened it (the window file names
//!   the boot it was written in), a boot has followed (this boot is not that one), and
//!   no lease user exists or was created since (the first `user-create` closes the
//!   window). Refused, too, when an item cannot be read: a baseline must be whole.
//! - `reboot-dirty` reboots (`launchctl reboot system`), only when the helper's own
//!   last scan came back dirty and no `user-create` came after it: the daemon cannot
//!   reboot a node at will, nor on a verdict older than the lease now running. The
//!   verdict is used once.

use std::sync::atomic::Ordering;

use super::Helper;
use crate::autologin::{LAUNCHCTL, remove};
use crate::proto::{Reply, SCAN_LINE_BYTES, SCAN_LINES};
use crate::scan::{self, Diff, read_baseline, read_window, write_baseline};

/// What a scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub diff: Diff,
    pub errors: Vec<String>,
}

impl ScanReport {
    /// Nothing added, nothing removed, nothing unreadable.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.diff.added.is_empty() && self.diff.removed.is_empty() && self.errors.is_empty()
    }
}

/// The reply to a scan, each list cut to what a frame holds.
pub(super) fn scanned(report: ScanReport) -> Reply {
    Reply::Scanned {
        clean: report.clean(),
        added: capped(report.diff.added),
        removed: capped(report.diff.removed),
        errors: capped(report.errors),
    }
}

/// At most [`SCAN_LINES`] lines of at most [`SCAN_LINE_BYTES`], then how many more.
fn capped(mut lines: Vec<String>) -> Vec<String> {
    let more = lines.len().saturating_sub(SCAN_LINES);
    lines.truncate(SCAN_LINES);
    for line in &mut lines {
        line.truncate(line.floor_char_boundary(SCAN_LINE_BYTES));
    }
    if more > 0 {
        lines.push(format!("and {more} more"));
    }
    lines
}

impl Helper {
    /// `scan`: see the module documentation.
    ///
    /// # Errors
    /// No baseline was recorded, or it cannot be read.
    pub fn scan(&self) -> Result<ScanReport, String> {
        let _ledger = self.lock();
        self.dirty.store(false, Ordering::Release);
        let baseline = read_baseline(&self.settings.scan.baseline)?
            .ok_or("no baseline was recorded; baseline first")?;
        let snapshot = scan::collect(self.host.as_ref(), &self.settings);
        let report = ScanReport {
            diff: scan::diff(&baseline, &snapshot.items),
            errors: snapshot.errors,
        };
        let clean = report.clean();
        self.dirty.store(!clean, Ordering::Release);
        tracing::info!(
            clean,
            added = ?report.diff.added,
            removed = ?report.diff.removed,
            errors = ?report.errors,
            "leak scan"
        );
        Ok(report)
    }

    /// `baseline`: see the module documentation. Returns how many items each class
    /// holds.
    ///
    /// # Errors
    /// Outside the window, an item could not be read, or the baseline not written.
    pub fn baseline(&self) -> Result<Vec<(String, usize)>, String> {
        let ledger = self.lock();
        if let Some(lease) = ledger.live_leases().first() {
            return Err(format!(
                "lease {lease}'s user exists; the baseline is recorded before the first lease user"
            ));
        }
        let paths = &self.settings.scan;
        let opened = read_window(&paths.window)?.ok_or(
            "the baseline window is closed: no apply or erase opened it, or a lease user was created since",
        )?;
        let boot = self
            .host
            .boot_session()
            .map_err(|why| format!("reading this boot's identifier: {why}"))?;
        if opened == boot {
            return Err(format!(
                "the baseline window was opened in this boot ({boot}); the baseline is recorded after the boot that follows"
            ));
        }
        let snapshot = scan::collect(self.host.as_ref(), &self.settings);
        if !snapshot.errors.is_empty() {
            return Err(format!(
                "the baseline would be incomplete: {}",
                snapshot.errors.join("; ")
            ));
        }
        write_baseline(&paths.baseline, &snapshot.items)
            .map_err(|why| format!("writing {}: {why}", paths.baseline.display()))?;
        self.dirty.store(false, Ordering::Release);
        let classes = snapshot.classes(&paths.folders);
        tracing::info!(?classes, "leak scan baseline recorded");
        Ok(classes)
    }

    /// `reboot-dirty`: see the module documentation.
    ///
    /// # Errors
    /// The last scan was not dirty (or is stale), or `launchctl` refused.
    pub fn reboot_dirty(&self) -> Result<(), String> {
        let _ledger = self.lock();
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Err(
                "the helper's last scan did not come back dirty, or a lease user was created since; scan first"
                    .to_owned(),
            );
        }
        tracing::warn!("the last scan was dirty; rebooting");
        let output = self
            .host
            .command(LAUNCHCTL, &["reboot", "system"])
            .map_err(|why| format!("running {LAUNCHCTL}: {why}"))?;
        if output.success {
            Ok(())
        } else {
            Err(format!(
                "launchctl reboot system failed: {}",
                output.stderr.trim()
            ))
        }
    }

    /// What the first lease user of a window ends, with the ledger's lock held: the
    /// baseline window closes, and a dirty verdict goes stale.
    pub(super) fn first_lease_user(&self) -> Result<(), String> {
        let window = &self.settings.scan.window;
        remove(window).map_err(|why| format!("closing {}: {why}", window.display()))?;
        self.dirty.store(false, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
