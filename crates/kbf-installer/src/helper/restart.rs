//! How the helper restarts the one service it was configured for. The unit is the helper's own
//! flag (root's), never anything an order carries.

use kbf_host::{Cmd, HostPath, Step};

use crate::cli::Mode;
use crate::layout::Layout;
use crate::setup::plan::mode_units;

/// The file an operator verb leaves while a restart it needs is not done: root-owned, in
/// the state directory. It is written before the links move, so that a verb killed
/// between the links and its own restart leaves the restart to the service's next pass.
pub fn owed_marker(host: &Layout) -> HostPath {
    HostPath::new(&format!("{}/restart.owed", host.state_dir().as_str()))
        .expect("the state directory and a plain name")
}

/// What to run to restart the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restart {
    /// `systemctl restart <unit>`.
    Systemd(String),
}

impl Restart {
    /// The step that restarts the service.
    pub fn step(&self) -> Step {
        let Restart::Systemd(unit) = self;
        let program = HostPath::new("/usr/bin/systemctl").expect("an absolute path");
        let cmd = ["restart", "--", unit.as_str()]
            .into_iter()
            .fold(Cmd::new(program), Cmd::arg);
        Step::Run(cmd)
    }
}

/// The unit `restart` restarts, when it is a frontend's (`kbf --mode=frontend`: the family of
/// the unit `setup` names for that mode). A node `activate` would swap and restart it, and a
/// frontend moves by `frontend_update`; the helper uses this to refuse that.
pub fn frontend_unit(restart: &Restart) -> Option<&str> {
    let Restart::Systemd(unit) = restart;
    let family = family(unit)?;
    mode_units(Mode::Frontend)
        .iter()
        .any(|u| self::family(u) == Some(family))
        .then_some(unit.as_str())
}

/// The family of a unit file name: `kbf-frontend` for `kbf-frontend.service` and for every
/// instance of its template, `kbf-frontend@<instance>.service`. A name that is neither, or
/// whose instance is empty or holds more than a unit instance does (a `/` would leave the
/// slice), has none.
fn family(unit: &str) -> Option<&str> {
    let name = unit.strip_suffix(".service")?;
    match name.split_once('@') {
        None => Some(name),
        Some((stem, instance)) => (!instance.is_empty()
            && instance
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)))
        .then_some(stem),
    }
}

#[cfg(test)]
#[path = "restart_tests.rs"]
mod tests;
