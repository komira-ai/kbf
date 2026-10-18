//! A host that answers `setup`'s questions from fields, so a test needs no root, no accounts
//! and no Envoy.

use std::collections::BTreeSet;
use std::io;

use kbf_host::{Owner, RunUser, Untrusted};

use super::os::Os;
use crate::envoy::{EnvoyVersion, VersionError, check_output};
use crate::layout::Platform;

pub struct FakeOs {
    pub euid: u32,
    pub platform: Platform,
    pub users: Vec<RunUser>,
    pub subuid: String,
    pub subgid: String,
    pub lingering: BTreeSet<String>,
    pub enabled: BTreeSet<String>,
    pub loaded: BTreeSet<String>,
    pub dscl_users: String,
    pub dscl_groups: String,
    pub root: Owner,
    pub untrusted: bool,
    /// What `envoy --version` printed, or `None` for no Envoy installed.
    pub envoy_says: Option<String>,
    /// The bytes `random` returns, repeated to the length asked.
    pub random_byte: u8,
}

pub const ENVOY_IN_RANGE: &str =
    "envoy  version: 0123456789abcdef0123456789abcdef01234567/1.39.3/Clean/RELEASE/BoringSSL";
pub const ENVOY_TOO_NEW: &str =
    "envoy  version: 0123456789abcdef0123456789abcdef01234567/1.40.0/Clean/RELEASE/BoringSSL";

pub fn user(name: &str, id: u32) -> RunUser {
    RunUser {
        name: name.to_owned(),
        uid: id,
        gid: id,
        groups: vec![id],
        home: "/nonexistent".to_owned(),
    }
}

impl FakeOs {
    /// A Linux host, as root, with the users the package's sysusers entries make and an
    /// Envoy in range, and nothing else set up.
    pub fn linux() -> FakeOs {
        FakeOs {
            euid: 0,
            platform: Platform::Linux,
            users: vec![user("kbf", 990), user("kbf-server", 989)],
            subuid: String::new(),
            subgid: String::new(),
            lingering: BTreeSet::new(),
            enabled: BTreeSet::new(),
            loaded: BTreeSet::new(),
            dscl_users: String::new(),
            dscl_groups: String::new(),
            root: Owner { uid: 0, gid: 0 },
            untrusted: false,
            envoy_says: Some(ENVOY_IN_RANGE.to_owned()),
            random_byte: 0xab,
        }
    }

    pub fn mac() -> FakeOs {
        FakeOs {
            platform: Platform::MacOs,
            users: Vec::new(),
            envoy_says: None,
            ..FakeOs::linux()
        }
    }
}

impl Os for FakeOs {
    fn euid(&self) -> u32 {
        self.euid
    }

    fn trusted_binary(&self) -> Result<(), Untrusted> {
        if self.untrusted {
            return Err(Untrusted::Writable {
                path: "/opt/x/kbf-installer".into(),
                mode: 0o775,
            });
        }
        Ok(())
    }

    fn platform(&self) -> Platform {
        self.platform
    }

    fn root_owner(&self) -> Owner {
        self.root
    }

    fn user(&self, name: &str) -> io::Result<Option<RunUser>> {
        Ok(self.users.iter().find(|u| u.name == name).cloned())
    }

    fn subuid(&self) -> io::Result<String> {
        Ok(self.subuid.clone())
    }

    fn subgid(&self) -> io::Result<String> {
        Ok(self.subgid.clone())
    }

    fn lingers(&self, user: &str) -> io::Result<bool> {
        Ok(self.lingering.contains(user))
    }

    fn unit_enabled(&self, unit: &str) -> io::Result<bool> {
        Ok(self.enabled.contains(unit))
    }

    fn launchd_loaded(&self, label: &str) -> io::Result<bool> {
        Ok(self.loaded.contains(label))
    }

    fn dscl_list(&self, what: &str) -> io::Result<String> {
        Ok(if what.starts_with("/Users") {
            self.dscl_users.clone()
        } else {
            self.dscl_groups.clone()
        })
    }

    fn envoy(&self) -> Result<EnvoyVersion, VersionError> {
        match &self.envoy_says {
            Some(text) => check_output(text),
            None => Err(VersionError::Run {
                path: super::paths::ENVOY.to_owned(),
                source: io::Error::from(io::ErrorKind::NotFound),
            }),
        }
    }

    fn random(&self, len: usize) -> io::Result<Vec<u8>> {
        Ok(vec![self.random_byte; len])
    }
}
