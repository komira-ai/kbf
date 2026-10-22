//! The two requests this program accepts, matched on the whole string.
//!
//! ```text
//! install kbf=<version> from <channel>
//! query
//! ```
//!
//! - single spaces only, no leading or trailing space, no tab, newline, CR or NUL, no
//!   extra token, lower-case verbs;
//! - `version`: `[0-9][0-9A-Za-z.+~-]*`, at most 128 bytes, no epoch (no `:`), and a
//!   version dpkg itself would parse (so `1.0-` is refused);
//! - `channel`: `[a-z0-9][a-z0-9-]{0,31}`.
//!
//! The package name is fixed (`kbf`). Whatever fits none of this is [`BadCommand`], and
//! nothing runs.

use crate::debver::DebVersion;

/// The longest version, in bytes.
pub const MAX_VERSION_LEN: usize = 128;

/// The longest channel name, in bytes.
pub const MAX_CHANNEL_LEN: usize = 32;

/// A request that is not one of the two forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadCommand;

/// A version of the package to install: the text (a safe argument) and its parse.
#[derive(Debug, Clone)]
pub struct Version {
    text: String,
    parsed: DebVersion,
}

impl Version {
    /// Check `text` against the version grammar.
    ///
    /// # Errors
    /// [`BadCommand`] when it is not in the grammar.
    pub fn new(_text: &str) -> Result<Self, BadCommand> {
        todo!()
    }

    /// The text, safe as an argument and as JSON.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The parsed version, for ordering.
    pub fn parsed(&self) -> &DebVersion {
        &self.parsed
    }
}

/// A channel name: a key of the allowlist, never a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel(String);

impl Channel {
    /// Check `text` against the channel grammar.
    ///
    /// # Errors
    /// [`BadCommand`] when it is not in the grammar.
    pub fn new(_text: &str) -> Result<Self, BadCommand> {
        todo!()
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An accepted request.
#[derive(Debug, Clone)]
pub enum Request {
    /// `install kbf=<version> from <channel>`.
    Install { version: Version, channel: Channel },
    /// `query`.
    Query,
}

impl Request {
    /// Match the whole of `text`.
    ///
    /// # Errors
    /// [`BadCommand`] for anything but the two forms.
    pub fn parse(_text: &str) -> Result<Self, BadCommand> {
        todo!()
    }

    /// The verb, for the audit line.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Install { .. } => "install",
            Self::Query => "query",
        }
    }
}

#[cfg(test)]
mod tests;
