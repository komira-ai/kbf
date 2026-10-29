//! A self-test of a backend's contract, run before a backend is used.
//!
//! The proxy, the lease and the journal rest on answers that a store must give exactly: a
//! create-only put that refuses a key that exists, a put that replaces only on the right content
//! tag, a ranged read that gives the range and nothing else, a head that gives the size and the
//! tag of what was written, a listing that pages by `start_after`, and (on the tier that takes
//! multipart uploads) a completion that is create-only. A store that answers one of them wrongly
//! does not fail: it overwrites silently, and the guarantees built on it are gone.
//! [`self_test`] asks each question on a key of its own, and says which answer was wrong.
//!
//! Two outcomes are kept apart. A store that answered, and wrongly, is a [`SelfTestFailure`]
//! (a [`SelfTestError::Violation`]): fixing it takes a different store, so a restart is no use.
//! A store that could not be asked ([`Error::Unavailable`]: down, 503, a timeout, a refused
//! connection) is asked again, a bounded number of times, and then is a
//! [`SelfTestError::Unreachable`], which a restart may mend. A store that is full
//! ([`Error::Full`], a state the hot tier is designed to be in) is not in doubt: the checks that
//! need a write are skipped, with a note, and the test passes. Any other error is an answer the
//! contract does not allow, and a violation.
//!
//! The probe keys are under `_tiering/selftest/<run id>/`, a prefix a client never sees (the
//! `_tiering/` prefix is reserved), and the caller gives a run id that is new for each start.
//! The keys of a check are deleted after it, whether it passed or not (a delete that fails is
//! noted). Keys under the prefix that are older than ten minutes, left by a start that was killed,
//! are deleted when the test starts (at most the first thousand are looked at).
//!
//! The test sees a backend through the [`Backend`] trait, so it asks what the trait promises: the
//! S3 backend turns a store's answers into those promises (`If-None-Match: *` answered 412 is
//! [`Error::AlreadyExists`], a reply of 200 to a `Range` request is an error, a HEAD with no
//! `Last-Modified` is an error), and a store that breaks one shows up here as the error or the
//! wrong answer it gave. What the trait hides is not tested: a continuation token or
//! `IsTruncated` (the S3 backend follows tokens itself and cuts a page at `max_keys`, so a store
//! that ignores `max-keys` is not seen), and the value of the time of the last write. Of the
//! multipart calls only the create-only completion is tested, not the refusal of a bad part list.

use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;

use crate::{Backend, ByteRange, Error, Listed};

/// The prefix of the probe keys.
const PROBE_PREFIX: &str = "_tiering/selftest/";

/// The age, in milliseconds, past which a probe key of an earlier start is deleted.
const SWEEP_AGE_MS: u64 = 10 * 60 * 1000;

/// The most keys of an earlier start that are looked at.
const SWEEP_LIMIT: usize = 1000;

/// An entity tag that no object has, in the form of the tags of S3 (32 hex digits).
const WRONG_TAG: &str = "00000000000000000000000000000000";

/// The names of the probe keys, under the run's prefix.
const CREATE_KEY: &str = "create";
const HEAD_KEY: &str = "head";
const MATCH_KEY: &str = "match";
const RANGE_KEY: &str = "range";
const PAGE_KEYS: [&str; 3] = ["page/a", "page/b", "page/c"];
const MULTIPART_KEY: &str = "multipart";

/// What the object of the range check holds.
const DIGITS: &[u8] = b"0123456789";

/// One question of the self-test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// A create-only put of a key that exists is refused (`If-None-Match: *` answers 412).
    CreateOnly,
    /// A HEAD answers (for a store that means it carries `Last-Modified`), with the size and the
    /// tag of what was written.
    Head,
    /// A replace on a wrong content tag is refused, and one on the right tag takes effect
    /// (`If-Match`).
    IfMatch,
    /// A ranged read gives exactly the range (206 with the range, not 200 with the object).
    Range,
    /// A listing, whole or after a key (`start-after`), a page at a time, gives the right keys.
    Paging,
    /// The completion of a multipart upload of a key that exists is refused
    /// (`If-None-Match: *` on `CompleteMultipartUpload`). Run on the tier that takes multipart
    /// uploads only.
    MultipartConditional,
}

impl Check {
    /// Every check, in the order they run.
    pub const ALL: [Check; 6] = [
        Check::CreateOnly,
        Check::Head,
        Check::IfMatch,
        Check::Range,
        Check::Paging,
        Check::MultipartConditional,
    ];

    /// The name of the check, as a flag names it and a message gives it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Check::CreateOnly => "create-only",
            Check::Head => "head",
            Check::IfMatch => "if-match",
            Check::Range => "range",
            Check::Paging => "paging",
            Check::MultipartConditional => "multipart-conditional",
        }
    }

    /// The check with this name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Check> {
        Check::ALL.into_iter().find(|check| check.name() == name)
    }

    /// The probe keys the check writes.
    fn keys(self) -> &'static [&'static str] {
        match self {
            Check::CreateOnly => &[CREATE_KEY],
            Check::Head => &[HEAD_KEY],
            Check::IfMatch => &[MATCH_KEY],
            Check::Range => &[RANGE_KEY],
            Check::Paging => &PAGE_KEYS,
            Check::MultipartConditional => &[MULTIPART_KEY],
        }
    }
}

impl fmt::Display for Check {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())
    }
}

/// A backend that gave a wrong answer to a check: which backend, which check, and what it did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfTestFailure {
    /// The backend, as the caller named it.
    pub backend: String,
    /// The check it failed.
    pub check: Check,
    /// What the backend answered, and what it should have.
    pub observed: String,
}

impl fmt::Display for SelfTestFailure {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "refusing to start: {} failed the {} check: {}",
            self.backend, self.check, self.observed
        )
    }
}

impl std::error::Error for SelfTestFailure {}

/// Why the self-test of a backend did not pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelfTestError {
    /// The backend answered a check wrongly.
    Violation(SelfTestFailure),
    /// The backend could not be asked: it did not answer in every attempt.
    Unreachable {
        /// The backend, as the caller named it.
        backend: String,
        /// The last error, as text.
        error: String,
    },
}

impl fmt::Display for SelfTestError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelfTestError::Violation(failure) => failure.fmt(out),
            SelfTestError::Unreachable { backend, error } => {
                write!(out, "tier unreachable: {backend}: {error}")
            }
        }
    }
}

impl std::error::Error for SelfTestError {}

/// What one run of the self-test is told.
#[derive(Clone, Copy, Debug)]
pub struct SelfTestOptions<'a> {
    /// The backend, as messages call it.
    pub name: &'a str,
    /// An id that is new for each start, made of letters, digits, `-` and `_`: the probe keys
    /// are under it.
    pub run_id: &'a str,
    /// Whether the backend takes multipart uploads (the cold tier), and so whether
    /// [`Check::MultipartConditional`] runs.
    pub multipart: bool,
    /// Checks that do not run.
    pub skip: &'a [Check],
    /// How many times a check that could not reach the backend is run (at least one).
    pub attempts: u32,
    /// How long to wait between two attempts.
    pub pause: Duration,
    /// The time now, in milliseconds since the Unix epoch, to judge the age of the keys of an
    /// earlier start by.
    pub now: u64,
}

/// Where the notes of the test go: a skipped check, an attempt that is repeated, a key that was
/// not deleted.
pub type Note<'a> = &'a (dyn Fn(String) + Sync);

/// Runs the checks on `backend`. The first check that fails ends the test.
///
/// # Errors
/// [`SelfTestError::Violation`] for the first check the backend answered wrongly, and
/// [`SelfTestError::Unreachable`] if it could not be asked in any of the attempts.
pub async fn self_test(
    backend: &dyn Backend,
    options: &SelfTestOptions<'_>,
    note: Note<'_>,
) -> Result<(), SelfTestError> {
    let probe = Probe {
        backend,
        prefix: format!("{PROBE_PREFIX}{}/", options.run_id),
        options,
        note,
    };
    probe.sweep().await;
    for check in Check::ALL {
        let multipart_only = check == Check::MultipartConditional && !options.multipart;
        if options.skip.contains(&check) || multipart_only {
            continue;
        }
        let verdict = probe.attempt(check).await;
        probe.delete_keys(check).await;
        let backend = options.name.to_owned();
        match verdict {
            Ok(()) => {}
            Err(Fault::Full) => {
                probe.note(format!("the {check} check was skipped: the tier is full"));
            }
            Err(Fault::Wrong(observed)) => {
                let failure = SelfTestFailure {
                    backend,
                    check,
                    observed,
                };
                return Err(SelfTestError::Violation(failure));
            }
            Err(Fault::Down(error)) => return Err(SelfTestError::Unreachable { backend, error }),
        }
    }
    Ok(())
}

/// Why a check did not pass.
enum Fault {
    /// The backend answered, and wrongly: what it did.
    Wrong(String),
    /// The backend could not be asked: the error.
    Down(String),
    /// The backend has no room for the write the check needs.
    Full,
}

type Verdict = Result<(), Fault>;

/// A backend, the prefix of the keys the checks use on it, and what the test was told.
struct Probe<'a> {
    backend: &'a dyn Backend,
    prefix: String,
    options: &'a SelfTestOptions<'a>,
    note: Note<'a>,
}

/// The bytes of a text.
fn text(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// What an error of the call `what` means to the test.
fn fault(what: &str, error: Error) -> Fault {
    match error {
        Error::Full(_) => Fault::Full,
        Error::Unavailable(why) => Fault::Down(format!("{what}: {why}")),
        other => Fault::Wrong(format!("{what} failed with: {other}")),
    }
}

/// The answer of a call, or the fault its error is.
fn saw<T>(what: &str, answer: Result<T, Error>) -> Result<T, Fault> {
    answer.map_err(|error| fault(what, error))
}

/// Whether the call that must be refused was: only the two refusals of a precondition count.
fn refused(what: &str, answer: Result<(), Error>) -> Verdict {
    match answer {
        Err(Error::AlreadyExists(_) | Error::PreconditionFailed(_)) => Ok(()),
        Ok(()) => Err(Fault::Wrong(format!(
            "{what} was accepted; it must be refused with 412 (already exists)"
        ))),
        Err(other) => Err(fault(what, other)),
    }
}

/// Whether `got` is `want`.
fn expect_eq<T: PartialEq + fmt::Debug + ?Sized>(what: &str, got: &T, want: &T) -> Verdict {
    if got == want {
        return Ok(());
    }
    Err(Fault::Wrong(format!("{what} gave {got:?}, not {want:?}")))
}

/// The bytes as text, for a message and a comparison.
fn text_of(bytes: &Bytes) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The keys of a listing.
fn keys_of(listed: &[Listed]) -> Vec<String> {
    listed.iter().map(|object| object.key.clone()).collect()
}

impl Probe<'_> {
    /// The probe key `name`.
    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// Passes a line to the note, with the name of the backend.
    fn note(&self, line: String) {
        (self.note)(format!("{}: {line}", self.options.name));
    }

    /// Runs `check`, again if it could not reach the backend, up to the attempts given.
    async fn attempt(&self, check: Check) -> Verdict {
        let mut tried = 0;
        loop {
            tried += 1;
            self.delete_keys(check).await;
            let verdict = self.run(check).await;
            let Err(Fault::Down(why)) = &verdict else {
                return verdict;
            };
            if tried >= self.options.attempts {
                return verdict;
            }
            self.note(format!(
                "the {check} check could not reach the tier ({why}); trying again"
            ));
            tokio::time::sleep(self.options.pause).await;
        }
    }

    async fn run(&self, check: Check) -> Verdict {
        match check {
            Check::CreateOnly => self.create_only().await,
            Check::Head => self.head().await,
            Check::IfMatch => self.if_match().await,
            Check::Range => self.range().await,
            Check::Paging => self.paging().await,
            Check::MultipartConditional => self.multipart_conditional().await,
        }
    }

    /// Deletes a probe key. A delete that fails is noted, not an error: the key is in a prefix
    /// nothing reads, and the next start sweeps it.
    async fn delete(&self, key: &str) {
        if let Err(error) = self.backend.delete(key).await {
            self.note(format!("could not delete the probe key {key}: {error}"));
        }
    }

    /// Deletes the keys `check` writes.
    async fn delete_keys(&self, check: Check) {
        for name in check.keys() {
            self.delete(&self.key(name)).await;
        }
    }

    /// Deletes the probe keys that an earlier start left.
    async fn sweep(&self) {
        let listed = self.backend.list_page(PROBE_PREFIX, None, SWEEP_LIMIT);
        let found = match listed.await {
            Ok(found) => found,
            Err(error) => {
                let why = format!("could not list the probe keys of earlier starts: {error}");
                self.note(why);
                return;
            }
        };
        for object in found {
            if object.modified.saturating_add(SWEEP_AGE_MS) < self.options.now {
                self.delete(&object.key).await;
            }
        }
    }

    async fn create_only(&self) -> Verdict {
        let key = self.key(CREATE_KEY);
        let first = self.backend.put(&key, text("first")).await;
        saw("the first create-only put", first)?;
        let again = self.backend.put(&key, text("second")).await;
        refused("a second create-only put of the same key", again)
    }

    async fn head(&self) -> Verdict {
        let key = self.key(HEAD_KEY);
        saw("a put", self.backend.put(&key, text("head")).await)?;
        let what = "a HEAD (the S3 backend fails one that has no Last-Modified)";
        let first = saw(what, self.backend.head(&key).await)?;
        expect_eq("the size in a HEAD of a 4-byte object", &first.size, &4)?;
        expect_eq("an empty tag in a HEAD", &first.tag.is_empty(), &false)?;
        let body = text("headed");
        let replaced = self.backend.put_if_match(&key, body, &first.tag).await;
        let tag = saw("a replace on the tag of the HEAD", replaced)?;
        let second = saw(what, self.backend.head(&key).await)?;
        expect_eq("the size in a HEAD of a 6-byte object", &second.size, &6)?;
        expect_eq("the tag in a HEAD after a replace", &second.tag, &tag)
    }

    async fn if_match(&self) -> Verdict {
        let key = self.key(MATCH_KEY);
        saw("a put", self.backend.put(&key, text("v1")).await)?;
        let head = saw("a HEAD", self.backend.head(&key).await)?;
        let wrong = self.backend.put_if_match(&key, text("v2"), WRONG_TAG).await;
        refused("a replace on a wrong ETag", wrong.map(drop))?;
        let kept = saw("a GET", self.backend.get(&key, None).await)?;
        let what = "the object after the refused replace";
        expect_eq(what, text_of(&kept).as_str(), "v1")?;
        let right = self.backend.put_if_match(&key, text("v2"), &head.tag).await;
        saw("a replace on the right ETag", right)?;
        let replaced = saw("a GET", self.backend.get(&key, None).await)?;
        let what = "the object after the replace on the right ETag";
        expect_eq(what, text_of(&replaced).as_str(), "v2")
    }

    async fn range(&self) -> Verdict {
        let key = self.key(RANGE_KEY);
        let whole = Bytes::from_static(DIGITS);
        saw("a put", self.backend.put(&key, whole).await)?;
        let asked = ByteRange::new(2, 4);
        let got = saw("a ranged GET", self.backend.get(&key, Some(asked)).await)?;
        let what = "a ranged GET of bytes 2 to 5 of ten";
        expect_eq(what, text_of(&got).as_str(), "2345")
    }

    async fn paging(&self) -> Verdict {
        let keys: Vec<String> = PAGE_KEYS.into_iter().map(|name| self.key(name)).collect();
        for key in &keys {
            saw("a put", self.backend.put(key, text("page")).await)?;
        }
        let under = self.key("page/");
        let backend = self.backend;
        let whole = saw("a listing", backend.list(&under).await)?;
        let what = "a listing of the three keys";
        expect_eq(what, keys_of(&whole).as_slice(), keys.as_slice())?;
        let wide = backend.list_page(&under, None, 10).await;
        let wide = saw("a listing of up to ten keys", wide)?;
        let what = "a page of up to ten keys";
        expect_eq(what, keys_of(&wide).as_slice(), keys.as_slice())?;
        let first = backend.list_page(&under, None, 2).await;
        let first = saw("a listing of two keys", first)?;
        let what = "the first page of two";
        expect_eq(what, keys_of(&first).as_slice(), &keys[..2])?;
        let after_b = backend.list_page(&under, Some(&keys[1]), 2).await;
        let after_b = saw("a listing after the second key", after_b)?;
        let what = "the page after the second key";
        expect_eq(what, keys_of(&after_b).as_slice(), &keys[2..])?;
        let after_a = backend.list_page(&under, Some(&keys[0]), 1).await;
        let after_a = saw("a listing of one key after the first key", after_a)?;
        let what = "the page of one after the first key";
        expect_eq(what, keys_of(&after_a).as_slice(), &keys[1..2])
    }

    async fn multipart_conditional(&self) -> Verdict {
        let key = self.key(MULTIPART_KEY);
        saw("a put", self.backend.put(&key, text("existing")).await)?;
        let created = self.backend.create_upload(&key).await;
        let id = saw("starting a multipart upload", created)?;
        let outcome = self.complete_over_existing(&key, &id).await;
        let _ = self.backend.abort_upload(&key, &id).await;
        outcome
    }

    /// Uploads one part to the upload `id` of `key` and completes it, which must be refused as
    /// `key` exists.
    async fn complete_over_existing(&self, key: &str, id: &str) -> Verdict {
        let part = Bytes::from_static(b"part");
        let len = part.len() as u64;
        let body = stream::iter([Ok(part)]).boxed();
        let sent = self
            .backend
            .upload_part_with_len(key, id, 1, Some(len), body);
        let tag = saw("uploading a part", sent.await)?;
        let done = self.backend.complete_upload(key, id, &[(1, tag)]).await;
        refused(
            "the completion of a multipart upload of a key that exists",
            done.map(drop),
        )
    }
}
