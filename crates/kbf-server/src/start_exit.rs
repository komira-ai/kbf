//! How a start that failed exits: "refused" (exit 2) or "not ready" (exit 75).
//!
//! The units of `storage`, `storage-and-daemon` and `all-in-one` carry
//! `RestartPreventExitStatus=2`, so a start that can succeed on a later try must not
//! exit 2: a node whose data directory is still held by the process that is stopping,
//! or whose registration the frontend answered with a boot the log has used, would stay
//! down until someone started it.
//!
//! - **Refused** (2, never restarted): the default. A flag, a certificate or key that
//!   cannot be read, a log directory that is not a log, a log of several voters: a
//!   restart reads the same bytes and fails the same way.
//! - **Not ready** (75, `EX_TEMPFAIL`, restarted after the unit's `RestartSec=`): only
//!   the failures named below, found by their type in the error's source chain, never by
//!   their text.
//!   - [`StaleWriter`]: the frontend handed out a boot the node's log has set already
//!     (its state store was restored from an older copy). Every registration takes the
//!     next boot of the member, so a restart registers again and, once its boot is past
//!     the log's, opens; a gap of N boots takes N restarts.
//!   - A directory lock another process holds ([`RaftLogError::Locked`],
//!     [`FsStoreError::Locked`]): the previous process, still stopping.
//!   - A file system call that failed for a reason a later try can clear (the device is
//!     busy, read-only or full), as [`io_not_ready`] says.

use std::error::Error;
use std::io;
use std::process::ExitCode;

use kbf_front::CacheError;
use kbf_meta::StaleWriter;
use kbf_objstore::FsStoreError;

use crate::RaftLogError;
use crate::write_tier::WriteTierError;

/// The exit status of a start that was refused.
pub const REFUSED: u8 = 2;
/// The exit status of a start that failed for a reason a later try can clear
/// (`EX_TEMPFAIL`).
pub const NOT_READY: u8 = 75;

/// Why a start failed, as far as a restart is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartFailure {
    /// The same start fails the same way: [`REFUSED`].
    Refused,
    /// A later start can succeed: [`NOT_READY`].
    NotReady,
}

impl StartFailure {
    /// The class of `e`: not ready if `e` or an error in its source chain is one of the
    /// types the module docs name, refused otherwise.
    #[must_use]
    pub fn of(e: &(dyn Error + 'static)) -> Self {
        let mut at = Some(e);
        while let Some(error) = at {
            if not_ready(error) {
                return Self::NotReady;
            }
            at = error.source();
        }
        Self::Refused
    }

    /// The process exit status.
    #[must_use]
    pub fn status(self) -> u8 {
        match self {
            Self::Refused => REFUSED,
            Self::NotReady => NOT_READY,
        }
    }

    /// [`StartFailure::status`] as an [`ExitCode`].
    #[must_use]
    pub fn code(self) -> ExitCode {
        ExitCode::from(self.status())
    }
}

/// What an operator must know beside the error, if `e` or an error in its source chain
/// calls for it: a [`StaleWriter`] means the frontend's state store is older than this
/// node's log (restored from a backup), and every restart until the boots catch up says
/// so, so the restarts do not read as an ordinary crash loop.
#[must_use]
pub fn hint(e: &(dyn Error + 'static)) -> Option<&'static str> {
    let mut at = Some(e);
    while let Some(error) = at {
        if stale_writer(error) {
            return Some(STALE_WRITER_HINT);
        }
        at = error.source();
    }
    None
}

/// What a start that failed with `e` says on stderr, each line after `name`: the error,
/// then [`hint`]'s line if there is one.
#[must_use]
pub fn said(name: &str, e: &(dyn Error + 'static)) -> String {
    let mut text = format!("{name}: {e}\n");
    if let Some(line) = hint(e) {
        text.push_str(&format!("{name}: {line}\n"));
    }
    text
}

/// The line [`hint`] gives for a [`StaleWriter`].
pub const STALE_WRITER_HINT: &str = "the frontend's state store is older than this node's \
    metadata log (was it restored from a backup?): the node exits 75, systemd starts it \
    again and each start takes the next boot, so it opens once its boot is past the log's; \
    wait, do not wipe --meta-dir (docs/deploy/kbf-modes.md, \"After restoring the frontend's \
    state store\")";

fn stale_writer(e: &(dyn Error + 'static)) -> bool {
    // `CacheError::StaleWriter` is `transparent`: the source chain does not name the
    // inner error, so the cache's own variant is matched.
    e.is::<StaleWriter>() || matches!(e.downcast_ref(), Some(CacheError::StaleWriter(_)))
}

/// Whether `e` itself (not its sources) is one of the not-ready types.
fn not_ready(e: &(dyn Error + 'static)) -> bool {
    if stale_writer(e) {
        return true;
    }
    if let Some(error) = e.downcast_ref::<RaftLogError>() {
        return match error {
            RaftLogError::Locked { .. } => true,
            RaftLogError::Io(source) => io_not_ready(source),
            _ => false,
        };
    }
    if let Some(error) = e.downcast_ref::<FsStoreError>() {
        return match error {
            FsStoreError::Locked { .. } => true,
            FsStoreError::Io { source, .. } => io_not_ready(source),
        };
    }
    if let Some(WriteTierError::Dir(source)) = e.downcast_ref::<WriteTierError>() {
        return io_not_ready(source);
    }
    false
}

/// Whether a file system failure can clear by itself: the device is busy, read-only,
/// full or slow. A missing path or a permission is the setup's, and a restart reads the
/// same.
fn io_not_ready(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ResourceBusy
            | io::ErrorKind::ReadOnlyFilesystem
            | io::ErrorKind::StorageFull
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

#[cfg(test)]
#[path = "start_exit_tests.rs"]
mod tests;
