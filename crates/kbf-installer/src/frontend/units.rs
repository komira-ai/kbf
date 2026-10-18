//! The frontend's two systemd units, `kbf-frontend@a.service` and `@b`, as the installer
//! asks about and changes them: `systemctl show` to read the state, `start` and a
//! `--no-block` `stop` to change it. Every call goes through the host's [`Exec`], so a test
//! plays systemd and records what was asked. Unit names come from [`Instance::unit`], never
//! from a record.
//!
//! Nothing here kills a unit. A stop is asked with `--no-block` and watched on later passes:
//! the template's `TimeoutStopSec=infinity` lets a draining instance finish its streams, so
//! a blocking stop could hold the installer for as long as a build runs.

use std::io;

use kbf_host::{Cmd, Exec, HostPath, Step};

use crate::frontend::pending::Instance;

pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// The template unit file as the installer ships it.
pub const UNIT_FILE: &str = include_str!("../../units/kbf-frontend@.service");

/// What systemd says an instance is (`ActiveState`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitState {
    Active,
    /// `inactive` and `failed`: not running, and free to start.
    Stopped,
    /// `activating`, `reloading`, `deactivating`, or a word this does not know.
    Changing(String),
}

impl UnitState {
    fn of(word: &str) -> UnitState {
        match word.trim() {
            "active" => UnitState::Active,
            "inactive" | "failed" => UnitState::Stopped,
            other => UnitState::Changing(other.to_owned()),
        }
    }
}

fn systemctl(args: &[&str]) -> Cmd {
    let program = HostPath::new(SYSTEMCTL).expect("an absolute path");
    args.iter().fold(Cmd::new(program), |cmd, a| cmd.arg(*a))
}

/// What systemd says `instance` is now.
pub fn state<E: Exec>(exec: &E, instance: Instance) -> io::Result<UnitState> {
    let unit = instance.unit();
    let out = exec.query(&systemctl(&[
        "show",
        "--property=ActiveState",
        "--value",
        "--",
        &unit,
    ]))?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "systemctl show {unit} exited {}",
            out.status
        )));
    }
    Ok(UnitState::of(&String::from_utf8_lossy(&out.stdout)))
}

/// `systemctl start`: the unit is `Type=notify`, so this returns when the instance has said
/// READY, and fails when it could not (`TimeoutStartSec` bounds it).
pub fn start<E: Exec>(exec: &mut E, instance: Instance) -> io::Result<()> {
    exec.apply(&Step::Run(systemctl(&["start", "--", &instance.unit()])))
}

/// Ask systemd to stop `instance` and return at once.
pub fn stop<E: Exec>(exec: &mut E, instance: Instance) -> io::Result<()> {
    exec.apply(&Step::Run(systemctl(&[
        "stop",
        "--no-block",
        "--",
        &instance.unit(),
    ])))
}

#[cfg(test)]
#[path = "units_tests.rs"]
mod tests;
