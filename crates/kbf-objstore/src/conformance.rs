//! The conformance suite: what any [`ObjectStore`] must do before kbf trusts it.
//!
//! [`run`] drives every check against one store under a scratch prefix and returns a
//! [`Report`]; it never panics on a misbehaving store. It checks the base contract
//! (round trip, ranged reads at the boundaries, a missing key, idempotent delete, list
//! paging) and every claim in [`Capabilities`]: a store claiming conditional writes must
//! refuse an overwrite, a store claiming Object Lock must refuse deleting a retained
//! object and keep it readable, and a store not claiming Object Lock must refuse a
//! retention date rather than store the object unlocked.
//!
//! Objects written under Object Lock cannot be removed until their date (one hour), so
//! give each run a fresh prefix when running against a real store.

use std::collections::BTreeSet;
use std::fmt;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::StreamExt;

use crate::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError,
    PageSize,
};

/// How long the Object Lock check retains its object.
const LOCK_FOR: Duration = Duration::from_secs(3600);
/// Objects the list check writes; with pages of [`LIST_PAGE`], the last page is short.
const LIST_OBJECTS: usize = 7;
const LIST_PAGE: u16 = 3;

/// One check in the suite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// `put_new` then reading the whole object returns the same bytes.
    RoundTrip,
    /// Ranged reads at the first byte, the last byte, across the end and past the end.
    RangeBoundaries,
    /// Reading an absent key is `NotFound`.
    MissingObject,
    /// Delete removes the object; deleting it again, or a key never written, succeeds.
    DeleteIdempotent,
    /// Paged listing returns every key under the prefix once, in order, in pages no
    /// larger than asked, and nothing outside the prefix.
    ListPagination,
    /// With conditional writes, `put_new` refuses an existing key and keeps its bytes.
    PutNewRefusesOverwrite,
    /// With Object Lock, a retained object refuses delete and stays readable; without
    /// it, a retention date is refused.
    ObjectLock,
    /// `put_stream` of an object of several parts, its body in pieces that do not line
    /// up with them, reads back whole; a body that fails, or ends short, stores
    /// nothing; with conditional writes, an existing key is refused and kept.
    StreamedPut,
}

/// The outcome of one check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The store did what the check requires.
    Passed,
    /// The check does not apply: the store does not claim the capability it tests.
    Skipped(&'static str),
    /// The store did something else; the text says what.
    Failed(String),
}

/// The outcome of every check, in the order they ran.
#[derive(Clone, Debug)]
pub struct Report {
    /// One entry per [`Check`].
    pub results: Vec<(Check, Outcome)>,
}

impl Report {
    /// Whether no check failed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures().next().is_none()
    }

    /// The checks that failed, with what went wrong.
    pub fn failures(&self) -> impl Iterator<Item = (Check, &str)> {
        self.results.iter().filter_map(|(c, o)| match o {
            Outcome::Failed(why) => Some((*c, why.as_str())),
            _ => None,
        })
    }

    /// The checks that were skipped.
    pub fn skipped(&self) -> impl Iterator<Item = Check> {
        self.results
            .iter()
            .filter_map(|(c, o)| matches!(o, Outcome::Skipped(_)).then_some(*c))
    }

    /// The outcome of `check`.
    #[must_use]
    pub fn outcome(&self, check: Check) -> Option<&Outcome> {
        self.results
            .iter()
            .find(|(c, _)| *c == check)
            .map(|(_, o)| o)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (check, outcome) in &self.results {
            match outcome {
                Outcome::Passed => writeln!(f, "pass {check:?}")?,
                Outcome::Skipped(why) => writeln!(f, "skip {check:?}: {why}")?,
                Outcome::Failed(why) => writeln!(f, "FAIL {check:?}: {why}")?,
            }
        }
        Ok(())
    }
}

/// Runs every check against `store`, writing only under `prefix`.
pub async fn run<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Report {
    let caps = store.capabilities();
    let mut results = Vec::new();
    let mut record = |check, r: Result<Outcome, String>| {
        results.push((check, r.unwrap_or_else(Outcome::Failed)));
    };
    record(Check::RoundTrip, round_trip(store, prefix).await);
    record(
        Check::RangeBoundaries,
        range_boundaries(store, prefix).await,
    );
    record(Check::MissingObject, missing_object(store, prefix).await);
    record(
        Check::DeleteIdempotent,
        delete_idempotent(store, prefix).await,
    );
    record(Check::ListPagination, list_pagination(store, prefix).await);
    record(
        Check::PutNewRefusesOverwrite,
        refuses_overwrite(store, prefix, caps).await,
    );
    record(Check::ObjectLock, object_lock(store, prefix, caps).await);
    record(Check::StreamedPut, streamed_put(store, prefix, caps).await);
    Report { results }
}

type CheckResult = Result<Outcome, String>;

macro_rules! ensure {
    ($cond:expr, $($why:tt)+) => {
        if !$cond {
            return Err(format!($($why)+));
        }
    };
}

fn key(prefix: &KeyPrefix, name: &str) -> Result<ObjectKey, String> {
    prefix
        .key(name)
        .map_err(|e| format!("scratch key {name}: {e}"))
}

fn range(offset: u64, len: u64) -> ByteRange {
    ByteRange::new(offset, len).expect("the suite's ranges are non-empty and small")
}

/// Bytes whose value at each offset differs from its neighbours', so a read at the
/// wrong offset or of the wrong length cannot match by accident.
fn pattern(len: usize, seed: u8) -> Bytes {
    (0..len)
        .map(|i| (i.wrapping_mul(31) as u8) ^ seed ^ ((i >> 8) as u8))
        .collect::<Vec<u8>>()
        .into()
}

async fn put<S: ObjectStore>(store: &S, key: &ObjectKey, body: &Bytes) -> Result<(), String> {
    store
        .put_new(key, body.clone(), None)
        .await
        .map_err(|e| format!("put_new {key}: {e}"))
}

async fn read<S: ObjectStore>(store: &S, key: &ObjectKey, r: ByteRange) -> Result<Bytes, String> {
    store
        .get_range(key, r)
        .await
        .map_err(|e| format!("get_range {key} {r:?}: {e}"))
}

async fn round_trip<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    for (name, len) in [("round-trip-1", 1), ("round-trip-big", 300_000)] {
        let k = key(prefix, name)?;
        let body = pattern(len, 7);
        put(store, &k, &body).await?;
        let got = read(store, &k, range(0, len as u64)).await?;
        ensure!(
            got == body,
            "{k}: read {} bytes that differ from the {len} written",
            got.len()
        );
    }
    Ok(Outcome::Passed)
}

async fn range_boundaries<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    const SIZE: u64 = 1000;
    let k = key(prefix, "ranges")?;
    let body = pattern(SIZE as usize, 3);
    put(store, &k, &body).await?;
    // (offset, len, the bytes the store must return)
    let cases = [
        (0, 1, 0..1),
        (SIZE - 1, 1, 999..1000),
        (1, 10, 1..11),
        (500, 100, 500..600),
        (0, SIZE, 0..1000),
        (990, 100, 990..1000), // runs past the end: cut at the end
        (0, SIZE + 5, 0..1000),
    ];
    for (offset, len, want) in cases {
        let r = range(offset, len);
        let got = read(store, &k, r).await?;
        let want = body.slice(want);
        ensure!(
            got == want,
            "{r:?} of a {SIZE}-byte object returned {} bytes, expected {} (bytes {}..{})",
            got.len(),
            want.len(),
            offset,
            offset + want.len() as u64,
        );
    }
    for offset in [SIZE, SIZE + 1] {
        match store.get_range(&k, range(offset, 1)).await {
            Err(ObjectStoreError::InvalidRange { .. }) => {}
            other => {
                return Err(format!(
                    "a range starting at {offset} of a {SIZE}-byte object: expected InvalidRange, got {}",
                    describe(&other)
                ));
            }
        }
    }
    Ok(Outcome::Passed)
}

async fn missing_object<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let k = key(prefix, "never-written")?;
    match store.get_range(&k, range(0, 1)).await {
        Err(ObjectStoreError::NotFound(_)) => Ok(Outcome::Passed),
        other => Err(format!(
            "reading {k}, never written: expected NotFound, got {}",
            describe(&other)
        )),
    }
}

async fn delete_idempotent<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let k = key(prefix, "deleted")?;
    put(store, &k, &pattern(10, 1)).await?;
    for attempt in ["first", "second"] {
        store
            .delete(&k)
            .await
            .map_err(|e| format!("{attempt} delete of {k}: {e}"))?;
        match store.get_range(&k, range(0, 1)).await {
            Err(ObjectStoreError::NotFound(_)) => {}
            other => {
                return Err(format!(
                    "{k} after the {attempt} delete: expected NotFound, got {}",
                    describe(&other)
                ));
            }
        }
    }
    let never = key(prefix, "never-deleted")?;
    store
        .delete(&never)
        .await
        .map_err(|e| format!("deleting {never}, never written: {e}"))?;
    Ok(Outcome::Passed)
}

async fn list_pagination<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let dir = prefix
        .child("list/")
        .map_err(|e| format!("list prefix: {e}"))?;
    let mut want = BTreeSet::new();
    for i in 0..LIST_OBJECTS {
        let k = key(&dir, &format!("k{i}"))?;
        put(store, &k, &pattern(i + 1, 9)).await?;
        want.insert((k, i as u64 + 1));
    }
    // Shares the text `<prefix>list` but not `<prefix>list/`: must not be listed.
    put(store, &key(prefix, "list-outside")?, &pattern(1, 0)).await?;

    let page_size = PageSize::new(LIST_PAGE).expect("in range");
    let mut seen = Vec::new();
    let mut token = None;
    let mut pages = 0;
    loop {
        pages += 1;
        ensure!(
            pages <= LIST_OBJECTS + 1,
            "list did not end after {pages} pages"
        );
        let page = store
            .list(&dir, token.as_ref(), page_size)
            .await
            .map_err(|e| format!("list page {pages}: {e}"))?;
        ensure!(
            page.objects.len() <= usize::from(LIST_PAGE),
            "page {pages} has {} keys; at most {LIST_PAGE} were asked for",
            page.objects.len()
        );
        seen.extend(page.objects.into_iter().map(|o| (o.key, o.size)));
        match page.next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    let keys: Vec<&ObjectKey> = seen.iter().map(|(k, _)| k).collect();
    ensure!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "listed keys are not strictly ascending (out of order or repeated): {keys:?}"
    );
    let got: BTreeSet<_> = seen.into_iter().collect();
    let missing: Vec<_> = want.difference(&got).map(|(k, _)| k.as_str()).collect();
    let extra: Vec<_> = got.difference(&want).map(|(k, _)| k.as_str()).collect();
    ensure!(
        missing.is_empty() && extra.is_empty(),
        "listing {dir:?} in pages of {LIST_PAGE}: missing {missing:?}, unexpected {extra:?} (sizes included)"
    );
    let min_pages = LIST_OBJECTS.div_ceil(usize::from(LIST_PAGE));
    ensure!(
        pages >= min_pages,
        "{LIST_OBJECTS} keys came in {pages} pages of at most {LIST_PAGE}"
    );
    Ok(Outcome::Passed)
}

async fn refuses_overwrite<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    caps: Capabilities,
) -> CheckResult {
    if !caps.conditional_put {
        return Ok(Outcome::Skipped(
            "the store does not claim conditional writes",
        ));
    }
    let k = key(prefix, "put-once")?;
    let first = pattern(64, 1);
    put(store, &k, &first).await?;
    match store.put_new(&k, pattern(64, 2), None).await {
        Err(ObjectStoreError::AlreadyExists(_)) => {}
        other => {
            return Err(format!(
                "a second put_new of {k}: expected AlreadyExists, got {}",
                describe(&other)
            ));
        }
    }
    let got = read(store, &k, range(0, 64)).await?;
    ensure!(got == first, "{k} changed after a refused overwrite");
    Ok(Outcome::Passed)
}

async fn object_lock<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    caps: Capabilities,
) -> CheckResult {
    let k = key(prefix, "locked")?;
    let body = pattern(32, 5);
    let until = SystemTime::now() + LOCK_FOR;
    if !caps.object_lock {
        return match store.put_new(&k, body, Some(until)).await {
            Err(ObjectStoreError::Unsupported(_)) => Ok(Outcome::Passed),
            other => Err(format!(
                "a retention date on a store without Object Lock: expected Unsupported, got {}",
                describe(&other)
            )),
        };
    }
    store
        .put_new(&k, body.clone(), Some(until))
        .await
        .map_err(|e| format!("put_new {k} with retention: {e}"))?;
    if store.delete(&k).await.is_ok() {
        // A success is a failure whatever the bytes did: kbf would believe them gone.
        // A plain DELETE on a versioned bucket succeeds by adding a delete marker, so the
        // object stops reading while an old version may keep the bytes.
        return Err(match store.get_range(&k, range(0, 32)).await {
            Ok(b) if b == body => format!(
                "delete of retained {k} reported success (the bytes survive, but kbf would believe them gone)"
            ),
            other => format!(
                "delete of retained {k} succeeded and the object no longer reads (removed, or hidden by a delete marker): {}",
                describe(&other)
            ),
        });
    }
    let got = read(store, &k, range(0, 32)).await?;
    ensure!(got == body, "{k} changed after a refused delete");
    Ok(Outcome::Passed)
}

fn describe<T>(r: &Result<T, ObjectStoreError>) -> String {
    match r {
        Ok(_) => "success".to_owned(),
        Err(e) => format!("error: {e}"),
    }
}

/// The length of the streamed object: three S3 parts, the last short.
const STREAMED_LEN: usize = 2 * crate::s3::PART_BYTES as usize + 12_345;

/// The pieces a streamed body comes in: not a divisor of the part size.
const STREAMED_PIECE: usize = (1 << 20) + 13;

/// `bytes` as a body of `STREAMED_PIECE`-byte pieces, then `tail` (an error) if given.
fn streamed(bytes: &Bytes, tail: Option<ObjectStoreError>) -> BodyStream {
    let mut pieces: Vec<Result<Bytes, ObjectStoreError>> = (0..bytes.len())
        .step_by(STREAMED_PIECE)
        .map(|at| Ok(bytes.slice(at..(at + STREAMED_PIECE).min(bytes.len()))))
        .collect();
    pieces.extend(tail.map(Err));
    futures::stream::iter(pieces).boxed()
}

async fn streamed_put<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    caps: Capabilities,
) -> CheckResult {
    let body = pattern(STREAMED_LEN, 11);
    let len = body.len() as u64;
    let k = key(prefix, "streamed")?;
    store
        .put_stream(&k, len, streamed(&body, None))
        .await
        .map_err(|e| format!("put_stream {k}: {e}"))?;
    const WINDOW: usize = 4 << 20;
    for at in (0..body.len()).step_by(WINDOW) {
        let n = WINDOW.min(body.len() - at);
        let got = read(store, &k, range(at as u64, n as u64)).await?;
        ensure!(
            got == body.slice(at..at + n),
            "{k}: bytes {at}..{} read back other than streamed",
            at + n
        );
    }
    let failed = key(prefix, "streamed-failed")?;
    let broke = ObjectStoreError::Body("the source broke".into());
    let wrote = store
        .put_stream(&failed, len, streamed(&body, Some(broke)))
        .await;
    ensure!(wrote.is_err(), "{failed}: a body that failed was written");
    absent(store, &failed).await?;
    let short = key(prefix, "streamed-short")?;
    let wrote = store
        .put_stream(&short, len + 1, streamed(&body, None))
        .await;
    ensure!(
        matches!(wrote, Err(ObjectStoreError::Body(_))),
        "{short}: a body one byte short answered {wrote:?}"
    );
    absent(store, &short).await?;
    if !caps.conditional_put {
        return Ok(Outcome::Passed);
    }
    let other = pattern(STREAMED_LEN, 12);
    match store.put_stream(&k, len, streamed(&other, None)).await {
        Err(ObjectStoreError::AlreadyExists(_)) => {}
        other => return Err(format!("{k}: streamed overwrite answered {other:?}")),
    }
    let kept = read(store, &k, range(0, 64)).await?;
    ensure!(
        kept == body.slice(..64),
        "{k}: a refused streamed overwrite changed it"
    );
    Ok(Outcome::Passed)
}

/// Fails unless reading `k` finds no object.
async fn absent<S: ObjectStore>(store: &S, k: &ObjectKey) -> Result<(), String> {
    match store.get_range(k, range(0, 1)).await {
        Err(ObjectStoreError::NotFound(_)) => Ok(()),
        other => Err(format!("{k}: a write that failed left {other:?}")),
    }
}
