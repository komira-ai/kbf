use std::time::Duration;

use bytes::Bytes;
use kbf_front::store_metrics::Op;
use kbf_objstore::{Capabilities, MemoryStore, ObjectKey, ObjectStore as _};
use tonic::transport::Endpoint;

use super::*;
use crate::cold_apply::Fixed;
use crate::join::{Identity, RETRY_BASE};

fn key() -> ObjectKey {
    ObjectKey::new("a/b").expect("a key")
}

fn pick() -> (CountedPick<Fixed<MemoryStore>>, Arc<StoreMetrics>) {
    let metrics = Arc::new(StoreMetrics::cold());
    let store = Fixed(MemoryStore::new(Capabilities::default()));
    (CountedPick(store, Arc::clone(&metrics)), metrics)
}

/// Catches: a node on the store its flags name whose requests are not counted (the store is
/// handed to the cache unwrapped): a put through the picked store is one put and its bytes.
#[tokio::test]
async fn the_flags_store_is_counted() {
    let (pick, metrics) = pick();
    let store = pick.alone().expect("the store");
    store
        .put_new(&key(), Bytes::from_static(b"four"), None)
        .await
        .expect("put");
    assert_eq!(metrics.requests(Op::Put), 1);
    assert_eq!(metrics.write_bytes(), 4);
}

/// Catches: a registered node whose store is not counted, and a registration's report
/// changed by the wrapper: the store picked after registering counts a put, and what the
/// wrapped pick reports is what it reports unwrapped (`Fixed` is never a registration).
#[tokio::test]
async fn the_registered_store_is_counted_and_the_report_is_the_picks() {
    let channel = Endpoint::from_static("http://frontend.invalid:8981").connect_lazy();
    let who = Identity {
        instance_id: "node".to_owned(),
        addr: "192.0.2.1:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let joiner = Joiner::new(channel, who, RETRY_BASE);
    let joined = Joined {
        member_id: 1,
        boot: 1,
        interval: Duration::from_secs(1),
        generation: 1,
        objstore_generation: 0,
    };
    let (pick, metrics) = pick();
    let (store, applied) = pick.registered(&joiner, &joined).await.expect("picked");
    assert_eq!(applied, Applied::nothing());
    store
        .put_new(&key(), Bytes::from_static(b"three"), None)
        .await
        .expect("put");
    assert_eq!(metrics.requests(Op::Put), 1);
    assert_eq!(metrics.write_bytes(), 5);
}
