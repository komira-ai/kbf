//! The conformance suite's failure messages for the faults `conformance_catches.rs` does
//! not plant: a store whose reads, deletes, retained delete or streamed overwrite break,
//! and a prefix too long for the suite's scratch keys. Each test names the check that
//! must fail and the words the report gives an operator to find the fault.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use futures::executor::block_on;
use kbf_objstore::conformance::{self, Check, Outcome, Report};
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey,
    ObjectStore, ObjectStoreError, PageSize,
};

const ALL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: true,
};

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    /// Every read fails with a server error.
    ReadsFail,
    /// A read starting past the end answers success with no bytes.
    PastEndIsEmpty,
    /// A delete of a key ending `/deleted` fails.
    DeleteOfDeletedFails,
    /// A delete of a key that was never written fails.
    DeleteOfNeverWrittenFails,
    /// A delete of the retained key answers success and keeps the bytes.
    RetainedDeleteLiesAndKeeps,
    /// A delete of the retained key answers success; the key then reads other bytes.
    RetainedDeleteCorrupts,
    /// A put with a retention date stores the object unretained, so a delete removes it.
    DropsRetention,
    /// A listing under `<dir>/` also lists the keys that merely begin `<dir>`.
    ListsSiblingPrefix,
    /// A put with a retention date fails with a server error.
    RefusesRetention,
    /// A streamed put onto an existing key replaces it and answers `AlreadyExists`.
    ReplacesWhileRefusing,
}

struct Faulty {
    inner: MemoryStore,
    fault: Fault,
    /// [`Fault::RetainedDeleteCorrupts`]: a delete of the retained key was answered.
    corrupted: AtomicBool,
}

fn server_error() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 500,
        code: "InternalError".into(),
        message: "injected".into(),
    }
}

impl ObjectStore for Faulty {
    fn capabilities(&self) -> Capabilities {
        ALL
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        if self.fault == Fault::RefusesRetention && retain_until.is_some() {
            return Err(server_error());
        }
        let retain_until = retain_until.filter(|_| self.fault != Fault::DropsRetention);
        self.inner.put_new(key, body, retain_until).await
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        let bytes = kbf_objstore::gather(body, len).await?;
        if self.fault == Fault::ReplacesWhileRefusing && self.inner.may_hold(key).await? {
            self.inner.delete(key).await?;
            self.inner.put_new(key, bytes, None).await?;
            return Err(ObjectStoreError::AlreadyExists(key.clone()));
        }
        self.inner.put_new(key, bytes, None).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        if self.corrupted.load(Ordering::SeqCst) && key.as_str().ends_with("/locked") {
            return Ok(Bytes::from_static(b"not what was written"));
        }
        match (self.fault, self.inner.get_range(key, range).await) {
            (Fault::ReadsFail, _) => Err(server_error()),
            (Fault::PastEndIsEmpty, Err(ObjectStoreError::InvalidRange { .. })) => Ok(Bytes::new()),
            (_, other) => other,
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let name = key.as_str();
        match self.fault {
            Fault::DeleteOfDeletedFails if name.ends_with("/deleted") => Err(server_error()),
            Fault::DeleteOfNeverWrittenFails if name.ends_with("/never-deleted") => {
                Err(server_error())
            }
            Fault::RetainedDeleteLiesAndKeeps if name.ends_with("/locked") => Ok(()),
            Fault::RetainedDeleteCorrupts if name.ends_with("/locked") => {
                self.corrupted.store(true, Ordering::SeqCst);
                Ok(())
            }
            _ => self.inner.delete(key).await,
        }
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        if self.fault == Fault::ListsSiblingPrefix {
            let loose = KeyPrefix::new(prefix.as_str().trim_end_matches('/')).unwrap();
            return self.inner.list(&loose, after, max_keys).await;
        }
        self.inner.list(prefix, after, max_keys).await
    }
}

fn run_with(fault: Fault, prefix: &KeyPrefix) -> Report {
    let store = Faulty {
        inner: MemoryStore::new(ALL),
        fault,
        corrupted: AtomicBool::new(false),
    };
    block_on(conformance::run(&store, prefix))
}

fn run(fault: Fault) -> Report {
    run_with(fault, &KeyPrefix::new("conformance/").unwrap())
}

fn why(report: &Report, check: Check) -> &str {
    match report.outcome(check) {
        Some(Outcome::Failed(why)) => why,
        other => panic!("{check:?} should have failed, got {other:?}\n{report}"),
    }
}

/// Catches: a read error swallowed by the suite (a store that cannot read passing the
/// round trip), or reported without naming the call and the key that failed.
#[test]
fn a_store_whose_reads_fail_fails_the_round_trip_naming_the_read() {
    let report = run(Fault::ReadsFail);
    let m = why(&report, Check::RoundTrip);
    assert!(m.starts_with("get_range conformance/round-trip-1 "), "{m}");
    assert!(m.contains("InternalError"), "{m}");
}

/// Catches: a suite that accepts an empty answer for a read starting past the end of
/// an object (it must be `InvalidRange`: an empty success reads as a zero-length tail).
#[test]
fn a_range_past_the_end_answered_with_success_fails_range_boundaries() {
    let report = run(Fault::PastEndIsEmpty);
    let m = why(&report, Check::RangeBoundaries);
    assert!(m.contains("expected InvalidRange, got success"), "{m}");
    assert!(m.contains("a range starting at"), "{m}");
}

/// Catches: a suite that skips the delete errors: a store that cannot delete a written
/// object, and one that fails a delete of a key that never existed (a retried delete
/// must be safe), each named by its own message.
#[test]
fn failing_deletes_fail_delete_idempotent_and_say_which_delete() {
    let report = run(Fault::DeleteOfDeletedFails);
    let m = why(&report, Check::DeleteIdempotent);
    assert!(
        m.starts_with("first delete of conformance/deleted: "),
        "{m}"
    );
    let report = run(Fault::DeleteOfNeverWrittenFails);
    let m = why(&report, Check::DeleteIdempotent);
    assert!(
        m.starts_with("deleting conformance/never-deleted, never written: "),
        "{m}"
    );
}

/// Catches: a store whose delete of a retained object says success while the bytes stay
/// (kbf would believe them gone): the report says the bytes survive.
#[test]
fn a_retained_delete_that_succeeds_and_keeps_the_bytes_fails_object_lock() {
    let report = run(Fault::RetainedDeleteLiesAndKeeps);
    let m = why(&report, Check::ObjectLock);
    assert!(m.contains("reported success (the bytes survive"), "{m}");
}

/// Catches: a retained delete that succeeds and leaves the key reading other bytes: the
/// suite must not take "the bytes differ" for "the bytes survive" (it says the object no
/// longer reads as written).
#[test]
fn a_retained_delete_that_leaves_other_bytes_fails_object_lock() {
    let report = run(Fault::RetainedDeleteCorrupts);
    let m = why(&report, Check::ObjectLock);
    assert!(m.contains("the object no longer reads"), "{m}");
    assert!(!m.contains("the bytes survive"), "{m}");
}

/// Catches: a suite that takes a delete of a retained object that succeeds and then hides
/// the object for a pass (the store removed what it promised to keep).
#[test]
fn a_retained_delete_that_removes_the_object_fails_object_lock() {
    let report = run(Fault::DropsRetention);
    let m = why(&report, Check::ObjectLock);
    assert!(m.contains("the object no longer reads"), "{m}");
}

/// Catches: a listing that also returns the keys sharing the prefix's text but not its
/// `/` (a sweep would delete a neighbour's objects): ListPagination names the extra key.
#[test]
fn a_listing_that_includes_a_key_outside_the_prefix_fails_pagination() {
    let report = run(Fault::ListsSiblingPrefix);
    let m = why(&report, Check::ListPagination);
    assert!(
        m.contains("unexpected [\"conformance/list-outside\"]"),
        "{m}"
    );
    assert!(m.contains("missing []"), "{m}");
}

/// Catches: a store that claims Object Lock and then refuses a retained put: the suite
/// must report the put, not go on to test a delete of an object that was never written.
#[test]
fn a_store_that_cannot_write_a_retained_object_fails_object_lock() {
    let report = run(Fault::RefusesRetention);
    let m = why(&report, Check::ObjectLock);
    assert!(
        m.starts_with("put_new conformance/locked with retention: "),
        "{m}"
    );
}

/// Catches: a streamed overwrite refused in word but done in fact (the answer says
/// `AlreadyExists` and the first bytes are gone).
#[test]
fn a_refused_streamed_overwrite_that_changed_the_object_fails_streamed_put() {
    let report = run(Fault::ReplacesWhileRefusing);
    let m = why(&report, Check::StreamedPut);
    assert!(m.ends_with("a refused overwrite changed it"), "{m}");
}

/// Catches: a report that prints a failure without its words or hides a skipped check,
/// which an operator reads to see what was and was not proven.
#[test]
fn the_report_prints_failed_and_skipped_checks_with_their_reasons() {
    let prefix = KeyPrefix::new("conformance/").unwrap();
    let plain = block_on(conformance::run(
        &MemoryStore::new(Capabilities::default()),
        &prefix,
    ));
    let text = plain.to_string();
    assert!(
        text.contains("skip PutNewRefusesOverwrite: the store does not claim conditional writes\n"),
        "{text}"
    );
    assert!(text.contains("pass RoundTrip\n"), "{text}");
    let text = run(Fault::ReadsFail).to_string();
    assert!(
        text.contains("FAIL RoundTrip: get_range conformance/round-trip-1 "),
        "{text}"
    );
}

/// Catches: a prefix so long that the suite's scratch keys are not valid, which the
/// suite must report as the checks' failure (naming the scratch key or prefix) and not
/// panic or pass without having written anything.
#[test]
fn a_prefix_too_long_for_the_scratch_keys_fails_the_checks_that_need_them() {
    let long = format!("{}/", "p".repeat(1020));
    let prefix = KeyPrefix::new(long).unwrap();
    let report = run_with(Fault::ReplacesWhileRefusing, &prefix);
    let failures: Vec<_> = report.failures().collect();
    assert!(failures.len() >= 10, "{report}");
    for (check, m) in &failures {
        assert!(
            m.starts_with("scratch key ") || m.starts_with("scratch prefix "),
            "{check:?}: {m}"
        );
    }
    assert!(
        failures
            .iter()
            .any(|(_, m)| m.starts_with("scratch prefix ")),
        "{report}"
    );
}
