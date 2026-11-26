//! A manifest write over the frontend's pack store ([`StoreWrites`]): its wait for one of the
//! store's slots, which a pack upload holds, reaches the write's tally through the write in
//! flight, and is told to the preparation that made it. The time is tokio's paused clock, moved
//! only by the test.

use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Mutex;
use std::time::Duration;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream;
use kbf_objstore::{
    BodyStream, Capabilities, MemoryStore, ObjectKey, ObjectStore, ObjectStoreError,
};
use tokio::sync::oneshot;

use super::*;
use crate::store_wait::{Backoff, STORE_WAIT};
use crate::store_writes::StoreWrites;

/// The pack store as the tree writer sees it, as the frontend's packs put it.
struct Packed(Arc<StoreWrites<MemoryStore>>);

impl LargeStore for Packed {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        Box::pin(self.0.len(key))
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(self.0.put_stream(key, len, body))
    }
}

/// Catches the pack store's slot wait lost between the store and the tree's write (a tally that
/// does not follow the write in flight), or counted as its request: with the store's one slot
/// held by a pack upload whose body has not come, a manifest write waits for it; 7 ms later the
/// upload's body comes, and the write is told once, with 7 ms of store slot and nothing else.
#[tokio::test(start_paused = true)]
async fn a_manifest_write_is_told_its_wait_for_a_pack_store_slot() {
    let memory = MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    });
    let backoff = Backoff::new(STORE_WAIT, Arc::default());
    let store = Arc::new(StoreWrites::new(
        Arc::new(memory),
        NonZeroUsize::MIN,
        backoff,
    ));
    let (release, released) = oneshot::channel::<()>();
    let upload = tokio::spawn({
        let store = Arc::clone(&store);
        async move {
            let body = stream::once(async move {
                let _ = released.await;
                Ok(Bytes::from_static(b"abc"))
            });
            let key = ObjectKey::new("cas/f/w/p0").expect("a key");
            store.put_stream(&key, 3, body.boxed()).await
        }
    });
    // The upload takes the slot and waits for its body.
    tokio::task::yield_now().await;
    let large: Arc<dyn LargeStore> = Arc::new(Packed(store));
    let flights = Flights::new(4);
    let told = Mutex::new(Vec::new());
    let wrote = |waits| told.lock().expect("told").push(waits);
    let manifest = Bytes::from_static(b"a manifest");
    let digest = kbf_segments::sha256(&manifest);
    let retry = PutRetry::default();
    let mut write = pin!(flights.write(&large, &retry, digest, manifest, &wrote));
    assert!(
        futures::poll!(write.as_mut()).is_pending(),
        "no slot is free"
    );
    tokio::time::advance(Duration::from_millis(7)).await;
    release.send(()).expect("the upload waits");
    upload.await.expect("joined").expect("stored");
    write.await.expect("stored");
    let want = WriteWaits {
        store_slot: Duration::from_millis(7),
        ..WriteWaits::default()
    };
    assert_eq!(*told.lock().expect("told"), [want]);
}
