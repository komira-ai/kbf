//! Telling systemd the frontend serves, once it does.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use crate::health::Checks;
use crate::notify;

/// How often the probes are asked again while one fails.
const LOOK: Duration = Duration::from_millis(100);

/// The probe `READY=1` does not wait for. At a cold power-up the `--storage` node may come
/// up after the frontend; a frontend that waited would be killed at `TimeoutStartSec` and
/// restart with a new epoch, in a loop. `/readyz` still fails on it (Envoy sends nothing
/// to a frontend whose storage is away, and the frontend reports it in `/readyz`'s
/// detail, health and logs), but the process is up, so the manager is told.
const NOT_AWAITED: &[&str] = &["storage"];

/// What a frontend does once it serves: wait for its other probes (the state store, the
/// map) to pass, not the storage node ([`NOT_AWAITED`]), then send `READY=1` to the service
/// manager.
pub(crate) struct Ready {
    pub checks: Arc<Checks>,
    /// `$NOTIFY_SOCKET`, if the manager set it.
    pub notify: Option<OsString>,
}

impl Ready {
    pub async fn run(self) {
        while !self.checks.failing_except(NOT_AWAITED).await.is_empty() {
            tokio::time::sleep(LOOK).await;
        }
        tracing::info!("the frontend is ready");
        let sent = self.notify.as_deref().map(notify::ready);
        if let Some(Err(error)) = sent {
            tracing::warn!(%error, "the service manager could not be told the frontend is ready");
        }
    }
}

#[cfg(test)]
mod tests;
