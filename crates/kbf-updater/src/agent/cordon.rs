//! The cordon the updater made, remembered across retries and restarts.
//!
//! STUB: passes every call through; the mark is not written yet.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::inventory::Host;
use crate::ports::{Drain, Placed, PortError};

/// A [`Drain`] that remembers, on disk, the nodes it cordoned itself.
pub struct OwnCordon {
    inner: Arc<dyn Drain>,
    #[allow(dead_code)]
    dir: PathBuf,
}

impl OwnCordon {
    pub fn new(inner: Arc<dyn Drain>, dir: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            dir: dir.into(),
        }
    }
}

impl Drain for OwnCordon {
    fn placed(&self, host: &Host) -> Result<Placed, PortError> {
        self.inner.placed(host)
    }

    fn drain(&self, host: &Host, deadline: Option<Duration>) -> Result<(), PortError> {
        self.inner.drain(host, deadline)
    }

    fn leases(&self, host: &Host) -> Result<usize, PortError> {
        self.inner.leases(host)
    }

    fn undrain(&self, host: &Host) -> Result<(), PortError> {
        self.inner.undrain(host)
    }
}
