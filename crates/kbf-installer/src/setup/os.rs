//! What `setup` needs to know about the host's accounts and services, behind a trait so a test
//! plays the host and never needs root. The trait only answers questions; the changes are
//! steps of a `kbf-host` plan, so a dry look and an apply agree.

use std::io;

use kbf_host::{Cmd, Exec, HostPath, Kind, RunUser, Untrusted};

use super::paths;
use crate::envoy::{EnvoyVersion, VersionError, check_installed};
use crate::layout::Platform;
use crate::run::SystemRunner;

pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
pub const LAUNCHCTL: &str = "/bin/launchctl";
pub const DSCL: &str = "/usr/bin/dscl";
const LINGER_DIR: &str = "/var/lib/systemd/linger";

/// The host, as `setup` asks about it.
pub trait Os {
    /// The effective uid. `setup` refuses unless it is 0; the test fake says 0.
    fn euid(&self) -> u32;
    /// Refuses unless the running binary and the directories above it are root's alone.
    fn trusted_binary(&self) -> Result<(), Untrusted>;
    fn platform(&self) -> Platform;
    /// The ids files in `root` are chowned to (0:0; a test that writes a real scratch root
    /// as itself says its own).
    fn root_owner(&self) -> kbf_host::Owner {
        kbf_host::Owner { uid: 0, gid: 0 }
    }
    /// The account `name`, `None` when the host has none.
    fn user(&self, name: &str) -> io::Result<Option<RunUser>>;
    /// The text of `/etc/subuid` and `/etc/subgid` (empty when the file is missing).
    fn subuid(&self) -> io::Result<String>;
    fn subgid(&self) -> io::Result<String>;
    /// Whether systemd keeps `user`'s manager alive without a login.
    fn lingers(&self, user: &str) -> io::Result<bool>;
    /// Whether the systemd unit is enabled.
    fn unit_enabled(&self, unit: &str) -> io::Result<bool>;
    /// Whether the launchd label is loaded in the system domain.
    fn launchd_loaded(&self, label: &str) -> io::Result<bool>;
    /// `dscl . -list <what>` (macOS): `/Users UniqueID` or `/Groups PrimaryGroupID`.
    fn dscl_list(&self, what: &str) -> io::Result<String>;
    /// The installed Envoy's version, or why it is refused (missing, unreadable, outside the
    /// tested range).
    fn envoy(&self) -> Result<EnvoyVersion, VersionError>;
    /// `len` bytes from the system's random source.
    fn random(&self, len: usize) -> io::Result<Vec<u8>>;
}

/// The real host, read through `kbf-host`'s [`Exec`].
pub struct SystemOs<'a, E: Exec> {
    exec: &'a E,
    platform: Platform,
}

impl<'a, E: Exec> SystemOs<'a, E> {
    pub fn new(exec: &'a E, platform: Platform) -> SystemOs<'a, E> {
        SystemOs { exec, platform }
    }

    fn path(p: &str) -> io::Result<HostPath> {
        HostPath::new(p).map_err(io::Error::other)
    }

    fn text(&self, path: &str) -> io::Result<String> {
        match self.exec.inspect(&Self::path(path)?)? {
            Some(kbf_host::Entry::File { bytes, .. }) => {
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
            _ => Ok(String::new()),
        }
    }

    /// Exit status 0 of a command that changes nothing.
    fn succeeds(&self, cmd: &Cmd) -> io::Result<bool> {
        Ok(self.exec.query(cmd)?.status.success())
    }
}

impl<E: Exec> Os for SystemOs<'_, E> {
    fn euid(&self) -> u32 {
        self.exec.euid()
    }

    fn trusted_binary(&self) -> Result<(), Untrusted> {
        self.exec.check_binary()
    }

    fn platform(&self) -> Platform {
        self.platform
    }

    fn user(&self, name: &str) -> io::Result<Option<RunUser>> {
        kbf_host::user::lookup(self.exec, name).map_err(io::Error::other)
    }

    fn subuid(&self) -> io::Result<String> {
        self.text("/etc/subuid")
    }

    fn subgid(&self) -> io::Result<String> {
        self.text("/etc/subgid")
    }

    fn lingers(&self, user: &str) -> io::Result<bool> {
        let path = Self::path(&format!("{LINGER_DIR}/{user}"))?;
        Ok(self.exec.stat(&path)?.is_some_and(|m| m.kind == Kind::File))
    }

    fn unit_enabled(&self, unit: &str) -> io::Result<bool> {
        let cmd = Cmd::new(Self::path(SYSTEMCTL)?)
            .arg("is-enabled")
            .arg("--")
            .arg(unit);
        let out = self.exec.query(&cmd)?;
        Ok(out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "enabled")
    }

    fn launchd_loaded(&self, label: &str) -> io::Result<bool> {
        let cmd = Cmd::new(Self::path(LAUNCHCTL)?)
            .arg("print")
            .arg(format!("system/{label}"));
        self.succeeds(&cmd)
    }

    fn dscl_list(&self, what: &str) -> io::Result<String> {
        let mut cmd = Cmd::new(Self::path(DSCL)?).arg(".").arg("-list");
        for word in what.split(' ') {
            cmd = cmd.arg(word);
        }
        let out = self.exec.query(&cmd)?;
        kbf_host::exec::succeeded(&cmd, out)
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn envoy(&self) -> Result<EnvoyVersion, VersionError> {
        check_installed(&SystemRunner, std::path::Path::new(paths::ENVOY))
    }

    fn random(&self, len: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let n = rustix::rand::getrandom(
                &mut bytes[filled..],
                rustix::rand::GetRandomFlags::empty(),
            )?;
            filled += n;
        }
        Ok(bytes)
    }
}
