//! The real ports of the hook: the helper's socket, the frontend's own-node calls with the
//! host's updater certificate (or the opt-in operator token), the system clock. The same
//! wiring as the agent's (`agent::build`), from the hook's flags.

use std::sync::Arc;

use super::HookWorld;
use super::flags::Flags;
use crate::agent::build::{
    frontend_ports_for, identity_of_files, node_ports_for, this_host, unix_now,
};
use crate::agent::local::{Limits, SocketLocal};
use crate::inventory::Role;
use crate::ports::SystemClock;
use crate::wire::KBF_SERVICE;

/// The world of this host.
///
/// # Errors
/// A certificate that is no updater's, a role the updater has no duties for, `--daemon-node`
/// given on a host that is not a frontend's, or a cordon directory that cannot be written.
pub fn world(flags: &Flags) -> Result<HookWorld, String> {
    let access = &flags.access;
    let identity = identity_of_files(&access.files)?;
    let host = this_host(&identity, access.daemon_node.as_deref())?;
    let local = Arc::new(SocketLocal::new(&flags.helper_socket, Limits::default()));
    let (drain, health) = if host.role == Role::Frontend {
        frontend_ports_for(access, &identity, &local)?
    } else {
        node_ports_for(access, &identity, host.role, KBF_SERVICE, &local)?
    };
    Ok(HookWorld {
        host,
        local,
        drain,
        health,
        clock: Arc::new(SystemClock::new()),
        unix_now: Arc::new(unix_now),
    })
}
