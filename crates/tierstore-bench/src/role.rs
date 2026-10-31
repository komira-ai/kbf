//! Role names: the only names a result file, a CSV or a log of this tool may carry for a
//! machine or a store. A role is bound to an endpoint, a bucket and a key in the run file,
//! which is never committed.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::ConfigError;

/// What a machine or a store is in the measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// The hot store: the farm's fast object store, test bucket only.
    Hot,
    /// The cold store: the farm's disk-backed object store, test bucket only.
    Cold,
    /// The tierstore under test: its S3 front.
    Instance,
    /// The fault proxy in front of the cold store (or, for one cell, the hot store).
    Proxy,
    /// A six-member test hot store on one host.
    TestSilo,
    /// The machine that sends the load, next to the instance.
    Generator,
    /// A load-sending machine other than the instance's.
    RemoteGenerator,
}

impl Role {
    /// Every role, in the order of the enum.
    pub const ALL: [Role; 7] = [
        Role::Hot,
        Role::Cold,
        Role::Instance,
        Role::Proxy,
        Role::TestSilo,
        Role::Generator,
        Role::RemoteGenerator,
    ];

    /// The name a result file, a flag and a log use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Hot => "hot",
            Role::Cold => "cold",
            Role::Instance => "instance",
            Role::Proxy => "proxy",
            Role::TestSilo => "test-silo",
            Role::Generator => "generator",
            Role::RemoteGenerator => "remote-generator",
        }
    }

    /// Whether the role is an S3 endpoint with a bucket and a key (the others are machines).
    #[must_use]
    pub fn is_store(self) -> bool {
        matches!(
            self,
            Role::Hot | Role::Cold | Role::Instance | Role::TestSilo
        )
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, ConfigError> {
        Role::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| ConfigError::UnknownRole(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a name that does not parse back to its role, which would make a flag
    /// `--endpoint hot=...` and the run file disagree with the result files.
    #[test]
    fn every_role_name_parses_back_and_matches_its_serde_name() {
        for role in Role::ALL {
            assert_eq!(role.to_string().parse::<Role>().unwrap(), role);
            assert_eq!(
                serde_json::to_string(&role).unwrap(),
                format!("\"{}\"", role.as_str())
            );
        }
    }

    /// Catches an unknown name being taken for a role.
    #[test]
    fn an_unknown_role_is_refused_by_name() {
        let err = "silo-1".parse::<Role>().unwrap_err();
        assert_eq!(
            err.to_string(),
            ConfigError::UnknownRole("silo-1".to_owned()).to_string()
        );
    }

    /// Catches a machine role being given a bucket, or a store role being refused one.
    #[test]
    fn only_the_four_stores_are_store_roles() {
        let stores: Vec<Role> = Role::ALL.into_iter().filter(|r| r.is_store()).collect();
        assert_eq!(
            stores,
            [Role::Hot, Role::Cold, Role::Instance, Role::TestSilo]
        );
    }
}
