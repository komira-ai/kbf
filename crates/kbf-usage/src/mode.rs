//! The `--usage-profiles` setting.

use std::fmt;
use std::str::FromStr;

/// How much of the learning a farm does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UsageMode {
    /// Nothing is learned or stored.
    #[default]
    Off,
    /// Runs are learned and stored; the scheduler does not read the curves.
    Record,
    /// Runs are learned and stored, and the scheduler reads the curves.
    On,
}

impl UsageMode {
    /// Whether runs are learned and stored.
    #[must_use]
    pub fn learns(self) -> bool {
        false
    }

    /// Whether the scheduler reads the learned curves.
    #[must_use]
    pub fn serves(self) -> bool {
        false
    }

    /// The setting's spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        "off"
    }
}

impl fmt::Display for UsageMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The text is not a [`UsageMode`].
#[derive(Debug, thiserror::Error)]
#[error("unknown usage profile mode {0:?}: expected off, record or on")]
pub struct BadMode(String);

impl FromStr for UsageMode {
    type Err = BadMode;

    fn from_str(text: &str) -> Result<Self, BadMode> {
        Ok(Self::Off)
    }
}
