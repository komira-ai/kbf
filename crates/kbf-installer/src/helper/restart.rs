//! How the helper restarts the one service it was configured for. The unit or label is
//! the helper's own flag (root's), never anything an order carries.

use kbf_host::{Cmd, HostPath, Step};

use crate::layout::Layout;

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
    /// `launchctl kickstart -k system/<label>`.
    Launchd(String),
}

impl Restart {
    /// The step that restarts the service.
    pub fn step(&self) -> Step {
        let (program, args): (&str, Vec<String>) = match self {
            Restart::Systemd(unit) => (
                "/usr/bin/systemctl",
                vec!["restart".to_owned(), "--".to_owned(), unit.clone()],
            ),
            Restart::Launchd(label) => (
                "/bin/launchctl",
                vec![
                    "kickstart".to_owned(),
                    "-k".to_owned(),
                    format!("system/{label}"),
                ],
            ),
        };
        let program = HostPath::new(program).expect("an absolute path");
        let cmd = args.into_iter().fold(Cmd::new(program), Cmd::arg);
        Step::Run(cmd)
    }
}
