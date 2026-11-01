//! How long the roll waits for each thing: [`Timing`].

use std::time::Duration;

use crate::inventory::Role;

/// How long the roll waits for each thing. The defaults are the package-update design's
/// (node health 300 s, daemon 780 s, no answer 120 s); the frontend's bound, the safe-to-stop
/// bound and the cap on an install in progress are this crate's choices.
#[derive(Clone, Debug)]
pub struct Timing {
    /// Between two asks of the same port.
    pub poll: Duration,
    /// How long a host may stay unsafe to stop (or safe-to-stop unanswered) before the roll
    /// fails at it.
    pub safe_bound: Duration,
    /// How long a restart waits for a host that answers "install in progress".
    pub install_bound: Duration,
    /// How long a restart waits for a host that does not answer a query at all.
    pub query_bound: Duration,
    pub healthy_frontend: Duration,
    pub healthy_storage: Duration,
    pub healthy_daemon: Duration,
    /// How long a daemon host's node is waited for to finish its leases once it is drained.
    /// `None`, the default, waits however long they take. When a set time is out the host is
    /// not installed: the roll stops with the node left drained.
    pub drain_deadline: Option<Duration>,
    /// Between two asks of a Mac host how its apply stands.
    pub apply_poll: Duration,
    /// How long a Mac host's apply may take, from the answer `apply_started` (or from a
    /// restart that resumes the wait), to be `done` for the target.
    pub apply_deadline: Duration,
    /// For a frontend member rolled without a proxy: how long every other member must answer
    /// `/readyz` 200 without a miss before the member is drained. The wait for that window,
    /// which a miss restarts, is given this and [`Timing::healthy_frontend`] on top.
    pub peer_ready: Duration,
    /// For a frontend member rolled without a proxy: how long the member is waited for, from the
    /// start of its own drain, to exit. `None`, the default, waits until it has exited however
    /// long that takes. When a set time is out the roll fails and the member is left draining.
    pub frontend_drain: Option<Duration>,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            poll: Duration::from_secs(5),
            safe_bound: Duration::from_secs(300),
            install_bound: Duration::from_secs(900),
            query_bound: Duration::from_secs(120),
            healthy_frontend: Duration::from_secs(300),
            healthy_storage: Duration::from_secs(300),
            healthy_daemon: Duration::from_secs(780),
            drain_deadline: None,
            apply_poll: Duration::from_secs(5),
            apply_deadline: Duration::from_secs(600),
            peer_ready: Duration::from_secs(30),
            frontend_drain: None,
        }
    }
}

impl Timing {
    pub(crate) fn healthy(&self, role: Role) -> Duration {
        match role {
            Role::Frontend => self.healthy_frontend,
            Role::Storage => self.healthy_storage,
            Role::Daemon | Role::MacDaemon => self.healthy_daemon,
        }
    }
}
