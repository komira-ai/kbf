//! Telling systemd the frontend serves, once it does.

use std::ffi::OsString;

use super::notify;

/// What a frontend does once it serves: send `READY=1` to the service manager.
///
/// `READY=1` says the frontend is serving (its listeners are bound and its state store is
/// open), not that the farm is healthy: a storage node only gets its map after it registers
/// with a serving frontend, so waiting for `/readyz` here would deadlock a fresh farm.
/// `/readyz` keeps reporting the storage, the state store and the map for load balancers.
pub(crate) struct Ready {
    /// `$NOTIFY_SOCKET`, if the manager set it.
    pub notify: Option<OsString>,
}

impl Ready {
    pub async fn run(self) {
        tracing::info!("the frontend serves");
        let sent = self.notify.as_deref().map(notify::ready);
        if let Some(Err(error)) = sent {
            tracing::warn!(%error, "the service manager could not be told the frontend serves");
        }
    }
}

#[cfg(test)]
mod tests;
