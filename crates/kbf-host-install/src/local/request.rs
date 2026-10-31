//! The five requests the local socket takes, matched on the whole line.
//!
//! ```text
//! query
//! install kbf=<version> from <channel>
//! switch kbf=<version>
//! confirm kbf=<version>
//! revert
//! ```
//!
//! A request may name the service it is for. kbf is the service of every request above, and
//! the form of another service puts its name where `kbf` is (`install js-agent=<version>
//! from <channel>`, `switch js-agent=<version>`, `confirm js-agent=<version>`) or after the
//! verb (`query js-agent`, `revert js-agent`); the rest of each line is as above
//! ([`Addressed`]). Today's lines mean what they always meant.
//!
//! The grammar of `<version>` and `<channel>` is that of the ssh forced command
//! ([`crate::request`]): single spaces, no leading or trailing space, no control byte, lower-case
//! verbs. Whatever fits none of this is [`BadCommand`], and nothing runs. The request carries
//! no path, no URL and no unit name: where a release comes from and what is restarted are the
//! root-owned configuration's.

use crate::request::{BadCommand, Channel, Version};
use crate::service::ServiceName;

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
    /// Keep the release on trial: it came up healthy.
    Confirm { version: Version },
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
        let version = text.strip_prefix("confirm kbf=").ok_or(BadCommand)?;
        Ok(Self::Confirm {
            version: version_of(version)?,
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

/// A request and the service it is for.
#[derive(Debug, Clone)]
pub struct Addressed {
    pub service: ServiceName,
    pub request: LocalRequest,
}

impl Addressed {
    /// Match the whole of `text`: one of the five forms of kbf's, or the form of a service.
    ///
    /// # Errors
    /// [`BadCommand`] for anything but those.
    pub fn parse(text: &str) -> Result<Self, BadCommand> {
        if let Ok(request) = LocalRequest::parse(text) {
            return Ok(Self {
                service: ServiceName::kbf(),
                request,
            });
        }
        if let Some(name) = text.strip_prefix("query ") {
            return Ok(Self {
                service: ServiceName::new(name)?,
                request: LocalRequest::Query,
            });
        }
        if let Some(name) = text.strip_prefix("revert ") {
            return Ok(Self {
                service: ServiceName::new(name)?,
                request: LocalRequest::Revert,
            });
        }
        // `<verb> <service>=<rest>` is `<verb> kbf=<rest>` for the service.
        let (verb, tail) = text.split_once(' ').ok_or(BadCommand)?;
        let (name, rest) = tail.split_once('=').ok_or(BadCommand)?;
        if !matches!(verb, "install" | "switch" | "confirm") {
            return Err(BadCommand);
        }
        Ok(Self {
            service: ServiceName::new(name)?,
            request: LocalRequest::parse(&format!("{verb} kbf={rest}"))?,
        })
    }
}
