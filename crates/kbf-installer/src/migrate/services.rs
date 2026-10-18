//! Stopping and starting the services a migration moves: `systemctl` on Linux,
//! `launchctl` on macOS, as steps through the host's [`Exec`] so a dry run asks the same
//! questions and a test records the commands.
//!
//! The launchd forms are UNVERIFIED on a Mac of the farm: `bootout`, `bootstrap`,
//! `print`, `enable` and `disable` are the documented `launchctl` verbs for a system
//! daemon.

use std::io;

use kbf_host::path::HostPath;
use kbf_host::{Cmd, Exec, Step};

use super::roots::Roots;
use super::units::{Kind, Unit};

/// The services of one host.
pub struct Services<'a, E: Exec> {
    pub exec: &'a mut E,
    pub roots: &'a Roots,
}

fn program(path: &str) -> HostPath {
    HostPath::new(path).expect("a constant host path")
}

fn systemctl(args: &[&str]) -> Cmd {
    args.iter()
        .fold(Cmd::new(program("/usr/bin/systemctl")), |cmd, a| {
            cmd.arg(*a)
        })
}

fn launchctl(args: &[&str]) -> Cmd {
    args.iter()
        .fold(Cmd::new(program("/bin/launchctl")), |cmd, a| cmd.arg(*a))
}

impl<E: Exec> Services<'_, E> {
    fn domain(unit: &Unit) -> String {
        format!("system/{}", unit.stem())
    }

    /// Whether `unit` is running now.
    pub fn is_active(&self, unit: &Unit) -> io::Result<bool> {
        let cmd = match unit.kind() {
            Kind::Systemd => systemctl(&["is-active", "--quiet", "--", unit.as_str()]),
            Kind::Launchd => launchctl(&["print", &Self::domain(unit)]),
        };
        Ok(self.exec.query(&cmd)?.status.success())
    }

    pub fn stop(&mut self, unit: &Unit) -> io::Result<()> {
        let cmd = match unit.kind() {
            Kind::Systemd => systemctl(&["stop", "--", unit.as_str()]),
            Kind::Launchd => launchctl(&["bootout", &Self::domain(unit)]),
        };
        self.exec.apply(&Step::Run(cmd))
    }

    pub fn start(&mut self, unit: &Unit) -> io::Result<()> {
        let cmd = match unit.kind() {
            Kind::Systemd => systemctl(&["start", "--", unit.as_str()]),
            Kind::Launchd => {
                let plist = format!("{}/{unit}", self.roots.unit_dir);
                launchctl(&["bootstrap", "system", &plist])
            }
        };
        self.exec.apply(&Step::Run(cmd))
    }

    /// Keep `unit` from starting again (the old helper's, once the installer manages the
    /// host).
    pub fn disable(&mut self, unit: &Unit) -> io::Result<()> {
        let cmd = match unit.kind() {
            Kind::Systemd => systemctl(&["disable", "--", unit.as_str()]),
            Kind::Launchd => launchctl(&["disable", &Self::domain(unit)]),
        };
        self.exec.apply(&Step::Run(cmd))
    }

    pub fn enable(&mut self, unit: &Unit) -> io::Result<()> {
        let cmd = match unit.kind() {
            Kind::Systemd => systemctl(&["enable", "--", unit.as_str()]),
            Kind::Launchd => launchctl(&["enable", &Self::domain(unit)]),
        };
        self.exec.apply(&Step::Run(cmd))
    }

    /// Make the unit manager read the unit files again (a no-op for launchd, which reads a
    /// plist when it is bootstrapped).
    pub fn reload(&mut self) -> io::Result<()> {
        match self.roots.platform {
            crate::layout::Platform::Linux => {
                self.exec.apply(&Step::Run(systemctl(&["daemon-reload"])))
            }
            crate::layout::Platform::MacOs => Ok(()),
        }
    }
}
