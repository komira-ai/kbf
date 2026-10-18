//! The probe a node runs on a registered key pair before it opens its cache on it: a
//! write, a read-back and a delete of one object under `.kbf-probe/`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::probe::{PROBE_DIR, ProbeStep, probe};
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

const ROOT: &str = "kbf/";

fn root() -> KeyPrefix {
    KeyPrefix::new(ROOT).unwrap()
}

/// A store on the fake signing with this key pair.
fn store(fake: &FakeS3, id: &str, secret: &str) -> S3Store {
    S3Store::new(S3Config {
        endpoint: fake.endpoint().to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "kbf-fake".to_owned(),
        credentials: Credentials::new(id, secret),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(10),
        min_body_rate: None,
    })
    .unwrap()
}

/// A bucket that holds the fake's key pair.
async fn guarded() -> FakeS3 {
    let fake = FakeS3::start().await;
    fake.require_credentials("AKIAOK", "right-secret");
    fake
}

/// Catches a probe that leaves its object behind (mutant: the delete is skipped) and
/// one that writes anywhere but under `.kbf-probe/` (mutant: the key drops `PROBE_DIR`).
#[tokio::test]
async fn a_working_key_pair_passes_and_leaves_nothing() {
    let fake = guarded().await;
    probe(&store(&fake, "AKIAOK", "right-secret"), &root(), "n-1")
        .await
        .expect("a probe on the right keys");
    assert_eq!(
        fake.keys(),
        Vec::<String>::new(),
        "the probe object is gone"
    );
    let key = format!("/kbf-fake/{ROOT}{PROBE_DIR}n-1?");
    assert_eq!(
        fake.requests(),
        [
            format!("PUT {key}"),
            format!("GET {key}"),
            format!("DELETE {key}")
        ],
        "one write, one read, one delete, all under the probe directory"
    );
}

/// Catches a probe that treats a refused write as success (mutant: the put error is
/// dropped) and a fake that accepts any signature (mutant: `signed_with` returns true):
/// a wrong secret and a wrong key id both fail at the write and write nothing.
#[tokio::test]
async fn a_wrong_key_pair_fails_at_the_write() {
    let fake = guarded().await;
    for (id, secret) in [("AKIAOK", "wrong-secret"), ("AKIAXX", "right-secret")] {
        let failed = probe(&store(&fake, id, secret), &root(), "n-2").await;
        assert_eq!(failed, Err(ProbeStep::Put), "{id}");
    }
    assert!(fake.keys().is_empty());
}

/// A store over memory that misbehaves on request.
struct Tampered {
    inner: MemoryStore,
    wrong_bytes: AtomicBool,
    fail_get: AtomicBool,
    fail_delete: AtomicBool,
}

impl Tampered {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(Capabilities::default()),
            wrong_bytes: AtomicBool::new(false),
            fail_get: AtomicBool::new(false),
            fail_delete: AtomicBool::new(false),
        }
    }

    async fn held(&self) -> usize {
        let all = KeyPrefix::new("").unwrap();
        let page = self
            .inner
            .list(&all, None, PageSize::new(100).unwrap())
            .await;
        page.unwrap().objects.len()
    }
}

fn refused() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 500,
        code: "InternalError".to_owned(),
        message: "SENTINEL-store-message".to_owned(),
    }
}

impl ObjectStore for Tampered {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        if self.fail_get.load(Ordering::SeqCst) {
            return Err(refused());
        }
        let bytes = self.inner.get_range(key, range).await?;
        Ok(if self.wrong_bytes.load(Ordering::SeqCst) {
            Bytes::from_static(b"other-data")
        } else {
            bytes
        })
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(refused());
        }
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

/// Catches a probe that trusts a read that returns other bytes than were written
/// (mutant: the comparison is dropped), and one that gives up on a failed read without
/// deleting (mutant: `?` on the read before the delete).
#[tokio::test]
async fn a_read_that_fails_or_differs_fails_the_probe_and_still_cleans_up() {
    for (wrong, fails) in [(true, false), (false, true)] {
        let store = Tampered::new();
        store.wrong_bytes.store(wrong, Ordering::SeqCst);
        store.fail_get.store(fails, Ordering::SeqCst);
        assert_eq!(
            probe(&store, &root(), "n-3").await,
            Err(ProbeStep::Get),
            "wrong bytes {wrong}, failing read {fails}"
        );
        assert_eq!(store.held().await, 0, "the object was deleted anyway");
    }
}

/// Catches a probe that reports success while its object stays (mutant: the delete's
/// result is ignored): the step names the delete.
#[tokio::test]
async fn a_delete_that_fails_fails_the_probe() {
    let store = Tampered::new();
    store.fail_delete.store(true, Ordering::SeqCst);
    assert_eq!(probe(&store, &root(), "n-4").await, Err(ProbeStep::Delete));
    assert_eq!(
        store.held().await,
        1,
        "the object stays and the probe says so"
    );
}

/// Catches a nonce that reaches the key unchecked (mutant: `unwrap` the key): a nonce
/// that is not a key gives the `Key` step, and nothing is written.
#[tokio::test]
async fn a_nonce_that_is_not_a_key_is_refused_before_any_write() {
    let store = Tampered::new();
    assert_eq!(
        probe(&store, &root(), "a b/../c").await,
        Err(ProbeStep::Key)
    );
    assert_eq!(store.held().await, 0);
}

/// The text of each step names the step and no more.
#[test]
fn each_step_has_its_own_text() {
    let texts: Vec<String> = [
        ProbeStep::Key,
        ProbeStep::Put,
        ProbeStep::Get,
        ProbeStep::Delete,
    ]
    .map(|step| step.to_string())
    .into();
    let mut sorted = texts.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 4, "{texts:?}");
}
