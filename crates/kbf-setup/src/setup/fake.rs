//! A host that answers `setup`'s questions from fields, so a test needs no root and no accounts.

use std::collections::BTreeSet;
use std::io;

use kbf_host::{Owner, RunUser, Untrusted};

use super::os::Os;
use crate::layout::Platform;

pub struct FakeOs {
    pub euid: u32,
    pub platform: Platform,
    pub users: Vec<RunUser>,
    /// The login shell of an account named here; any other has the shell `setup` makes.
    pub shells: Vec<(String, String)>,
    pub enabled: BTreeSet<String>,
    pub root: Owner,
    pub untrusted: bool,
    /// The bytes `random` returns, repeated to the length asked.
    pub random_byte: u8,
}

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
    /// A Linux host, as root, with the users the package's sysusers entries make and
    /// nothing else set up.
    pub fn linux() -> FakeOs {
        FakeOs {
            euid: 0,
            platform: Platform::Linux,
            users: vec![user("kbf-server", 989)],
            shells: Vec::new(),
            enabled: BTreeSet::new(),
            root: Owner { uid: 0, gid: 0 },
            untrusted: false,
            random_byte: 0xab,
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

    fn login_shell(&self, name: &str) -> io::Result<String> {
        let made = "/usr/sbin/nologin";
        Ok(self
            .shells
            .iter()
            .find(|(n, _)| n == name)
            .map_or(made, |(_, s)| s)
            .to_owned())
    }

    fn unit_enabled(&self, unit: &str) -> io::Result<bool> {
        Ok(self.enabled.contains(unit))
    }

    fn random(&self, len: usize) -> io::Result<Vec<u8>> {
        Ok(vec![self.random_byte; len])
    }
}
