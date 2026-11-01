//! The five requests the local socket takes, matched on the whole line.
//!
//! ```text
//! query
//! install kbf=<version> from <channel>
//! switch kbf=<version>
//! confirm kbf=<version>[ last_good=<version>]
//! revert
//! ```
//!
//! The grammar of `<version>` and `<channel>` is that of the ssh forced command
//! ([`crate::request`]): single spaces, no leading or trailing space, no control byte, lower-case
//! verbs. Whatever fits none of this is [`BadCommand`], and nothing runs. The request carries
//! no path, no URL and no unit name: where a release comes from and what is restarted are the
//! root-owned configuration's.

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
}

impl LocalRequest {
    /// Match the whole of `text`.
    ///
    /// # Errors
    /// [`BadCommand`] for anything but the five forms.
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
        }
    }
}
