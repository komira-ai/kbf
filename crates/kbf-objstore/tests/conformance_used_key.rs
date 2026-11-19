//! The used-key check must fail a store that lets a key hold other bytes than the first.
//!
//! Each store here is the in-memory fake with conditional writes and one behaviour
//! broken. Each test checks that the suite reports `UsedKeyKeepsItsBytes` as failed
//! (and, where it says so, no other check), and the reason it gives.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::SystemTime;

use bytes::Bytes;
use futures::executor::block_on;
use kbf_objstore::conformance::{self, Check, Outcome, Report};
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey,
    ObjectStore, ObjectStoreError, PageSize,
};

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

#[derive(Clone, Copy, PartialEq)]
enum Defect {
    /// Refuses an overwrite only when the new body is as long as the stored one, and
    /// otherwise replaces the object.
    RefusesOnlySameLength,
    /// Checks that the key is free, gives other writers a turn, then writes over whatever
    /// is there: every writer racing for a fresh key is answered Ok and the last body
    /// stays.
    ChecksThenWrites,
    /// Answers a second put onto the first key with a server error.
    ErrorsOnARetry,
    /// Answers every put onto the racing key with a server error.
    FailsARace,
    /// Answers a second put onto the first key with `AlreadyExists` and replaces the
    /// bytes all the same.
    RefusesButReplaces,
}

struct Broken {
    inner: MemoryStore,
    defect: Defect,
}

fn server_error() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 500,
        code: "InternalError".into(),
        message: "refused".into(),
    }
}

/// A future that is pending once and then ready, so the futures joined with it run.
struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl Broken {
    /// The stored bytes of `key`, if any.
    async fn stored(&self, key: &ObjectKey) -> Option<Bytes> {
        let all = ByteRange::new(0, u64::MAX / 2).unwrap();
        self.inner.get_range(key, all).await.ok()
    }
}

impl ObjectStore for Broken {
    fn capabilities(&self) -> Capabilities {
        CONDITIONAL
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let first = key.as_str().ends_with("/used-key");
        let racing = key.as_str().ends_with("/used-key-race");
        match self.defect {
            Defect::RefusesOnlySameLength => {
                if let Some(old) = self.stored(key).await {
                    if old.len() == body.len() {
                        return Err(ObjectStoreError::AlreadyExists(key.clone()));
                    }
                    self.inner.delete(key).await?;
                }
            }
            Defect::ChecksThenWrites => {
                let taken = self.stored(key).await.is_some();
                YieldNow(false).await;
                if taken {
                    return Err(ObjectStoreError::AlreadyExists(key.clone()));
                }
                self.inner.delete(key).await?;
            }
            Defect::ErrorsOnARetry if first => {
                return match self.inner.put_new(key, body, retain_until).await {
                    Err(ObjectStoreError::AlreadyExists(_)) => Err(server_error()),
                    other => other,
                };
            }
            Defect::FailsARace if racing => return Err(server_error()),
            Defect::RefusesButReplaces if first => {
                let answer = self.inner.put_new(key, body.clone(), retain_until).await;
                if matches!(answer, Err(ObjectStoreError::AlreadyExists(_))) {
                    self.inner.delete(key).await?;
                    self.inner.put_new(key, body, retain_until).await?;
                }
                return answer;
            }
            _ => {}
        }
        self.inner.put_new(key, body, retain_until).await
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        let bytes = kbf_objstore::gather(body, len).await?;
        self.put_new(key, bytes, None).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
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

fn run(defect: Defect) -> Report {
    let store = Broken {
        inner: MemoryStore::new(CONDITIONAL),
        defect,
    };
    let prefix = KeyPrefix::new("conformance/").unwrap();
    block_on(conformance::run(&store, &prefix))
}

/// Runs the suite on a sound store that already holds `names`, under `prefix`.
fn run_under(prefix: &str, names: &[&str]) -> Report {
    let store = MemoryStore::new(CONDITIONAL);
    for name in names {
        let k = ObjectKey::new(*name).unwrap();
        block_on(store.put_new(&k, Bytes::from_static(b"mine"), None)).unwrap();
    }
    block_on(conformance::run(&store, &KeyPrefix::new(prefix).unwrap()))
}

/// Asserts that exactly the used-key check failed, and says why.
fn used_key_failure(report: &Report) -> &str {
    let failed: Vec<Check> = report.failures().map(|(c, _)| c).collect();
    assert_eq!(failed, [Check::UsedKeyKeepsItsBytes], "\n{report}");
    match report.outcome(Check::UsedKeyKeepsItsBytes) {
        Some(Outcome::Failed(why)) => why,
        other => panic!("not failed: {other:?}\n{report}"),
    }
}

/// Catches: a used-key check that tries only a body as long as the stored one, so a
/// store that refuses a same-length overwrite and replaces the object with a shorter or
/// longer body passes, and a key would hold other bytes.
#[test]
fn a_store_that_replaces_a_used_key_with_a_body_of_another_length_fails() {
    let report = run(Defect::RefusesOnlySameLength);
    assert_eq!(
        used_key_failure(&report),
        "put_new of shorter bytes onto conformance/used-key: expected AlreadyExists, got success"
    );
    // The checks with a same-length body pass for it.
    for check in [Check::PutNewRefusesOverwrite, Check::StreamedPut] {
        assert_eq!(report.outcome(check), Some(&Outcome::Passed), "{report}");
    }
}

/// Catches: a used-key check that races no writers, so a store whose existence check and
/// write are apart (every racer sees a free key and the last body stays) passes.
#[test]
fn a_store_whose_check_and_write_are_apart_fails_the_race() {
    let report = run(Defect::ChecksThenWrites);
    assert_eq!(
        used_key_failure(&report),
        "4 of 4 put_new calls racing for conformance/used-key-race were answered Ok, expected exactly one"
    );
}

/// Catches: a used-key check that accepts any answer to a retry of the same bytes.
#[test]
fn a_store_that_errors_on_a_retry_of_the_same_bytes_fails() {
    let report = run(Defect::ErrorsOnARetry);
    let why = used_key_failure(&report);
    assert!(
        why.starts_with(
            "put_new of the same bytes onto conformance/used-key: expected success or AlreadyExists, got error: "
        ),
        "{why}"
    );
}

/// Catches: a used-key check that counts only Ok answers in the race, so a racer that
/// fails outright is taken for a loser.
#[test]
fn a_store_that_fails_a_racing_write_fails_naming_the_answer() {
    let report = run(Defect::FailsARace);
    let why = used_key_failure(&report);
    assert!(
        why.starts_with(
            "a put_new racing for conformance/used-key-race: expected Ok or AlreadyExists, got error: "
        ),
        "{why}"
    );
}

/// Catches: a used-key check that trusts the answer and not the bytes: a refusal that
/// still replaces the object.
#[test]
fn a_refused_write_that_replaced_the_bytes_fails_naming_the_key() {
    let report = run(Defect::RefusesButReplaces);
    assert_eq!(
        used_key_failure(&report),
        "conformance/used-key after put_new of shorter bytes: reads 32 bytes that differ from the 64 first written"
    );
}

/// Catches: a used-key check that carries on when its first write was refused (the key
/// was taken by someone else), and so tests another's object, not its own.
#[test]
fn a_used_key_that_is_taken_fails_naming_the_put() {
    let report = run_under("conformance/", &["conformance/used-key"]);
    let why = used_key_failure(&report);
    assert!(why.starts_with("put_new conformance/used-key: "), "{why}");
}

/// Catches: a used-key race that takes "no writer won" for a pass: when the racing key is
/// already taken every racer is refused, and the check must say none was answered Ok.
#[test]
fn a_race_that_nobody_wins_fails_naming_the_count() {
    let report = run_under("conformance/", &["conformance/used-key-race"]);
    assert_eq!(
        used_key_failure(&report),
        "0 of 4 put_new calls racing for conformance/used-key-race were answered Ok, expected exactly one"
    );
}

/// Catches: a used-key check that panics, or passes, when the racing key is too long for
/// the store while the first key still fits.
#[test]
fn a_prefix_that_fits_the_used_key_but_not_the_racing_key_fails_naming_it() {
    let prefix = format!("{}/", "p".repeat(1013));
    let report = run_under(&prefix, &[]);
    let why = report.outcome(Check::UsedKeyKeepsItsBytes);
    let Some(Outcome::Failed(why)) = why else {
        panic!("not failed: {why:?}\n{report}");
    };
    assert!(why.starts_with("scratch key used-key-race: "), "{why}");
}
