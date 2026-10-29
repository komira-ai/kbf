//! A self-test of a backend's contract, run before a backend is used.
//!
//! The proxy, the lease and the journal rest on answers that a store must give exactly: a
//! create-only put that refuses a key that exists, a put that replaces only on the right content
//! tag, a ranged read that gives the range and nothing else, a head that carries the time of the
//! last write, a listing that pages by `start_after`, and (on the tier that takes multipart
//! uploads) a completion that is create-only. A store that answers one of them wrongly does not
//! fail: it overwrites silently, and the guarantees built on it are gone. [`self_test`] asks
//! each question once, on a key of its own, and says which answer was wrong.
//!
//! The probe keys are under `_tiering/selftest/<run id>/`, a prefix a client never sees (the
//! `_tiering/` prefix is reserved), and the caller gives a run id that is new for each start.
//! Every probe key is deleted when the test ends, whether it passed or not.
//!
//! The test sees a backend through the [`Backend`] trait, so it asks what the trait promises:
//! the S3 backend turns a store's answers into those promises (`If-None-Match: *` answered 412
//! is [`Error::AlreadyExists`], a reply of 200 to a `Range` request is an error, a HEAD with no
//! `Last-Modified` is an error), and a store that breaks one shows up here as the error or the
//! wrong bytes it gave.

use std::fmt;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;

use crate::{Backend, ByteRange, Error, Listed};

/// The prefix of the probe keys.
const PROBE_PREFIX: &str = "_tiering/selftest/";

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
    /// A HEAD answers, which for a store means it carries `Last-Modified`.
    LastModified,
    /// A replace on a wrong content tag is refused, and one on the right tag takes effect
    /// (`If-Match`).
    IfMatch,
    /// A ranged read gives exactly the range (206 with the range, not 200 with the object).
    Range,
    /// A listing after a key (`start-after`), a page at a time, gives the right keys.
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
        Check::LastModified,
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
            Check::LastModified => "last-modified",
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

/// Runs the checks on `backend`, which the caller calls `name` in messages.
///
/// The probe keys are under a prefix that `run_id` makes: use an id that is new for each start,
/// made of letters, digits, `-` and `_`. `multipart` says whether the backend takes multipart
/// uploads (the cold tier), and so whether [`Check::MultipartConditional`] runs. A check in
/// `skip` does not run. The first check that fails ends the test.
///
/// # Errors
/// [`SelfTestFailure`] for the first check the backend answered wrongly.
pub async fn self_test(
    name: &str,
    backend: &dyn Backend,
    run_id: &str,
    multipart: bool,
    skip: &[Check],
) -> Result<(), SelfTestFailure> {
    let probe = Probe {
        backend,
        prefix: format!("{PROBE_PREFIX}{run_id}/"),
    };
    let mut outcome = Ok(());
    for check in Check::ALL {
        let multipart_only = check == Check::MultipartConditional && !multipart;
        if skip.contains(&check) || multipart_only {
            continue;
        }
        outcome = probe.run(check).await.map_err(|observed| SelfTestFailure {
            backend: name.to_owned(),
            check,
            observed,
        });
        if outcome.is_err() {
            break;
        }
    }
    probe.clean_up().await;
    outcome
}

/// A backend and the prefix of the keys the checks use on it.
struct Probe<'a> {
    backend: &'a dyn Backend,
    prefix: String,
}

/// The bytes of a text.
fn text(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// The answer of a call, or what went wrong with `what` as the text of a failure.
fn saw<T>(what: &str, answer: Result<T, Error>) -> Result<T, String> {
    answer.map_err(|error| format!("{what} failed with: {error}"))
}

/// Whether the call that must be refused was: only the two refusals of a precondition count.
fn refused(what: &str, answer: Result<(), Error>) -> Result<(), String> {
    match answer {
        Err(Error::AlreadyExists(_) | Error::PreconditionFailed(_)) => Ok(()),
        Ok(()) => Err(format!(
            "{what} was accepted; it must be refused with 412 (already exists)"
        )),
        Err(other) => Err(format!(
            "{what} was answered with {other}; it must be refused with 412 (already exists)"
        )),
    }
}

/// Whether `got` is `want`.
fn same_bytes(what: &str, got: &Bytes, want: &[u8]) -> Result<(), String> {
    if got.as_ref() == want {
        return Ok(());
    }
    let (got, want) = (String::from_utf8_lossy(got), String::from_utf8_lossy(want));
    Err(format!("{what} gave {got:?}, not {want:?}"))
}

/// Whether the keys of `got` are `want`.
fn same_keys(what: &str, got: &[Listed], want: &[String]) -> Result<(), String> {
    let keys: Vec<String> = got.iter().map(|object| object.key.clone()).collect();
    if keys == want {
        return Ok(());
    }
    Err(format!("{what} gave {keys:?}, not {want:?}"))
}

impl Probe<'_> {
    /// The probe key `name`.
    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    async fn run(&self, check: Check) -> Result<(), String> {
        match check {
            Check::CreateOnly => self.create_only().await,
            Check::LastModified => self.last_modified().await,
            Check::IfMatch => self.if_match().await,
            Check::Range => self.range().await,
            Check::Paging => self.paging().await,
            Check::MultipartConditional => self.multipart_conditional().await,
        }
    }

    /// Deletes every probe key. A delete that fails is passed over: the keys are in a prefix
    /// nothing reads, and a store that cannot delete them has bigger trouble to report.
    async fn clean_up(&self) {
        let names = [CREATE_KEY, HEAD_KEY, MATCH_KEY, RANGE_KEY, MULTIPART_KEY];
        for name in names.into_iter().chain(PAGE_KEYS) {
            let _ = self.backend.delete(&self.key(name)).await;
        }
    }

    async fn create_only(&self) -> Result<(), String> {
        let key = self.key(CREATE_KEY);
        saw(
            "the first create-only put",
            self.backend.put(&key, text("first")).await,
        )?;
        let again = self.backend.put(&key, text("second")).await;
        refused("a second create-only put of the same key", again)
    }

    async fn last_modified(&self) -> Result<(), String> {
        let key = self.key(HEAD_KEY);
        saw("a put", self.backend.put(&key, text("head")).await)?;
        let head = self.backend.head(&key).await;
        saw("a HEAD (it needs the Last-Modified header)", head).map(drop)
    }

    async fn if_match(&self) -> Result<(), String> {
        let key = self.key(MATCH_KEY);
        saw("a put", self.backend.put(&key, text("v1")).await)?;
        let head = saw("a HEAD", self.backend.head(&key).await)?;
        let wrong = self.backend.put_if_match(&key, text("v2"), WRONG_TAG).await;
        refused("a replace on a wrong ETag", wrong.map(drop))?;
        let kept = saw("a GET", self.backend.get(&key, None).await)?;
        same_bytes("the object after the refused replace", &kept, b"v1")?;
        let right = self.backend.put_if_match(&key, text("v2"), &head.tag).await;
        saw("a replace on the right ETag", right)?;
        let replaced = saw("a GET", self.backend.get(&key, None).await)?;
        same_bytes(
            "the object after the replace on the right ETag",
            &replaced,
            b"v2",
        )
    }

    async fn range(&self) -> Result<(), String> {
        let key = self.key(RANGE_KEY);
        let whole = Bytes::from_static(DIGITS);
        saw("a put", self.backend.put(&key, whole).await)?;
        let asked = ByteRange::new(2, 4);
        let got = saw("a ranged GET", self.backend.get(&key, Some(asked)).await)?;
        same_bytes("a ranged GET of bytes 2 to 5 of ten", &got, b"2345")
    }

    async fn paging(&self) -> Result<(), String> {
        let keys: Vec<String> = PAGE_KEYS.into_iter().map(|name| self.key(name)).collect();
        for key in &keys {
            saw("a put", self.backend.put(key, text("page")).await)?;
        }
        let under = self.key("page/");
        let backend = self.backend;
        let first = backend.list_page(&under, None, 2).await;
        let first = saw("a listing of two keys", first)?;
        same_keys("the first page of two", &first, &keys[..2])?;
        let after_b = backend.list_page(&under, Some(&keys[1]), 2).await;
        let after_b = saw("a listing after the second key", after_b)?;
        same_keys("the page after the second key", &after_b, &keys[2..])?;
        let after_a = backend.list_page(&under, Some(&keys[0]), 1).await;
        let after_a = saw("a listing of one key after the first key", after_a)?;
        same_keys("the page of one after the first key", &after_a, &keys[1..2])
    }

    async fn multipart_conditional(&self) -> Result<(), String> {
        let key = self.key(MULTIPART_KEY);
        saw("a put", self.backend.put(&key, text("existing")).await)?;
        let id = saw(
            "starting a multipart upload",
            self.backend.create_upload(&key).await,
        )?;
        let outcome = self.complete_over_existing(&key, &id).await;
        let _ = self.backend.abort_upload(&key, &id).await;
        outcome
    }

    /// Uploads one part to the upload `id` of `key` and completes it, which must be refused as
    /// `key` exists.
    async fn complete_over_existing(&self, key: &str, id: &str) -> Result<(), String> {
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
