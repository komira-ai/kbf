//! The role a host plays in the farm, as the update coordinator limits and orders it
//! (`docs/design/updates.md`, section 3).

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

/// The longest host role name, in bytes.
const MAX_LEN: usize = 32;

/// What a host runs, which decides its turn in an update and how many hosts of its kind
/// may update at once.
///
/// A name is lower-case ASCII letters, digits and `-`: 1 to 32 bytes, starting with a
/// letter, with no trailing or doubled `-`. [`FromStr`] and [`HostRole::from_uri`] are the
/// only ways to make one from text; the four names the farm knows are the constants
/// [`HostRole::FRONTEND`], [`HostRole::STORAGE`], [`HostRole::DAEMON`] and
/// [`HostRole::MAC_DAEMON`].
///
/// This is not the certificate role of [`crate::Role`] (`storage`, `daemon`): an updater's
/// certificate has the role `updater`, and names the host's role in a second URI
/// subjectAltName, `urn:kbf:host-role:<name>` ([`HostRole::URI_PREFIX`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostRole(Cow<'static, str>);

/// A string that names no host role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a host role: frontend, storage, daemon or mac-daemon")]
pub struct HostRoleError;

impl HostRole {
    /// How an updater's certificate names the host's role: this, then [`HostRole::name`].
    pub const URI_PREFIX: &'static str = "urn:kbf:host-role:";

    /// A frontend: updated after every other kind, and never the last one that is up.
    pub const FRONTEND: Self = Self(Cow::Borrowed("frontend"));
    /// A storage node, possibly with a daemon.
    pub const STORAGE: Self = Self(Cow::Borrowed("storage"));
    /// A daemon-only Linux host.
    pub const DAEMON: Self = Self(Cow::Borrowed("daemon"));
    /// A Mac that runs a daemon.
    pub const MAC_DAEMON: Self = Self(Cow::Borrowed("mac-daemon"));

    /// The name in a certificate, a file and the status, such as `frontend` or `mac-daemon`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }

    /// [`FromStr`], then only the four named roles: how every verifier and issuer reads a
    /// role until they take any well-formed name.
    ///
    /// # Errors
    /// [`HostRoleError`] for a malformed name and for a well-formed one that is not
    /// `frontend`, `storage`, `daemon` or `mac-daemon`.
    pub fn parse_named(name: &str) -> Result<Self, HostRoleError> {
        let role: Self = name.parse()?;
        match role.0 {
            Cow::Borrowed(_) => Ok(role),
            Cow::Owned(_) => Err(HostRoleError),
        }
    }

    /// The role a certificate's URI names: [`HostRole::URI_PREFIX`] exactly, then a name.
    /// `None` for any other URI, and for a prefix followed by a malformed name.
    #[must_use]
    pub fn from_uri(uri: &str) -> Option<Self> {
        uri.strip_prefix(Self::URI_PREFIX)?.parse().ok()
    }

    /// The URI a certificate carries for this role.
    #[must_use]
    pub fn to_uri(&self) -> String {
        format!("{}{}", Self::URI_PREFIX, self.name())
    }
}

impl FromStr for HostRole {
    type Err = HostRoleError;

    fn from_str(name: &str) -> Result<Self, HostRoleError> {
        let bytes = name.as_bytes();
        let well_formed = (1..=MAX_LEN).contains(&bytes.len())
            && bytes[0].is_ascii_lowercase()
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            && !name.ends_with('-')
            && !name.contains("--");
        if !well_formed {
            return Err(HostRoleError);
        }
        // The four named roles share their static string.
        Ok([
            Self::FRONTEND,
            Self::STORAGE,
            Self::DAEMON,
            Self::MAC_DAEMON,
        ]
        .into_iter()
        .find(|role| role.name() == name)
        .unwrap_or_else(|| Self(Cow::Owned(name.to_owned()))))
    }
}

impl fmt::Display for HostRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a host role spelled other than the certificate table spells it, a name
    /// accepted in another letter case or with a space, and the prefix changed.
    #[test]
    fn the_four_old_names_keep_their_strings_and_uris() {
        let four = [
            (HostRole::FRONTEND, "frontend"),
            (HostRole::STORAGE, "storage"),
            (HostRole::DAEMON, "daemon"),
            (HostRole::MAC_DAEMON, "mac-daemon"),
        ];
        for (role, name) in four {
            assert_eq!(name.parse(), Ok(role.clone()));
            assert_eq!(role.name(), name);
            assert_eq!(role.to_string(), name);
            let uri = format!("urn:kbf:host-role:{name}");
            assert_eq!(role.to_uri(), uri);
            assert_eq!(HostRole::from_uri(&uri), Some(role));
        }
        assert_eq!(HostRole::URI_PREFIX, "urn:kbf:host-role:");
        assert_eq!(
            HostRoleError.to_string(),
            "not a host role: frontend, storage, daemon or mac-daemon"
        );
    }

    /// Catches: a URI accepted with another prefix or case, an empty or malformed name,
    /// or a doubled prefix read as a name.
    #[test]
    fn from_uri_takes_only_the_exact_prefix_and_a_well_formed_name() {
        assert_eq!(
            HostRole::from_uri("urn:kbf:host-role:cache").map(|r| r.to_uri()),
            Some("urn:kbf:host-role:cache".to_owned())
        );
        for bad in [
            "",
            "cache",
            "urn:kbf:host-role:",
            "urn:kbf:host-role:urn:kbf:host-role:cache",
            "URN:KBF:HOST-ROLE:cache",
            "urn:kbf:role:cache",
            "urn:kbf:host-role:Cache",
            "urn:kbf:host-role: cache",
            "urn:kbf:host-role:cache\0",
            " urn:kbf:host-role:cache",
        ] {
            assert_eq!(HostRole::from_uri(bad), None, "{bad:?}");
        }
    }

    /// Catches: the four names parsing to anything but their own strings, and a new
    /// well-formed name being refused as it was when the roles were a closed enum.
    #[test]
    fn a_new_well_formed_name_parses() {
        for name in ["cache", "a", "build-farm-2", "x1", &"a".repeat(32)] {
            let role: HostRole = name.parse().unwrap_or_else(|e| panic!("{name:?}: {e}"));
            assert_eq!(role.to_string(), name);
        }
    }

    /// Catches: a malformed name reaching a certificate, a row or an order: case, spaces,
    /// lookalike letters, separators, and the hyphen rules.
    #[test]
    fn malformed_names_are_refused() {
        let too_long = "a".repeat(33);
        for bad in [
            "",
            "Cache",
            "CACHE",
            "ca che",
            " cache",
            "cache ",
            "\tcache",
            "cache\n",
            "c\u{430}che",
            "\u{ff43}ache",
            "caché",
            "1cache",
            "-cache",
            "cache-",
            "ca--che",
            "ca_che",
            "ca.che",
            "ca/che",
            "ca%2Fche",
            "ca\0che",
            "\0",
            too_long.as_str(),
        ] {
            assert_eq!(bad.parse::<HostRole>(), Err(HostRoleError), "{bad:?}");
        }
    }

    /// Catches: a verifier or issuer reading a role outside the four before it is meant to.
    #[test]
    fn parse_named_takes_the_four_and_nothing_else() {
        for name in ["frontend", "storage", "daemon", "mac-daemon"] {
            assert_eq!(HostRole::parse_named(name), name.parse());
        }
        for bad in ["cache", "Frontend", "", "daemon "] {
            assert_eq!(HostRole::parse_named(bad), Err(HostRoleError), "{bad:?}");
        }
    }
}
