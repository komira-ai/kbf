//! The three requests this program accepts, matched on the whole string.
//!
//! ```text
//! install kbf=<version> from <channel>
//! restart frontend=<name>
//! query
//! ```
//!
//! - single spaces only, no leading or trailing space, no tab, newline, CR or NUL, no
//!   extra token, lower-case verbs;
//! - `version`: `[0-9][0-9A-Za-z.+~-]*`, at most 128 bytes (and 230 once `+` and `~` count
//!   twice, the room the unit name has), no epoch (no `:`), and a
//!   version dpkg itself would parse (so `1.0-` is refused);
//! - `channel`: `[a-z0-9][a-z0-9-]{0,31}`;
//! - `name`: `-` for the plain unit `kbf-frontend.service`, else an instance
//!   `[a-z][a-z0-9-]{0,31}` of `kbf-frontend@<name>.service`.
//!
//! The package name is fixed (`kbf`), and so is the one family of units a restart can name.
//! Whatever fits none of this is [`BadCommand`], and nothing runs.

use crate::debver::DebVersion;

/// The longest version, in bytes.
pub const MAX_VERSION_LEN: usize = 128;

/// The longest version, in bytes, once `+` and `~` are spelled as two bytes each in the
/// unit name (see `plan::unit_name`): the unit name is the 17-byte prefix, this, and
/// `.service`, so it stays within systemd's 255 bytes.
pub const MAX_UNIT_VERSION_LEN: usize = 230;

/// The longest channel name, in bytes.
pub const MAX_CHANNEL_LEN: usize = 32;

/// The longest frontend instance name, in bytes.
pub const MAX_FRONTEND_LEN: usize = 32;

/// The unit of a frontend that has no instance name.
const FRONTEND_UNIT: &str = "kbf-frontend.service";

/// A request that is not one of the three forms.
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
    pub fn new(text: &str) -> Result<Self, BadCommand> {
        if !in_grammar(text) {
            return Err(BadCommand);
        }
        let parsed = DebVersion::parse(text).map_err(|_| BadCommand)?;
        Ok(Self {
            text: text.to_owned(),
            parsed,
        })
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

/// Whether `text` is within the version grammar, before dpkg's own parse is asked: a digit
/// first (so never a hyphen, which `apt-get` would take for an option), then letters,
/// digits and `.+~-`, within both length limits.
fn in_grammar(text: &str) -> bool {
    let Some((first, rest)) = text.split_at_checked(1) else {
        return false;
    };
    // `+` and `~` take two bytes in the unit name.
    let unit_len = text.len() + text.bytes().filter(|b| b"+~".contains(b)).count();
    text.len() <= MAX_VERSION_LEN
        && unit_len <= MAX_UNIT_VERSION_LEN
        && first.starts_with(|c: char| c.is_ascii_digit())
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+~-".contains(&b))
}

/// A channel name: a key of the allowlist, never a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel(String);

impl Channel {
    /// Check `text` against the channel grammar.
    ///
    /// # Errors
    /// [`BadCommand`] when it is not in the grammar.
    pub fn new(text: &str) -> Result<Self, BadCommand> {
        let (first, rest) = text.split_at_checked(1).ok_or(BadCommand)?;
        let lower_or_digit = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let well_formed = text.len() <= MAX_CHANNEL_LEN
            && first.bytes().all(lower_or_digit)
            && rest.bytes().all(|b| lower_or_digit(b) || b == b'-');
        if !well_formed {
            return Err(BadCommand);
        }
        Ok(Self(text.to_owned()))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A systemd unit a request may restart: a frontend, spelled only from a checked name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit(String);

impl Unit {
    /// The frontend unit `name` stands for: `-` is `kbf-frontend.service`, an instance name
    /// is `kbf-frontend@<name>.service`.
    ///
    /// # Errors
    /// [`BadCommand`] when `name` is neither `-` nor an instance name in the grammar.
    pub fn frontend(name: &str) -> Result<Self, BadCommand> {
        if name == "-" {
            return Ok(Self(FRONTEND_UNIT.to_owned()));
        }
        let (first, rest) = name.split_at_checked(1).ok_or(BadCommand)?;
        let lower_or_digit = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let well_formed = name.len() <= MAX_FRONTEND_LEN
            && first.bytes().all(|b| b.is_ascii_lowercase())
            && rest.bytes().all(|b| lower_or_digit(b) || b == b'-');
        if !well_formed {
            return Err(BadCommand);
        }
        Ok(Self(format!("kbf-frontend@{name}.service")))
    }

    /// A unit of the root-owned configuration of the local socket: `kbf-<name>.service`, or
    /// `kbf-<name>@<instance>.service`, `<name>` of lower-case letters, digits and `-` and
    /// `<instance>` as a frontend's. The unit name is a file name's worth of text and
    /// nothing else, so it cannot be taken for an option or name another kind of unit.
    ///
    /// # Errors
    /// [`BadCommand`] when `text` is not such a name.
    pub fn service(text: &str) -> Result<Self, BadCommand> {
        let stem = text.strip_suffix(".service").ok_or(BadCommand)?;
        let (base, instance) = match stem.split_once('@') {
            Some((base, instance)) => (base, Some(instance)),
            None => (stem, None),
        };
        let plain = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        let base_ok = base.len() <= MAX_SERVICE_LEN
            && base.strip_prefix("kbf-").is_some_and(|rest| {
                !rest.is_empty() && rest.bytes().all(plain) && !rest.ends_with('-')
            });
        if !base_ok {
            return Err(BadCommand);
        }
        if let Some(instance) = instance {
            Self::frontend(instance)?;
            if instance == "-" {
                return Err(BadCommand);
            }
        }
        Ok(Self(text.to_owned()))
    }

    /// The unit name, safe as an argument and as JSON.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The longest base name of a configured unit, in bytes.
const MAX_SERVICE_LEN: usize = 64;

/// An accepted request.
#[derive(Debug, Clone)]
pub enum Request {
    /// `install kbf=<version> from <channel>`.
    Install { version: Version, channel: Channel },
    /// `restart frontend=<name>`.
    Restart { unit: Unit },
    /// `query`.
    Query,
}

impl Request {
    /// Match the whole of `text`.
    ///
    /// # Errors
    /// [`BadCommand`] for anything but the three forms.
    pub fn parse(text: &str) -> Result<Self, BadCommand> {
        if text == "query" {
            return Ok(Self::Query);
        }
        if let Some(name) = text.strip_prefix("restart frontend=") {
            return Ok(Self::Restart {
                unit: Unit::frontend(name)?,
            });
        }
        let rest = text.strip_prefix("install kbf=").ok_or(BadCommand)?;
        let (version, channel) = rest.split_once(" from ").ok_or(BadCommand)?;
        Ok(Self::Install {
            version: Version::new(version)?,
            channel: Channel::new(channel)?,
        })
    }

    /// The verb, for the audit line.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Install { .. } => "install",
            Self::Restart { .. } => "restart",
            Self::Query => "query",
        }
    }
}

#[cfg(test)]
mod tests;
