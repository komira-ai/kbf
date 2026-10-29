//! The self-test of a backend: the reference backends pass it, a backend that answers a check
//! wrongly is refused with the check and the answer, one that cannot be asked is unreachable, and
//! one that is full is not in doubt.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use kbf_testkit::dir::TestDir;

use crate::{
    Backend, ByteRange, Check, DirBackend, Error, Listed, MemoryBackend, Meta, PartBody,
    SelfTestError, SelfTestFailure, SelfTestOptions, TestClock, self_test,
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
    /// A HEAD gives a size that is one too many.
    HeadLies,
    /// A listing after a key starts from the first key.
    IgnoresStartAfter,
    /// A multipart completion replaces the object, as a store that ignores `If-None-Match` there.
    CompletionOverwrites,
    /// The multipart calls are not done.
    NoMultipart,
    /// Every put fails as a store that is down does (a refused connection).
    Down,
    /// The first put fails as a store that answers 503 once does.
    FlakyOnce,
    /// Every put fails as a store that has no room does (507).
    Full,
    /// A listing by page fails with an error that is not a down store's.
    ListFails,
    /// A delete fails with an error that is not a down store's.
    DeleteFails,
}

/// A memory backend with one flaw. The memory backend is reachable, to see what a test left.
pub(crate) struct Flawed {
    pub(crate) inner: MemoryBackend,
    flaw: Flaw,
    tripped: AtomicBool,
}

impl Flawed {
    pub(crate) fn new(flaw: Flaw) -> Self {
        Self {
            inner: MemoryBackend::new(),
            flaw,
            tripped: AtomicBool::new(false),
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

    /// The error a put gives for the flaws that fail it.
    fn put_error(&self) -> Result<(), Error> {
        if self.is(Flaw::Down) {
            return Err(Error::Unavailable("connection refused".to_owned()));
        }
        if self.is(Flaw::Full) {
            return Err(Error::Full("507 Insufficient Storage".to_owned()));
        }
        if self.is(Flaw::FlakyOnce) && !self.tripped.swap(true, Ordering::SeqCst) {
            return Err(Error::Unavailable("503 Service Unavailable".to_owned()));
        }
        Ok(())
    }
}

#[async_trait]
impl Backend for Flawed {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.put_error()?;
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
        let mut meta = self.inner.head(key).await?;
        if self.is(Flaw::HeadLies) {
            meta.size += 1;
        }
        Ok(meta)
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        if self.is(Flaw::DeleteFails) {
            return Err(Error::Other("delete denied".to_owned()));
        }
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
        if self.is(Flaw::ListFails) {
            return Err(Error::Other("listing broke".to_owned()));
        }
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

/// The options of a run: three attempts, no wait between them, the time zero.
fn options<'a>(run_id: &'a str, multipart: bool, skip: &'a [Check]) -> SelfTestOptions<'a> {
    SelfTestOptions {
        name: "the backend",
        run_id,
        multipart,
        skip,
        attempts: 3,
        pause: Duration::ZERO,
        now: 0,
    }
}

/// Runs the self-test and gives its outcome and the notes it made.
async fn go(
    backend: &dyn Backend,
    options: &SelfTestOptions<'_>,
) -> (Result<(), SelfTestError>, Vec<String>) {
    let notes = Mutex::new(Vec::new());
    let note = |line: String| notes.lock().unwrap().push(line);
    let outcome = self_test(backend, options, &note).await;
    (outcome, notes.into_inner().unwrap())
}

/// The failure of a self-test that must have found a backend answering wrongly; no probe key is
/// left in the backend whatever the answer.
async fn refused(flaw: Flaw, multipart: bool) -> SelfTestFailure {
    let backend = Flawed::new(flaw);
    let (outcome, _) = go(&backend, &options("run-1", multipart, &[])).await;
    let failure = match outcome {
        Err(SelfTestError::Violation(failure)) => failure,
        other => panic!("{flaw:?} was not found to answer wrongly: {other:?}"),
    };
    let left = backend.inner.list(PROBE).await.unwrap();
    assert!(left.is_empty(), "{flaw:?} left {left:?}");
    assert_eq!(failure.backend, "the backend");
    failure
}

/// Asserts that a backend that is correct passes every check, as a hot tier and as a cold tier,
/// says nothing and leaves no probe key behind: the memory backend and a flat directory backend,
/// the references of the contract.
#[tokio::test]
async fn the_memory_and_the_directory_backend_pass_and_leave_nothing() {
    let memory = MemoryBackend::new();
    for (run, multipart) in [("run-1", false), ("run-2", true)] {
        let (outcome, notes) = go(&memory, &options(run, multipart, &[])).await;
        assert_eq!(outcome, Ok(()));
        assert!(notes.is_empty(), "{notes:?}");
    }
    assert!(memory.list(PROBE).await.unwrap().is_empty());

    let dir = TestDir::under(std::env::temp_dir(), "tierstore-selftest");
    let folder = DirBackend::open(dir.path().join("bucket")).unwrap();
    for multipart in [false, true] {
        let (outcome, notes) = go(&folder, &options("run-1", multipart, &[])).await;
        assert_eq!(outcome, Ok(()));
        assert!(notes.is_empty(), "{notes:?}");
    }
    assert!(folder.list(PROBE).await.unwrap().is_empty());
}

/// Asserts that only keys of the run's reserved prefix are written or deleted: a client object
/// is still there afterwards.
#[tokio::test]
async fn the_probe_keys_are_under_the_reserved_prefix_of_the_run() {
    let flawed = Flawed::new(Flaw::None);
    let client = Bytes::from_static(b"kept");
    flawed.put("client/object", client).await.unwrap();
    let (outcome, _) = go(&flawed, &options("run-9", true, &[])).await;
    assert_eq!(outcome, Ok(()));
    let all = flawed.inner.list("").await.unwrap();
    let keys: Vec<&str> = all.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(keys, ["client/object"]);
}

/// Asserts that a create-only put that overwrites is refused as the create-only check, with the
/// answer that the second put was accepted, and that the message names the backend and the check.
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

/// Asserts that a store that answers the conditional header with an error that is not a refusal
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

/// Asserts that a head that fails for want of `Last-Modified` is refused as the head check, with
/// the error the backend gave.
#[tokio::test]
async fn a_head_without_last_modified_is_refused() {
    let failure = refused(Flaw::NoLastModified, true).await;
    assert_eq!(failure.check, Check::Head);
    assert!(failure.observed.contains("Last-Modified"), "{failure}");
}

/// Asserts that a head that gives another size than the object written is refused as the head
/// check, and the message gives both sizes.
#[tokio::test]
async fn a_head_with_the_wrong_size_is_refused() {
    let failure = refused(Flaw::HeadLies, true).await;
    assert_eq!(failure.check, Check::Head);
    assert!(failure.observed.contains("size"), "{failure}");
    assert!(failure.observed.contains("5, not 4"), "{failure}");
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
/// multipart-conditional check, and that the same backend is not asked on a tier that takes no
/// multipart uploads.
#[tokio::test]
async fn a_multipart_completion_that_overwrites_is_refused_on_the_cold_tier_only() {
    let failure = refused(Flaw::CompletionOverwrites, true).await;
    assert_eq!(failure.check, Check::MultipartConditional);
    assert!(failure.observed.contains("was accepted"), "{failure}");

    let backend = Flawed::new(Flaw::CompletionOverwrites);
    let (outcome, _) = go(&backend, &options("run-1", false, &[])).await;
    assert_eq!(outcome, Ok(()));
}

/// Asserts that a cold tier that does not do multipart uploads is refused as the
/// multipart-conditional check, with the error it gave.
#[tokio::test]
async fn a_cold_tier_without_multipart_is_refused() {
    let failure = refused(Flaw::NoMultipart, true).await;
    assert_eq!(failure.check, Check::MultipartConditional);
    assert!(failure.observed.contains("no multipart here"), "{failure}");
}

/// Asserts that a store that cannot be asked (a refused connection) is not a wrong answer: the
/// first check is run three times, with a note for each repeat, and the outcome is unreachable
/// with the backend and the error, not a violation; nothing is left behind.
#[tokio::test]
async fn a_store_that_cannot_be_asked_is_unreachable_after_three_attempts() {
    let backend = Flawed::new(Flaw::Down);
    let (outcome, notes) = go(&backend, &options("run-1", true, &[])).await;
    let (name, error) = match outcome {
        Err(SelfTestError::Unreachable { backend, error }) => (backend, error),
        other => panic!("{other:?}"),
    };
    assert_eq!(name, "the backend");
    assert!(error.contains("connection refused"), "{error}");
    let repeats = notes.iter().filter(|note| note.contains("trying again"));
    assert_eq!(repeats.count(), 2, "{notes:?}");
    assert!(
        notes[0].contains("the backend: the create-only check"),
        "{notes:?}"
    );
    assert!(backend.inner.list(PROBE).await.unwrap().is_empty());
    let text = SelfTestError::Unreachable {
        backend: name,
        error,
    }
    .to_string();
    assert!(
        text.starts_with("tier unreachable: the backend: "),
        "{text}"
    );
}

/// Asserts that a store that fails once with a 503 and then answers passes, with one note for the
/// repeat: a brief outage at the start does not stop the start.
#[tokio::test]
async fn a_store_that_fails_once_is_asked_again_and_passes() {
    let backend = Flawed::new(Flaw::FlakyOnce);
    let (outcome, notes) = go(&backend, &options("run-1", true, &[])).await;
    assert_eq!(outcome, Ok(()));
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("503 Service Unavailable"), "{notes:?}");
}

/// Asserts that a store that is full passes without being asked anything that needs a write: each
/// check that needs one is skipped with a note that says the tier is full (five on a hot tier, six
/// on a cold one), and nothing is left behind.
#[tokio::test]
async fn a_full_store_passes_with_a_note_for_each_check_it_skipped() {
    for (multipart, skipped) in [(false, 5), (true, 6)] {
        let backend = Flawed::new(Flaw::Full);
        let (outcome, notes) = go(&backend, &options("run-1", multipart, &[])).await;
        assert_eq!(outcome, Ok(()));
        let full = notes
            .iter()
            .filter(|note| note.contains("the tier is full"));
        assert_eq!(full.count(), skipped, "{notes:?}");
        assert!(backend.inner.list(PROBE).await.unwrap().is_empty());
    }
}

/// Asserts that a check named in `skip` does not run, and the others do: a backend that
/// overwrites on a create-only put passes with that check skipped, one that also ignores
/// `If-Match` is still refused as the if-match check, and with every check skipped backends that
/// would fail a different check each are not asked anything.
#[tokio::test]
async fn a_skipped_check_does_not_run_and_the_others_do() {
    let overwrites = Flawed::new(Flaw::OverwritesOnCreate);
    let skip = [Check::CreateOnly];
    let (outcome, _) = go(&overwrites, &options("run-1", true, &skip)).await;
    assert_eq!(outcome, Ok(()));

    let both = Flawed::new(Flaw::IgnoresIfMatch);
    let (outcome, _) = go(&both, &options("run-1", true, &skip)).await;
    let failure = match outcome {
        Err(SelfTestError::Violation(failure)) => failure,
        other => panic!("{other:?}"),
    };
    assert_eq!(failure.check, Check::IfMatch);

    // With every check skipped, a backend that fails a different check each is not asked.
    for flaw in [
        Flaw::NoLastModified,
        Flaw::WholeObjectForRange,
        Flaw::NoMultipart,
    ] {
        let flawed = Flawed::new(flaw);
        let (outcome, _) = go(&flawed, &options("run-1", true, &Check::ALL)).await;
        assert_eq!(outcome, Ok(()));
    }
}

/// Asserts that probe keys of an earlier start that are older than ten minutes are deleted when
/// the test starts, and a key that is newer, and a key outside the prefix, are not.
#[tokio::test]
async fn probe_keys_of_an_earlier_start_older_than_ten_minutes_are_swept() {
    let clock = std::sync::Arc::new(TestClock::new());
    let memory = MemoryBackend::with_clock(clock.clone());
    let old = format!("{PROBE}killed-start/create");
    let recent = format!("{PROBE}running-start/create");
    memory.put(&old, Bytes::from_static(b"x")).await.unwrap();
    memory
        .put("client/object", Bytes::from_static(b"x"))
        .await
        .unwrap();
    clock.set(30 * 60 * 1000);
    memory.put(&recent, Bytes::from_static(b"x")).await.unwrap();
    let mut swept = options("run-1", true, &[]);
    swept.now = 35 * 60 * 1000;
    let (outcome, notes) = go(&memory, &swept).await;
    assert_eq!(outcome, Ok(()));
    assert!(notes.is_empty(), "{notes:?}");
    let all = memory.list("").await.unwrap();
    let keys: Vec<&str> = all.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(
        keys,
        ["_tiering/selftest/running-start/create", "client/object"]
    );
}

/// Asserts that a listing of the old probe keys that fails is noted and does not end the test, and
/// that a delete that fails is noted with the key, and does not end it either.
#[tokio::test]
async fn a_listing_or_a_delete_that_fails_is_noted_and_goes_on() {
    let listing = Flawed::new(Flaw::ListFails);
    let skip = [Check::Paging];
    let (outcome, notes) = go(&listing, &options("run-1", true, &skip)).await;
    assert_eq!(outcome, Ok(()));
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("could not list the probe keys of earlier starts"),
        "{notes:?}"
    );
    assert!(notes[0].contains("listing broke"), "{notes:?}");

    let deleting = Flawed::new(Flaw::DeleteFails);
    let (outcome, notes) = go(&deleting, &options("run-1", false, &[])).await;
    assert_eq!(outcome, Ok(()));
    let denied = notes
        .iter()
        .filter(|note| note.contains("could not delete the probe key"));
    assert!(denied.count() > 0, "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("_tiering/selftest/run-1/create"))
    );
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
            "head",
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
    assert_eq!(Check::parse("last-modified"), None);
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
    assert_eq!(
        SelfTestError::Violation(failure.clone()).to_string(),
        failure.to_string()
    );
}
