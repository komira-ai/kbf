//! What the raft directory records about its log beside the store's files: the log's
//! id and its voter set, written once when the log is created (docs/design/ha.md,
//! section 4.11). Every later start reads them from here, never from flags.
//!
//! The record is four lines of text: a header, the log id as 16 hex digits, this
//! server's id and the voters. This version creates only single-voter logs, so the
//! last two lines are always `server 1` and `voters 1`, and any other record is
//! refused.

use std::num::NonZeroU64;

use kbf_log::FarmLogId;
use kbf_store::{Fs, FsFile};

use super::RaftLogError;

/// The file that holds the record.
pub const RECORD: &str = "log-record";

/// The file a new record is written to before it is renamed over [`RECORD`].
pub const RECORD_TMP: &str = "log-record.tmp";

const HEADER: &str = "kbf raft log record 1\nlog_id ";
const MEMBERS: &str = "server 1\nvoters 1\n";

/// The recorded log id, or `None` for a directory that holds nothing yet besides
/// [`super::LOCK`]. A leftover [`RECORD_TMP`] (a create that died before its rename)
/// is removed first.
///
/// # Errors
/// [`RaftLogError::NotEmpty`] if the directory holds files but no record,
/// [`RaftLogError::Record`] if the record does not parse, and [`RaftLogError::Io`].
pub fn read<F: Fs>(fs: &F) -> Result<Option<FarmLogId>, RaftLogError> {
    let mut names = fs.list()?;
    // The lock `open_dir` holds is not part of the log.
    names.retain(|n| n != super::LOCK);
    if names.iter().any(|n| n == RECORD_TMP) {
        fs.remove(RECORD_TMP)?;
        names.retain(|n| n != RECORD_TMP);
    }
    if !names.iter().any(|n| n == RECORD) {
        return if names.is_empty() {
            Ok(None)
        } else {
            Err(RaftLogError::NotEmpty)
        };
    }
    let bytes = fs.read(RECORD)?;
    let id = std::str::from_utf8(&bytes).ok().and_then(parse);
    id.map(Some).ok_or(RaftLogError::Record)
}

/// Records `log_id` durably: written to [`RECORD_TMP`], synced, renamed over
/// [`RECORD`], and the directory synced.
///
/// # Errors
/// [`RaftLogError::Io`].
pub fn write<F: Fs>(fs: &F, log_id: FarmLogId) -> Result<(), RaftLogError> {
    let mut file = fs.create(RECORD_TMP)?;
    file.append(format!("{HEADER}{log_id}\n{MEMBERS}").as_bytes())?;
    file.sync()?;
    drop(file);
    fs.rename(RECORD_TMP, RECORD)?;
    fs.sync_dir()?;
    Ok(())
}

/// The log id of a record exactly as [`write`] writes it; anything else is `None`.
fn parse(text: &str) -> Option<FarmLogId> {
    let (hex, members) = text.strip_prefix(HEADER)?.split_once('\n')?;
    let n = u64::from_str_radix(hex, 16).ok()?;
    NonZeroU64::new(n)
        .map(FarmLogId::new)
        .filter(|id| format!("{id}\n{members}") == format!("{hex}\n{MEMBERS}"))
}
