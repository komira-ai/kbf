//! Stopping and starting the services a migration moves: `systemctl` on Linux,
//! `launchctl` on macOS, as steps through the host's [`Exec`] so a dry run asks the same
//! questions and a test records the commands.
//!
//! The launchd forms are the Mac kit's, the sequence that ran on the farm's Macs: a job is
//! stopped with `bootout` only when it is loaded (`launchctl print system/<label>`
//! succeeds), and started with `enable` and then `bootstrap`, and the start returns once
//! `print` shows the job running with a pid. This code has not run them on a Mac itself;
//! the rehearsal does that.
//!
//! The kit's readiness signal is the daemon's `welcomed` line in its error log after the
//! offset it had before the start. The installer cannot read that log generically (the
//! path is in each job's plist, and the old helper's job writes none), so the pid that
//! `print` shows is the signal here, the one the helper's confirm already reads
//! ([`main_pid`]). It says launchd started the process, not that the daemon is
//! serving; the checks after the migration say that.

use std::io;

use kbf_host::path::HostPath;
use kbf_host::{Cmd, Exec, Step};

use crate::helper::restart::Restart;
use crate::helper::service::main_pid;

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

/// How many times a started job is looked at, a second apart, before it is called not
/// running.
const READY_POLLS: u32 = 30;

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

    /// Stop `unit`. A launchd job that is not loaded is already stopped, and `bootout` of
    /// it is an error, so it is not asked.
    pub fn stop(&mut self, unit: &Unit) -> io::Result<()> {
        let cmd = match unit.kind() {
            Kind::Systemd => systemctl(&["stop", "--", unit.as_str()]),
            Kind::Launchd => {
                if !self.is_active(unit)? {
                    return Ok(());
                }
                launchctl(&["bootout", &Self::domain(unit)])
            }
        };
        self.exec.apply(&Step::Run(cmd))
    }

    /// Start `unit`. A launchd job is enabled (an earlier `disable` would make `bootstrap`
    /// refuse it), bootstrapped, and waited for until it runs.
    pub fn start(&mut self, unit: &Unit) -> io::Result<()> {
        match unit.kind() {
            Kind::Systemd => {
                self.exec
                    .apply(&Step::Run(systemctl(&["start", "--", unit.as_str()])))
            }
            Kind::Launchd => {
                self.enable(unit)?;
                let plist = format!("{}/{unit}", self.roots.unit_dir);
                self.exec
                    .apply(&Step::Run(launchctl(&["bootstrap", "system", &plist])))?;
                self.wait_running(unit)
            }
        }
    }

    /// Look at the job up to [`READY_POLLS`] times, a second apart (the pause is a `sleep`
    /// step, so it goes through the host like every other command), until `print` shows
    /// a pid.
    fn wait_running(&mut self, unit: &Unit) -> io::Result<()> {
        let job = Restart::Launchd(unit.stem().to_owned());
        let mut last = String::new();
        for poll in 0..READY_POLLS {
            if poll > 0 {
                let pause = Cmd::new(program("/bin/sleep")).arg("1");
                self.exec.apply(&Step::Run(pause))?;
            }
            match main_pid(&*self.exec, &job) {
                Ok(_) => return Ok(()),
                Err(why) => last = why,
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{unit} was bootstrapped and is not running after {READY_POLLS} s: {last}"),
        ))
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
