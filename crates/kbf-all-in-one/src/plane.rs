//! [`LocalDataPlane`]: a stand-in for the daemon's data plane as calls into the server
//! half of the same process. It stores nothing and reads nothing yet, so the data plane's
//! conformance checks over it fail (`tests/conformance.rs`).

use std::sync::Arc;

use kbf_proto::reapi::Digest;
use kbf_server::LocalStore;
use kbf_transfer::{Cas, CasError, label};
use kbf_types::LeaseId;
use tokio::runtime::Handle;

/// The daemon's data plane as calls into the server half of this process (a stand-in).
pub struct LocalDataPlane {
    _store: LocalStore,
    _server: Handle,
}

impl LocalDataPlane {
    /// The plane over the server's `store`, its calls run on the server half's runtime
    /// `server`.
    #[must_use]
    pub fn new(store: LocalStore, server: Handle) -> Self {
        Self {
            _store: store,
            _server: server,
        }
    }
}

impl Cas for LocalDataPlane {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        Err(CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let blob = format!("of {} bytes", bytes.len());
        Err(CasError::Unavailable(
            blob,
            "not stored: a stand-in".to_owned(),
        ))
    }
}
