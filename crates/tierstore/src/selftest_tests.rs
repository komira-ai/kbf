//! The self-test of a backend: the reference backends pass it, and a backend that breaks one
//! answer of the contract is refused with the check and the answer.

use async_trait::async_trait;
use bytes::Bytes;
use kbf_testkit::dir::TestDir;

use crate::{
    Backend, ByteRange, Check, DirBackend, Error, Listed, MemoryBackend, Meta, PartBody,
    SelfTestFailure, self_test,
};

/// The prefix the probe keys are under.
const PROBE: &str = "_tiering/selftest/";

/// How a [`Flawed`] backend differs from a memory backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flaw {
    /// Behaves as the memory backend.
    None,
    /// A create-only put replaces what is there, as a store that ignores `If-None-Match`.
    OverwritesOnCreate,
    /// A create-only put of a key that exists is answered 501, as a store that rejects the header.
    RejectsConditionalHeader,
    /// A replace on a wrong content tag takes effect, as a store that ignores `If-Match`.
    IgnoresIfMatch,
    /// A ranged read gives the whole object, as a store that answers 200 to a `Range` request.
    WholeObjectForRange,
    /// A HEAD fails as the S3 backend does for an answer with no `Last-Modified`.
    NoLastModified,
    /// A listing after a key starts from the first key.
    IgnoresStartAfter,
    /// A multipart completion replaces the object, as a store that ignores `If-None-Match` there.
    CompletionOverwrites,
    /// The multipart calls are not done.
    NoMultipart,
}

/// A memory backend with one flaw. The memory backend is reachable, to see what a test left.
pub(crate) struct Flawed {
    pub(crate) inner: MemoryBackend,
    flaw: Flaw,
}

impl Flawed {
    pub(crate) fn new(flaw: Flaw) -> Self {
        Self {
            inner: MemoryBackend::new(),
            flaw,
        }
    }

    fn is(&self, flaw: Flaw) -> bool {
        self.flaw == flaw
    }

    fn multipart_refused(&self) -> Result<(), Error> {
        if self.is(Flaw::NoMultipart) {
            return Err(Error::Unsupported("no multipart here".to_owned()));
        }
        Ok(())
    }
}

#[async_trait]
impl Backend for Flawed {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let exists = self.inner.head(key).await.is_ok();
        if exists && self.is(Flaw::OverwritesOnCreate) {
            self.inner.delete(key).await?;
        }
        if exists && self.is(Flaw::RejectsConditionalHeader) {
            return Err(Error::Other("501 NotImplemented: If-None-Match".to_owned()));
        }
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let mut etag = etag.to_owned();
        if self.is(Flaw::IgnoresIfMatch) {
            etag = self.inner.head(key).await?.tag;
        }
        self.inner.put_if_match(key, body, &etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let range = range.filter(|_| !self.is(Flaw::WholeObjectForRange));
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        if self.is(Flaw::NoLastModified) {
            return Err(Error::Other("missing or bad Last-Modified".to_owned()));
        }
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let after = start_after.filter(|_| !self.is(Flaw::IgnoresStartAfter));
        self.inner.list_page(prefix, after, max_keys).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.multipart_refused()?;
        self.inner.create_upload(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.inner.upload_part(key, id, number, body).await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        if self.is(Flaw::CompletionOverwrites) {
            self.inner.delete(key).await?;
        }
        self.inner.complete_upload(key, id, parts).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        self.inner.abort_upload(key, id).await
    }
}

/// The failure of a self-test that must have failed.
async fn refused(flaw: Flaw, multipart: bool) -> SelfTestFailure {
    let backend = Flawed::new(flaw);
    let outcome = self_test("the backend", &backend, "run-1", multipart, &[]).await;
    let failure = outcome.expect_err("refused");
    // Whatever the test wrote is gone, whether it passed or not.
    let left = backend.inner.list(PROBE).await.unwrap();
    assert!(left.is_empty(), "{flaw:?} left {left:?}");
    assert_eq!(failure.backend, "the backend");
    failure
}

/// Asserts that a backend that is correct passes every check, as a hot tier and as a cold tier,
/// and leaves no probe key behind: the memory backend and a flat directory backend, the
/// references of the contract.
#[tokio::test]
async fn the_memory_and_the_directory_backend_pass_and_leave_nothing() {
    let memory = MemoryBackend::new();
    self_test("memory", &memory, "run-1", false, &[])
        .await
        .unwrap();
    self_test("memory", &memory, "run-2", true, &[])
        .await
        .unwrap();
    assert!(memory.list(PROBE).await.unwrap().is_empty());

    let dir = TestDir::under(std::env::temp_dir(), "tierstore-selftest");
    let folder = DirBackend::open(dir.path().join("bucket")).unwrap();
    self_test("dir", &folder, "run-1", false, &[])
        .await
        .unwrap();
    self_test("dir", &folder, "run-1", true, &[]).await.unwrap();
    assert!(folder.list(PROBE).await.unwrap().is_empty());
}

/// Asserts that the probe keys are in the reserved prefix and under the run's id: a backend that
/// is correct is asked about keys that start with `_tiering/selftest/<run id>/`, and no other key
/// is written or deleted.
#[tokio::test]
async fn the_probe_keys_are_under_the_reserved_prefix_of_the_run() {
    let flawed = Flawed::new(Flaw::None);
    flawed
        .put("client/object", Bytes::from_static(b"kept"))
        .await
        .unwrap();
    self_test("flawed", &flawed, "run-9", true, &[])
        .await
        .unwrap();
    let all = flawed.inner.list("").await.unwrap();
    let keys: Vec<&str> = all.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(keys, ["client/object"]);
}

/// Asserts that a create-only put that overwrites is refused as the create-only check, with the
/// answer that the second put was accepted.
#[tokio::test]
async fn a_create_only_put_that_overwrites_is_refused() {
    let failure = refused(Flaw::OverwritesOnCreate, true).await;
    assert_eq!(failure.check, Check::CreateOnly);
    assert!(failure.observed.contains("was accepted"), "{failure}");
    let text = failure.to_string();
    assert!(text.contains("refusing to start"), "{text}");
    assert!(text.contains("the backend"), "{text}");
    assert!(text.contains("create-only"), "{text}");
}

/// Asserts that a store that answers the conditional header with another error than a refusal
/// is refused as the create-only check, and the message gives that answer.
#[tokio::test]
async fn a_create_only_put_answered_with_another_error_is_refused() {
    let failure = refused(Flaw::RejectsConditionalHeader, true).await;
    assert_eq!(failure.check, Check::CreateOnly);
    assert!(failure.observed.contains("501 NotImplemented"), "{failure}");
}

/// Asserts that a replace that ignores `If-Match` is refused as the if-match check.
#[tokio::test]
async fn a_replace_that_ignores_if_match_is_refused() {
    let failure = refused(Flaw::IgnoresIfMatch, true).await;
    assert_eq!(failure.check, Check::IfMatch);
    assert!(failure.observed.contains("wrong ETag"), "{failure}");
    assert!(failure.observed.contains("was accepted"), "{failure}");
}

/// Asserts that a ranged read answered with the whole object is refused as the range check, and
/// the message gives what came back.
#[tokio::test]
async fn a_range_answered_with_the_whole_object_is_refused() {
    let failure = refused(Flaw::WholeObjectForRange, true).await;
    assert_eq!(failure.check, Check::Range);
    assert!(failure.observed.contains("0123456789"), "{failure}");
    assert!(failure.observed.contains("2345"), "{failure}");
}

/// Asserts that a head that fails for want of `Last-Modified` is refused as the last-modified
/// check, with the error the backend gave.
#[tokio::test]
async fn a_head_without_last_modified_is_refused() {
    let failure = refused(Flaw::NoLastModified, true).await;
    assert_eq!(failure.check, Check::LastModified);
    assert!(failure.observed.contains("Last-Modified"), "{failure}");
}

/// Asserts that a listing that ignores `start_after` is refused as the paging check, and the
/// message gives the keys it returned.
#[tokio::test]
async fn a_listing_that_ignores_start_after_is_refused() {
    let failure = refused(Flaw::IgnoresStartAfter, true).await;
    assert_eq!(failure.check, Check::Paging);
    assert!(failure.observed.contains("page/b"), "{failure}");
    assert!(
        failure.observed.contains("the page after the second key"),
        "{failure}"
    );
}

/// Asserts that a multipart completion that replaces an object that exists is refused as the
/// multipart-conditional check, that the upload it started is not left open, and that the same
/// backend is not asked on a tier that takes no multipart uploads.
#[tokio::test]
async fn a_multipart_completion_that_overwrites_is_refused_on_the_cold_tier_only() {
    let failure = refused(Flaw::CompletionOverwrites, true).await;
    assert_eq!(failure.check, Check::MultipartConditional);
    assert!(failure.observed.contains("was accepted"), "{failure}");

    let backend = Flawed::new(Flaw::CompletionOverwrites);
    self_test("hot", &backend, "run-1", false, &[])
        .await
        .unwrap();
}

/// Asserts that a cold tier that does not do multipart uploads is refused as the
/// multipart-conditional check, with the error it gave.
#[tokio::test]
async fn a_cold_tier_without_multipart_is_refused() {
    let failure = refused(Flaw::NoMultipart, true).await;
    assert_eq!(failure.check, Check::MultipartConditional);
    assert!(failure.observed.contains("no multipart here"), "{failure}");
}

/// Asserts that a check named in `skip` does not run, and the others do: a backend that
/// overwrites on a create-only put passes with that check skipped, one that also ignores
/// `If-Match` is still refused as the if-match check, and with every check skipped backends that
/// would fail a different check each are not asked anything.
#[tokio::test]
async fn a_skipped_check_does_not_run_and_the_others_do() {
    let overwrites = Flawed::new(Flaw::OverwritesOnCreate);
    let skip = [Check::CreateOnly];
    self_test("b", &overwrites, "run-1", true, &skip)
        .await
        .unwrap();

    let both = Flawed::new(Flaw::IgnoresIfMatch);
    let failure = self_test("b", &both, "run-1", true, &skip)
        .await
        .unwrap_err();
    assert_eq!(failure.check, Check::IfMatch);

    // With every check skipped, a backend that fails a different check each is not asked.
    for flaw in [
        Flaw::NoLastModified,
        Flaw::WholeObjectForRange,
        Flaw::NoMultipart,
    ] {
        let flawed = Flawed::new(flaw);
        self_test("b", &flawed, "run-1", true, &Check::ALL)
            .await
            .unwrap();
    }
}

/// Asserts that the checks are named as the flag and the messages name them, and that a name
/// that is not one parses to nothing.
#[test]
fn checks_have_names_that_parse_back() {
    let names: Vec<&str> = Check::ALL.iter().map(|check| check.name()).collect();
    assert_eq!(
        names,
        [
            "create-only",
            "last-modified",
            "if-match",
            "range",
            "paging",
            "multipart-conditional"
        ]
    );
    for check in Check::ALL {
        assert_eq!(Check::parse(check.name()), Some(check));
        assert_eq!(check.to_string(), check.name());
    }
    assert_eq!(Check::parse("Create-Only"), None);
    assert_eq!(Check::parse(""), None);
}

/// Asserts that a failure is an error whose text names the backend, the check and the answer.
#[test]
fn a_failure_reads_as_an_error_that_names_backend_check_and_answer() {
    let failure = SelfTestFailure {
        backend: "cold tier s3:bucket".to_owned(),
        check: Check::Range,
        observed: "a ranged GET gave the whole object".to_owned(),
    };
    let error: &dyn std::error::Error = &failure;
    assert_eq!(
        error.to_string(),
        "refusing to start: cold tier s3:bucket failed the range check: a ranged GET gave the whole object"
    );
}
