//! The gateway's soft state: which servers registered lately, and which of them is the
//! leader Envoy should send REAPI traffic to.
//!
//! An entry lives for the TTL after the call that last renewed it. The state is rebuilt
//! from the servers' calls after a gateway restart, so nothing is stored. Time is read
//! through a [`Clock`], so a test moves it by hand instead of waiting.
//!
//! **The leader** is, among the live entries that claim the role, the one with the
//! highest term. Two live claims at the same term are a tie (a split brain the servers
//! should not produce): the lowest node name wins, and the tie is logged once when it
//! appears or changes.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// Where the gateway reads the time. [`Clock::system`] in the binary; tests build one
/// from a closure over a value they move.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl Clock {
    /// The monotonic system clock.
    #[must_use]
    pub fn system() -> Self {
        Self(Arc::new(Instant::now))
    }

    /// A clock that reads `now`.
    pub fn from_fn(now: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    fn now(&self) -> Instant {
        (self.0)()
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Clock")
    }
}

/// What one call of a server says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    /// The server's node name: the one DNS name of its certificate.
    pub name: String,
    /// Where its REAPI listener is.
    pub address: SocketAddr,
    /// Whether it holds the scheduler role.
    pub is_leader: bool,
    /// Its scheduler term.
    pub term: u64,
}

/// The server Envoy sends REAPI traffic to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leader {
    /// The node name.
    pub name: String,
    /// Where its REAPI listener is.
    pub address: SocketAddr,
    /// The term it claimed.
    pub term: u64,
}

#[derive(Debug)]
struct Entry {
    claim: Claim,
    seen: Instant,
}

#[derive(Debug, Default)]
struct State {
    entries: BTreeMap<String, Entry>,
    /// The names tied for the leader at the last decision (empty: no tie), so a tie is
    /// logged when it appears and not at every call.
    tied: Vec<String>,
}

/// The registered servers and the leader they elect, announced to subscribers.
#[derive(Debug)]
pub struct Registry {
    ttl: Duration,
    clock: Clock,
    state: Mutex<State>,
    leader: watch::Sender<Option<Leader>>,
}

impl Registry {
    /// An empty registry whose entries live for `ttl` after their last call.
    #[must_use]
    pub fn new(ttl: Duration, clock: Clock) -> Self {
        Self {
            ttl,
            clock,
            state: Mutex::new(State::default()),
            leader: watch::Sender::new(None),
        }
    }

    /// How long an entry lives without another call.
    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Records or renews a server's entry, and announces a changed leader.
    pub fn register(&self, claim: Claim) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let seen = self.clock.now();
        state
            .entries
            .insert(claim.name.clone(), Entry { claim, seen });
        self.decide(&mut state, seen);
    }

    /// Forgets the entries whose TTL has run out, and announces a changed leader. The
    /// binary calls it on a timer; a test calls it after moving its clock.
    pub fn sweep(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = self.clock.now();
        let ttl = self.ttl;
        state
            .entries
            .retain(|_, entry| now.saturating_duration_since(entry.seen) < ttl);
        self.decide(&mut state, now);
    }

    /// The current leader, and every later change of it.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Option<Leader>> {
        self.leader.subscribe()
    }

    /// The names of the live entries, in order.
    #[must_use]
    pub fn servers(&self) -> Vec<String> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.entries.keys().cloned().collect()
    }

    fn decide(&self, state: &mut State, now: Instant) {
        let ttl = self.ttl;
        let live = state
            .entries
            .values()
            .filter(|e| now.saturating_duration_since(e.seen) < ttl)
            .map(|e| &e.claim);
        let (leader, tied) = elect(live);
        if !tied.is_empty() && tied != state.tied {
            tracing::warn!(
                tied = ?tied,
                "several servers claim the leader role at one term; the lowest name wins"
            );
        }
        state.tied = tied;
        self.leader.send_if_modified(|current| {
            if *current == leader {
                return false;
            }
            match &leader {
                Some(l) => tracing::info!(
                    leader = %l.name, address = %l.address, term = l.term, "leader changed"
                ),
                None => tracing::warn!("no live server claims the leader role"),
            }
            *current = leader;
            true
        });
    }
}

/// The leader among `claims` (live ones, in name order) and the names tied with it (all
/// the claimants at its term, the leader first; empty when it has no rival).
fn elect<'a>(claims: impl Iterator<Item = &'a Claim>) -> (Option<Leader>, Vec<String>) {
    let claimants: Vec<&Claim> = claims.filter(|c| c.is_leader).collect();
    let Some(top) = claimants.iter().map(|c| c.term).max() else {
        return (None, Vec::new());
    };
    let at_top: Vec<&&Claim> = claimants.iter().filter(|c| c.term == top).collect();
    let first = at_top[0];
    let leader = Leader {
        name: first.name.clone(),
        address: first.address,
        term: first.term,
    };
    let tied = if at_top.len() > 1 {
        at_top.iter().map(|c| c.name.clone()).collect()
    } else {
        Vec::new()
    };
    (Some(leader), tied)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
