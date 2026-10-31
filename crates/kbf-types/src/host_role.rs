//! The role a host plays in the farm, as the update coordinator limits and orders it
//! (`docs/design/updates.md`, section 3).

use std::fmt;
use std::str::FromStr;

/// What a host runs, which decides its turn in an update and how many hosts of its kind
/// may update at once.
///
/// This is not the certificate role of [`crate::Role`] (`storage`, `daemon`): an updater's
/// certificate has the role `updater`, and names the host's role in a second URI
/// subjectAltName, `urn:kbf:host-role:<name>` ([`HostRole::URI_PREFIX`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostRole {
    /// A frontend: updated after every other kind, and never the last one that is up.
    Frontend,
    /// A storage node, possibly with a daemon.
    Storage,
    /// A daemon-only Linux host.
    Daemon,
    /// A Mac that runs a daemon.
    MacDaemon,
}

/// A string that names no host role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a host role: frontend, storage, daemon or mac-daemon")]
pub struct HostRoleError;

impl HostRole {
    /// How an updater's certificate names the host's role: this, then [`HostRole::name`].
    pub const URI_PREFIX: &'static str = "urn:kbf:host-role:";

    /// Every host role, in the order [`Ord`] gives.
    pub const ALL: [Self; 4] = [Self::Frontend, Self::Storage, Self::Daemon, Self::MacDaemon];

    /// The name in a certificate, a file and the status: `frontend`, `storage`, `daemon` or
    /// `mac-daemon`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Frontend => "frontend",
            Self::Storage => "storage",
            Self::Daemon => "daemon",
            Self::MacDaemon => "mac-daemon",
        }
    }
}

impl FromStr for HostRole {
    type Err = HostRoleError;

    fn from_str(name: &str) -> Result<Self, HostRoleError> {
        Self::ALL
            .into_iter()
            .find(|role| role.name() == name)
            .ok_or(HostRoleError)
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
    fn each_host_role_has_one_spelling() {
        assert_eq!(
            HostRole::ALL.map(HostRole::name),
            ["frontend", "storage", "daemon", "mac-daemon"]
        );
        for role in HostRole::ALL {
            assert_eq!(role.name().parse(), Ok(role));
            assert_eq!(role.to_string(), role.name());
        }
        for bad in [
            "",
            "Frontend",
            "mac_daemon",
            " storage",
            "storage ",
            "admin",
        ] {
            assert_eq!(bad.parse::<HostRole>(), Err(HostRoleError), "{bad:?}");
        }
        assert_eq!(HostRole::URI_PREFIX, "urn:kbf:host-role:");
        assert_eq!(
            HostRoleError.to_string(),
            "not a host role: frontend, storage, daemon or mac-daemon"
        );
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
}
