//! Keeping the worker listener's copy of the `deny/` rows fresh ([`crate::identity`]'s
//! "store's rows"), apart from everything else the membership does with the store.
//!
//! The copy is good for [`crate::identity::DENY_SNAPSHOT_SWEEPS`] beats after the read that
//! made it began; after that every check of the worker listener is refused UNAVAILABLE. Three
//! things keep it younger than that:
//!
//! - **A read every beat of its own** ([`refreshing`], beside the sweeper's map refresh and
//!   reap in [`crate::serve::sweeping`]). It reads the rows and nothing else
//!   ([`Members::refresh_denied`]). The sweeper's map refresh and reap are writes, and every
//!   write of the state store queues on one commit head that all the frontends of the store
//!   share, so a long write of another frontend (one that is starting) holds them up; were the
//!   rows read only between them, every frontend of the store would refuse every daemon while
//!   one of them starts.
//! - **A read just before the listeners serve** ([`denying`]). The read the start makes when
//!   it opens the members (`crate::serve::members_of`) comes before the farm's state is
//!   restored, which can take longer than the copy is good for.
//! - **Readiness** ([`probing`]): `/readyz` and `READY=1` fail the `deny_rows` check while the
//!   copy is unusable, so a frontend that cannot admit anybody does not say it is ready.

use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;

use crate::health::{Checks, Probe};
use crate::identity::{DeniedRows, Peers};
use crate::members::Members;
use crate::members_rpc::blocking;

/// The name of the readiness check ([`probing`]).
pub(crate) const CHECK: &str = "deny_rows";

/// The membership sweeper of `members` ([`crate::members_rpc::sweep`]) and, beside it, the
/// read of their deny rows every beat ([`refreshing`]): one future, so whatever ends the sweeper
/// (a retire or a drain standing down, as it stops the worker listener) ends both.
pub(crate) fn beside_sweep(members: Arc<Members>) -> BoxFuture<'static, ()> {
    futures::future::join(
        crate::members_rpc::sweep(Arc::clone(&members)),
        refreshing(members),
    )
    .map(drop)
    .boxed()
}

/// Reads `members`' deny rows every beat, starting at once. Never completes; a failure is
/// logged and tried again at the next beat (the earlier copy ages meanwhile).
pub(crate) async fn refreshing(members: Arc<Members>) {
    let mut tick = tokio::time::interval(members.config().beat());
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let reading = Arc::clone(&members);
        if let Err(e) = blocking(move || reading.refresh_denied()).await {
            tracing::warn!("deny rows refresh: {}", e.message());
        }
    }
}

/// `peers`, also refusing what the `deny/` rows of `members` name ([`Peers::with_denied_rows`]),
/// with the rows read now: the start calls it just before its listeners serve. A read that
/// fails is logged, and the start goes on: the rule of the module docs applies.
pub(crate) fn denying(peers: Peers, members: Option<&Arc<Members>>) -> Peers {
    if let Some(members) = members
        && let Err(error) = members.refresh_denied()
    {
        tracing::error!(
            %error,
            "reading the deny rows before serving failed: the worker listener refuses every \
             call until a refresh reads them"
        );
    }
    peers.with_denied_rows(members.cloned())
}

/// `checks`, and the `deny_rows` check when `peers` check callers against the `deny/` rows.
pub(crate) fn probing(checks: Checks, peers: &Peers) -> Checks {
    match peers.denied_rows() {
        Some(rows) => checks.probing(CHECK, Arc::new(Fresh(rows.clone()))),
        None => checks,
    }
}

/// The `deny_rows` check: the copy is one a check may use. Reads no store.
struct Fresh(DeniedRows);

impl Probe for Fresh {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        let answer =
            self.0.usable().map(drop).map_err(|why| {
                format!("the worker listener refuses every call: the deny rows: {why}")
            });
        Box::pin(std::future::ready(answer))
    }
}

#[cfg(test)]
mod tests;
