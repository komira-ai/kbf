//! The six requests the local socket takes, matched on the whole line.
//!
//! ```text
//! query
//! install kbf=<version> from <channel>
//! switch kbf=<version>
//! confirm kbf=<version>[ last_good=<version>]
//! revert
//! activate kbf=<commit|version>
//! ```
//!
//! The grammar of `<version>` and `<channel>` is that of the ssh forced command
//! ([`crate::request`]): single spaces, no leading or trailing space, no control byte, lower-case
//! verbs. The `<commit>` of `activate` is 12 to 40 lower-case hex digits ([`Target`]). Whatever fits none of this is [`BadCommand`], and nothing runs. The request carries
//! no path, no URL and no unit name: where a release comes from and what is restarted are the
//! root-owned configuration's.

use crate::mac::release::commit_prefix;
use crate::request::{BadCommand, Channel, Version};

/// The longest request line, in bytes.
pub const MAX_LINE_LEN: usize = 512;

/// An accepted request.
#[derive(Debug, Clone)]
pub enum LocalRequest {
    /// What is installed, what is on trial, and how the last update ended.
    Query,
    /// Fetch, verify and lay out `version` from `channel` beside the running release; no
    /// link moves.
    Install { version: Version, channel: Channel },
    /// Put the staged `version` in place on trial, and start the clock of the revert.
    Switch { version: Version },
    /// Keep the release on trial: it came up healthy. `last_good` is the coordinator's last
    /// good release, as the order carries it: the host keeps it whatever its age.
    Confirm {
        version: Version,
        last_good: Option<Version>,
    },
    /// Go back now to the release that ran before the one on trial.
    Revert,
    /// Make a release the host keeps the one that runs, for good: no fetch, no trial.
    Activate { target: Target },
}

/// The release an `activate` names: a commit (12 to 40 lower-case hex digits, the start of the
/// full one), or a version, whose `+g<12 hex>` names the commit, or whose text the host recorded
/// when it laid the release out.
#[derive(Debug, Clone)]
pub enum Target {
    Commit(String),
    Version(Version),
}

impl Target {
    /// Read `text`: hex digits are a commit, anything else must be a version.
    ///
    /// # Errors
    /// [`BadCommand`] for text that is neither.
    pub fn parse(text: &str) -> Result<Self, BadCommand> {
        let hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
        if (12..=40).contains(&text.len()) && text.bytes().all(hex) {
            return Ok(Self::Commit(text.to_owned()));
        }
        Version::new(text).map(Self::Version)
    }

    /// The text as it was given.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Commit(commit) => commit,
            Self::Version(version) => version.as_str(),
        }
    }

    /// Whether this names the release `commit`, which the host recorded as `version`.
    pub fn names(&self, commit: &str, version: Option<&str>) -> bool {
        match self {
            Self::Commit(prefix) => commit.starts_with(prefix.as_str()),
            Self::Version(wanted) => {
                version == Some(wanted.as_str())
                    || commit_prefix(wanted.as_str()).is_some_and(|p| commit.starts_with(p))
            }
        }
    }
}

impl LocalRequest {
    /// Match the whole of `text`.
    ///
    /// # Errors
    /// [`BadCommand`] for anything but the six forms.
    pub fn parse(text: &str) -> Result<Self, BadCommand> {
        match text {
            "query" => return Ok(Self::Query),
            "revert" => return Ok(Self::Revert),
            _ => {}
        }
        let version_of = |text: &str| Version::new(text);
        if let Some(rest) = text.strip_prefix("install kbf=") {
            let (version, channel) = rest.split_once(" from ").ok_or(BadCommand)?;
            return Ok(Self::Install {
                version: version_of(version)?,
                channel: Channel::new(channel)?,
            });
        }
        if let Some(target) = text.strip_prefix("activate kbf=") {
            return Ok(Self::Activate {
                target: Target::parse(target)?,
            });
        }
        if let Some(version) = text.strip_prefix("switch kbf=") {
            return Ok(Self::Switch {
                version: version_of(version)?,
            });
        }
        let rest = text.strip_prefix("confirm kbf=").ok_or(BadCommand)?;
        let (version, last_good) = match rest.split_once(" last_good=") {
            Some((version, last_good)) => (version, Some(version_of(last_good)?)),
            None => (rest, None),
        };
        Ok(Self::Confirm {
            version: version_of(version)?,
            last_good,
        })
    }

    /// The verb, for the audit line and the log.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Install { .. } => "stage",
            Self::Switch { .. } => "switch",
            Self::Confirm { .. } => "confirm",
            Self::Revert => "revert",
            Self::Activate { .. } => "activate",
        }
    }
}
