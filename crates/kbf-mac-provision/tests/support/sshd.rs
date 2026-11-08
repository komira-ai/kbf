//! The fake Mac's host keys, and programs that fail as a real one does: an exit code
//! with what it said on standard error, a signal, or a program that cannot be started.

use std::fs;
use std::io;

use kbf_mac_provision::host::Output;

use super::FakeMac;

/// How a faulted call ends.
#[derive(Clone, Debug)]
pub enum Fault {
    /// Exits with this code, having said this on standard error.
    Exit(i32, String),
    /// Ended by a signal, having said this on standard error.
    Signal(String),
    /// Cannot be started.
    Missing,
}

impl FakeMac {
    /// Makes the host key `ssh-keygen -A` makes on a Mac that has none.
    pub fn host_keys(&self) {
        self.write("/etc/ssh/ssh_host_ed25519_key", "fake host key\n");
    }

    /// How the call `name args` ends when a fault of the state names it; `None` when
    /// none does.
    pub(super) fn fault(&self, name: &str, args: &[&str]) -> Option<io::Result<Output>> {
        let call = format!("{name} {}", args.join(" "));
        let state = self.state.borrow();
        let (_, fault) = state
            .faults
            .iter()
            .find(|(p, _)| call.starts_with(p.as_str()))?;
        Some(match fault {
            Fault::Exit(code, said) => Ok(Output {
                code: Some(*code),
                stderr: said.clone(),
                ..Output::default()
            }),
            Fault::Signal(said) => Ok(Output {
                stderr: said.clone(),
                ..Output::default()
            }),
            Fault::Missing => Err(io::Error::from(io::ErrorKind::NotFound)),
        })
    }

    /// Whether the fake has a host key file.
    pub fn has_host_key(&self) -> bool {
        self.path("/etc/ssh/ssh_host_ed25519_key").exists()
    }

    /// Removes the host keys, as on a Mac that never ran sshd.
    pub fn remove_host_keys(&self) {
        fs::remove_file(self.path("/etc/ssh/ssh_host_ed25519_key")).unwrap();
    }
}
