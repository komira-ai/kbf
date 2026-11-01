//! The real ports of the hook: the helper's socket, the frontend's own-node calls with the
//! host's updater certificate (or the opt-in operator token), and the time. The same wiring as
//! the agent's (`agent::build`), from the hook's flags.

use std::sync::Arc;

use super::flags::Flags;
use super::{HookWorld, WorldError};
use crate::agent::build::{
    frontend_ports_for, identity_of_files, node_ports_for, this_host, unix_now,
};
use crate::agent::local::{Limits, SocketLocal};
use crate::inventory::Role;
use crate::ports::{Clock, SystemClock};
use crate::wire::KBF_SERVICE;

/// The time a world reads: the clock its waits run on, and Unix seconds (the helper's deadline
/// is in them).
#[derive(Clone)]
pub struct Time {
    pub clock: Arc<dyn Clock>,
    pub unix_now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Time {
    /// The system's: what `kbf update-hook` runs on.
    #[must_use]
    pub fn system() -> Time {
        Time {
            clock: Arc::new(SystemClock::new()),
            unix_now: Arc::new(unix_now),
        }
    }
}

/// The world of this host, on the system's time.
///
/// # Errors
/// As [`world_at`].
pub fn world(flags: &Flags) -> Result<HookWorld, WorldError> {
    world_at(flags, Time::system())
}

/// The world of this host, on `time`.
///
/// # Errors
/// A certificate that is no updater's, a role the updater has no duties for, `--daemon-node`
/// given on a host that is not a frontend's, or a cordon directory that cannot be written.
pub fn world_at(flags: &Flags, time: Time) -> Result<HookWorld, WorldError> {
    let access = &flags.access;
    let identity = identity_of_files(&access.files).map_err(WorldError::Certificate)?;
    let host = this_host(&identity, access.daemon_node.as_deref()).map_err(WorldError::Host)?;
    let local = Arc::new(SocketLocal::new(&flags.helper_socket, Limits::default()));
    let (drain, health) = if host.role == Role::Frontend {
        frontend_ports_for(access, &identity, &local)
    } else {
        node_ports_for(access, &identity, host.role, KBF_SERVICE, &local)
    }
    .map_err(WorldError::Ports)?;
    Ok(HookWorld {
        host,
        local,
        drain,
        health,
        clock: time.clock,
        unix_now: time.unix_now,
    })
}
