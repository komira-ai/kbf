//! The version a roll installs, validated once, here, with the rules the host-side wrapper
//! applies to the same string.
//!
//! The wrapper is the last check before `apt-get` runs as root, and the version is the only
//! caller-chosen text that reaches it, so the rule is narrow: a Debian upstream version and
//! revision (`[0-9]` first, then letters, digits and `. + ~ -`), no epoch (`:`), at most
//! [`MAX_LEN`] bytes. That excludes, without naming them, a leading `-` (an option), a
//! newline or NUL (a second command, a truncated string), whitespace, quotes and every shell
//! metacharacter. [`ERE`] is the same rule as a POSIX extended regular expression; the
//! wrapper matches the whole string against it and [`ACCEPTED`] and [`REJECTED`] are the
//! corpus both sides are tested with.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The longest version accepted, in bytes.
pub const MAX_LEN: usize = 128;

/// The rule as a POSIX extended regular expression, anchored at both ends.
pub const ERE: &str = "^[0-9][A-Za-z0-9.+~-]{0,127}$";

/// Versions every implementation of the rule must accept.
pub const ACCEPTED: &[&str] = &[
    "1",
    "0.1.0",
    "1.2.3-1",
    "1.2.3~rc1-1",
    "20261021.1+git9ba38c2-2",
    "7.0.0-1~deb12u1",
];

/// Strings every implementation of the rule must refuse, each a way the rule could be loosened
/// by mistake.
pub const REJECTED: &[&str] = &[
    "",
    "-1.0",
    "--version",
    "-o Dpkg::Options::=--force-all",
    "1.0\n",
    "\n1.0",
    "1.0\nfoo",
    "1.0\r",
    "1.0\0",
    "1.0\0foo",
    "1:2.0",
    "0:1.0",
    " 1.0",
    "1.0 ",
    "1.0 2.0",
    "1.0\t",
    "a1.0",
    ".1",
    "1.0;id",
    "1.0&&id",
    "1.0|id",
    "$(id)",
    "1.0`id`",
    "1.0'",
    "1.0\"",
    "1.0\\",
    "1.0/2",
    "1.0_2",
    "1.0=2",
    "\u{ff11}.0",
    "1.0\u{e9}",
    "1.0\u{2028}",
];

/// Why a string is not a version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    #[error("the version is empty")]
    Empty,
    #[error("the version is longer than {MAX_LEN} bytes")]
    TooLong,
    #[error("the version must start with a digit")]
    FirstNotDigit,
    #[error("the version has a character outside letters, digits and . + ~ -")]
    Character,
}

/// A validated version. Only [`Version::parse`] makes one, and deserializing runs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl Version {
    pub fn parse(s: &str) -> Result<Version, VersionError> {
        let first = s.bytes().next().ok_or(VersionError::Empty)?;
        if s.len() > MAX_LEN {
            return Err(VersionError::TooLong);
        }
        if !first.is_ascii_digit() {
            return Err(VersionError::FirstNotDigit);
        }
        if !s.bytes().all(allowed) {
            return Err(VersionError::Character);
        }
        Ok(Version(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn allowed(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'~' | b'-')
}

impl TryFrom<String> for Version {
    type Error = VersionError;

    fn try_from(s: String) -> Result<Version, VersionError> {
        Version::parse(&s)
    }
}

impl From<Version> for String {
    fn from(v: Version) -> String {
        v.0
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a rule that is looser than the corpus says: any of the hostile strings (an
    /// option, an embedded newline or NUL, an epoch, whitespace, a metacharacter, a non-ASCII
    /// digit) reaching the wrapper.
    #[test]
    fn every_hostile_string_is_refused() {
        for s in REJECTED {
            assert!(Version::parse(s).is_err(), "accepted {s:?}");
        }
    }

    /// Catches a rule that is stricter than real package versions: the nightly path would
    /// refuse to roll a version the archive holds.
    #[test]
    fn every_real_version_is_accepted() {
        for s in ACCEPTED {
            assert_eq!(Version::parse(s).expect(s).as_str(), *s);
        }
    }

    /// Catches a wrong reason: the refusals are told apart, and the 128-byte bound is
    /// inclusive (a `<` for `<=` drops a legal version; no bound lets a megabyte through).
    #[test]
    fn the_reasons_and_the_length_bound() {
        assert_eq!(Version::parse(""), Err(VersionError::Empty));
        assert_eq!(Version::parse("-1"), Err(VersionError::FirstNotDigit));
        assert_eq!(Version::parse("1:2"), Err(VersionError::Character));
        let longest = format!("1{}", "0".repeat(MAX_LEN - 1));
        assert!(Version::parse(&longest).is_ok());
        assert_eq!(
            Version::parse(&format!("{longest}0")),
            Err(VersionError::TooLong)
        );
        // A long string that also starts badly is reported as long: the bound is checked
        // before the content is walked.
        assert_eq!(
            Version::parse(&"-".repeat(MAX_LEN + 1)),
            Err(VersionError::TooLong)
        );
    }

    /// Catches a deserializer that skips the rule: a version read from a request body or
    /// from `roll.json` is as valid as one parsed by hand.
    #[test]
    fn deserializing_validates_and_serializing_round_trips() {
        let v: Version = serde_json::from_str("\"1.2.3-1\"").expect("valid");
        assert_eq!(serde_json::to_string(&v).expect("json"), "\"1.2.3-1\"");
        assert_eq!(v.to_string(), "1.2.3-1");
        let bad = serde_json::from_str::<Version>("\"-1\"");
        assert!(bad.is_err());
        assert!(
            bad.expect_err("bad")
                .to_string()
                .contains("start with a digit")
        );
    }

    /// Catches the ERE drifting from the function: the pattern must stay anchored at both
    /// ends (a mutant dropping `^` or `$` accepts a hostile prefix or suffix) and carry the
    /// same bound and alphabet.
    #[test]
    fn the_ere_states_the_same_rule() {
        assert!(ERE.starts_with('^') && ERE.ends_with('$'));
        assert!(ERE.contains("[0-9]"));
        assert!(ERE.contains("[A-Za-z0-9.+~-]"));
        assert!(ERE.contains(&format!("{{0,{}}}", MAX_LEN - 1)));
    }
}
