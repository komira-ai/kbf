//! What a storage node is told of the registered cold store (`cold_store_view`): the URI
//! of the store, from the non-secret rows, read at one generation. Each test names the
//! defect it catches.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Cond, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::object_store_api::BindingRole;
use prost::Message as _;

use super::{SENTINEL, as_store, bind, fake, name, open, put, scratch};
use crate::object_store::registry::cold_store_view;
use crate::object_store::rows::BindRow;
use crate::object_store::{ColdStoreView, Registry, RegistryError};

fn bound(store: &Arc<dyn Store>, label: &str) -> Registry {
    let reg = open(store, &scratch(label).join("s"));
    reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    reg
}

fn view(store: &Arc<dyn Store>) -> ColdStoreView {
    cold_store_view(&**store).expect("a view")
}

// Catches a view that invents a store when none is bound (a node would start on it), or
// reports the wrong generation for "nothing registered" (0) and "a connection only" (1).
#[test]
fn no_binding_is_no_uri_at_the_registry_generation() {
    let store = as_store(&fake());
    assert_eq!(
        view(&store),
        ColdStoreView {
            generation: 0,
            uri: None
        }
    );
    let reg = bound(&store, "view-none");
    assert_eq!(
        view(&store),
        ColdStoreView {
            generation: 1,
            uri: None
        }
    );
    reg.put_binding(BindingRole::Cold, &bind("cold", "farm")).unwrap();
    assert_eq!(view(&store).generation, 2);
    reg.delete_binding(BindingRole::Cold).unwrap();
    assert_eq!(view(&store).uri, None, "a binding taken back is no store");
}

// Catches a URI with a field of the connection that is not part of the store's identity
// and settings, in particular the access key id or the secret, and defaults not spelt
// the way the store URI parser reads them (an empty region, an empty prefix).
#[test]
fn the_uri_is_the_bucket_prefix_endpoint_and_settings_and_nothing_else() {
    let store = as_store(&fake());
    let reg = bound(&store, "view-uri");
    let mut binding = bind("cold", "farm-cold");
    binding.prefix = String::new();
    reg.put_binding(BindingRole::Cold, &binding).unwrap();
    let seen = view(&store);
    assert_eq!(
        seen.uri.as_deref(),
        Some("s3://farm-cold/kbf/?endpoint=http%3A%2F%2Fs3.example%3A9000&region=us-east-1"),
        "an empty prefix and region are the defaults the node's parser gives"
    );
    let text = seen.uri.expect("a uri");
    assert!(!text.contains("AKID") && !text.contains(SENTINEL), "{text}");
}

// Catches a prefix without its closing slash being compared by a node as another store,
// and `conditional-put` and the put rate being dropped (a node would overwrite keys on a
// store that refuses it, or time out slow puts at the wrong rate).
#[test]
fn the_registered_settings_are_in_the_uri() {
    let store = as_store(&fake());
    let reg = open(&store, &scratch("view-set").join("s"));
    let mut conn = put(SENTINEL);
    conn.region = Some("eu-1".to_owned());
    conn.conditional_put = Some(true);
    conn.min_put_rate = std::num::NonZeroU64::new(512);
    reg.put_connection(&name("cold"), &conn).unwrap();
    let mut binding = bind("cold", "b");
    binding.prefix = "p~q/r".to_owned();
    reg.put_binding(BindingRole::Cold, &binding).unwrap();
    assert_eq!(
        view(&store).uri.as_deref(),
        Some(
            "s3://b/p~q/r/?endpoint=http%3A%2F%2Fs3.example%3A9000&region=eu-1\
             &conditional-put=true&min-put-rate=512"
        )
    );
}

// Catches a row that does not read being told as "no store" or a half store: a binding
// whose connection is gone, a connection name that is not one, a bucket that does not
// survive a URI (and so would name another store when parsed).
#[test]
fn rows_that_do_not_make_a_store_are_corrupt_not_absent() {
    let store = as_store(&fake());
    let reg = bound(&store, "view-bad");
    reg.put_binding(BindingRole::Cold, &bind("cold", "b?x")).unwrap();
    let err = cold_store_view(&*store).unwrap_err();
    assert!(matches!(err, RegistryError::Corrupt("bind")), "{err}");

    let row = |connection: &str, bucket: &str| BindRow {
        connection: connection.to_owned(),
        bucket: bucket.to_owned(),
        prefix: String::new(),
        updated_generation: 1,
    };
    let write = |connection: &str| {
        let txn = Txn::new().put("objstore/bind/cold", row(connection, "b").encode_to_vec(), Cond::Any);
        store.txn(txn).unwrap().unwrap();
    };
    write("NOT A NAME");
    let err = cold_store_view(&*store).unwrap_err();
    assert!(matches!(err, RegistryError::Corrupt("bind")), "{err}");
    write("gone");
    let err = cold_store_view(&*store).unwrap_err();
    assert!(matches!(err, RegistryError::Corrupt("conn")), "{err}");
}

/// A store that, at each read of the binding, lets `writer` change the registry first, up
/// to `times`.
struct Changing {
    inner: Arc<dyn Store>,
    writer: Registry,
    times: usize,
    done: AtomicUsize,
}

impl Store for Changing {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        let before = self.inner.get(key)?;
        if key == "objstore/bind/cold" {
            let n = self.done.fetch_add(1, Ordering::SeqCst);
            if n < self.times {
                let mut conn = put(SENTINEL);
                conn.region = Some(format!("r{n}"));
                self.writer.put_connection(&name("cold"), &conn).unwrap();
            }
        }
        Ok(before)
    }
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }
    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }
    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

fn changing(label: &str, times: usize) -> Changing {
    let inner = as_store(&fake());
    let writer = bound(&inner, label);
    writer.put_binding(BindingRole::Cold, &bind("cold", "b")).unwrap();
    Changing {
        inner,
        writer,
        times,
        done: AtomicUsize::new(0),
    }
}

// Catches a view assembled from rows of two generations (a generation that names the
// old store beside the new store's URI): a write between the reads makes the view read
// again, and the answer is of the newer generation with the newer region.
#[test]
fn a_write_between_the_reads_is_read_again() {
    let store = changing("view-retry", 1);
    let seen = cold_store_view(&store).expect("a view");
    assert_eq!(seen.generation, 3, "connection, binding, then the region change");
    assert!(
        seen.uri.expect("a uri").contains("region=r0"),
        "the rows of the generation named"
    );
}

// Catches a view that loops for ever on a registry that keeps changing, or answers with a
// reading it knows is torn.
#[test]
fn a_registry_that_keeps_changing_is_a_conflict() {
    let store = changing("view-conflict", 99);
    let err = cold_store_view(&store).unwrap_err();
    assert!(matches!(err, RegistryError::Conflict), "{err}");
    assert_eq!(
        store.done.load(Ordering::SeqCst),
        3,
        "it read three times and gave up"
    );
}
