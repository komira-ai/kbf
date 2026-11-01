//! What the daemon waits for once it drains ([`wait_for_drain`]).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, mpsc};
use tokio::time::sleep;

/// Waits until the drain is over: the daemon has nothing left to wait for (`drained`
/// was notified), or the operator sent a second stop signal on `signals`, which is the
/// hard stop and abandons the leases still running.
///
/// A drain never ends because time ran out. `deadline`, if set, only makes the wait
/// say so once, `deadline` after the call: a warning with the number of leases in
/// `running`, and the wait goes on. A closed `signals` channel is no signal.
pub(crate) async fn wait_for_drain(
    signals: &mut mpsc::UnboundedReceiver<()>,
    drained: &Notify,
    deadline: Option<Duration>,
    running: &AtomicUsize,
) {
    let overdue = async {
        if let Some(deadline) = deadline {
            sleep(deadline).await;
            tracing::warn!(
                ?deadline,
                running = running.load(Ordering::Relaxed),
                "drain deadline passed: leases are still running; waiting for them to end \
                 (a second signal stops at once and abandons them)"
            );
        }
        std::future::pending::<()>().await;
    };
    tokio::select! {
        Some(()) = signals.recv() => tracing::warn!("second signal: stopping at once"),
        () = drained.notified() => tracing::info!("drained: stopping"),
        () = overdue => {}
    }
}

#[cfg(test)]
mod tests;
