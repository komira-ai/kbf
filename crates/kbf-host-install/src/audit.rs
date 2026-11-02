//! The audit log: one JSON line per invocation, appended to a root-owned 0600 file.
//!
//! The file is opened `O_APPEND | O_CREAT | O_NOFOLLOW` with mode 0600, and used only if it
//! is a regular file owned by the trusted user that neither its group nor others can
//! touch. A line is serialized, never formatted: a request that is not in the grammar is
//! recorded as an escaped, truncated string, so it cannot forge a line.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::trust::{Trust, nofollow};

/// The longest part of a refused command that is recorded, in characters.
pub const MAX_RAW_CHARS: usize = 200;

/// One line of the log.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Entry {
    /// RFC 3339, UTC.
    pub time: String,
    /// `install`, `restart`, `query`, `apply` (the Mac's apply job) or `prune` (what that job
    /// removed after it, or why it could not); none when the request was not understood.
    pub verb: Option<&'static str>,
    pub version: Option<String>,
    pub channel: Option<String>,
    /// The unit a `restart` named, only on the line of a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// `installed`, `already_installed`, `restarted`, `apply_started`, `applied` (the end of
    /// an apply job), `pruned` or `prune_failed` (its prune), `query`, `refused` or `failed`.
    pub outcome: &'static str,
    /// The closed reason code of a refusal or a failure.
    pub reason: Option<&'static str>,
    /// What became of an apply job (`done`, `failed:<reason>[:<what became of the old
    /// release>]`, the status file's line), only on the line the job appends when it ends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// The request as received, only when it was not understood.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// The time now, RFC 3339 in UTC.
pub fn now() -> String {
    // Formatting a date in this range does not fail.
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// `text` cut to [`MAX_RAW_CHARS`] characters.
pub fn truncate_raw(text: &str) -> String {
    text.chars().take(MAX_RAW_CHARS).collect()
}

/// An open audit log.
#[derive(Debug)]
pub struct Audit {
    file: File,
}

impl Audit {
    /// Open (creating) the log at `path`.
    ///
    /// # Errors
    /// The file cannot be opened, is a link, is not a regular file, is not the trusted
    /// user's, or is accessible to its group or to others.
    pub fn open(trust: &Trust, path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .custom_flags(nofollow())
            .mode(0o600)
            .open(path)?;
        let meta = file.metadata()?;
        // Not `||`: each test is a condition of the log, none a branch of the code.
        let usable = meta.is_file() & trust.owns(&meta) & (meta.mode() & 0o077 == 0);
        if !usable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not a regular file of the trusted user that only it can access",
            ));
        }
        Ok(Self { file })
    }

    /// Append `entry` as one line.
    ///
    /// # Errors
    /// The write failed.
    pub fn append(&mut self, entry: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_string(entry).map_err(io::Error::other)?;
        line.push('\n');
        // One `write` on an `O_APPEND` descriptor: the line lands whole.
        self.file.write_all(line.as_bytes())
    }
}

#[cfg(test)]
mod tests;
