//! The uploads in flight in this process, by resource name, for `QueryWriteStatus`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// The uploads that are being received now and the bytes of each that have arrived.
#[derive(Debug, Default)]
pub(super) struct Uploads {
    next: AtomicU64,
    live: Mutex<HashMap<String, (u64, Arc<AtomicU64>)>>,
}

impl Uploads {
    /// An upload named `name` begins. It is in flight until the returned [`Upload`] is dropped,
    /// however the call ends. A second upload of one name replaces the first in the table (the
    /// later stream is the one a status asks about) and does not remove it when it ends first.
    pub(super) fn begin(self: &Arc<Self>, name: &str) -> Upload {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let received = Arc::new(AtomicU64::new(0));
        self.table()
            .insert(name.to_owned(), (id, Arc::clone(&received)));
        Upload {
            uploads: Arc::clone(self),
            name: name.to_owned(),
            id,
            received,
        }
    }

    /// The bytes that have arrived of the upload `name`, if one is in flight.
    pub(super) fn received(&self, name: &str) -> Option<u64> {
        let live = self.table();
        live.get(name)
            .map(|(_, bytes)| bytes.load(Ordering::Relaxed))
    }

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<String, (u64, Arc<AtomicU64>)>> {
        // A poisoned table is still a whole map: nobody writes half of one.
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An upload in flight.
pub(super) struct Upload {
    uploads: Arc<Uploads>,
    name: String,
    id: u64,
    received: Arc<AtomicU64>,
}

impl Upload {
    /// `bytes` more have arrived.
    pub(super) fn add(&self, bytes: u64) {
        self.received.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        let mut live = self.uploads.table();
        if live.get(&self.name).is_some_and(|(id, _)| *id == self.id) {
            live.remove(&self.name);
        }
    }
}
