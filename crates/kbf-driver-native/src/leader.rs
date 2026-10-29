//! The action's leader process, reaped with `wait4` so that its resource usage is kept:
//! the CPU of the leader and of every descendant it reaped ([`crate::usage::Meter::reaped`]).
//!
//! This is the stub of the red-first commit: it reaps the child and reports no usage.

use std::io;
use std::process::{Child, ExitStatus};

/// What reaping the leader gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reaped {
    /// The leader's exit status.
    pub status: ExitStatus,
    /// User CPU time of the leader and the descendants it reaped, in microseconds.
    pub user_micros: u64,
    /// System CPU time, likewise.
    pub system_micros: u64,
}

/// The leader of one action, owned from its spawn to its reap.
#[derive(Debug)]
pub struct Leader {
    child: Child,
}

impl Leader {
    /// Takes the spawned `child`.
    #[must_use]
    pub fn new(child: Child) -> Self {
        Self { child }
    }

    /// The leader's pid.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits for the leader to exit and reaps it.
    ///
    /// # Errors
    /// The wait failed.
    pub async fn wait(&mut self) -> io::Result<Reaped> {
        let status = self.child.wait()?;
        Ok(Reaped {
            status,
            user_micros: 0,
            system_micros: 0,
        })
    }
}
