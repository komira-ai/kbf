//! Storing the pack: one create-only HTTP PUT of its bytes to a presigned URL.
//!
//! The URL is taken as given and is a credential (its query string carries the signature), so it
//! is never printed: every error, and every message a lower layer produced, names the URL only
//! as [`redact`] renders it. The PUT carries `Content-Length` and `If-None-Match: *`, so the
//! store refuses it when the key already holds an object.
//!
//! One answer is retried: 503, which the store gives when its hot tier is full or it is taking as
//! many puts as it may, and nothing was stored ([`put_retrying`]): up to [`MAX_RETRIES`] more
//! times, each after the store's `Retry-After` moved by a random share of -50% to +50%
//! ([`Jitter`]), so that tasks that were refused together do not come back together, and
//! stopping when the waits have added up to [`MAX_TOTAL_WAIT`]. The same retry follows a
//! connection that broke after it was made ([`PutError::Interrupted`]), which is what a client
//! that is still sending a large body sees when the store answers 503 and closes without reading
//! it. Every other answer, a 412 (the key exists, and a re-attempt of the task gets a new key, so
//! it is final), 429, 500, 502 and 504 (deliberately not retried) and any other transport error
//! included, is reported on the first attempt.
//!
//! [`put_file`] is the same PUT for a large object: the body is streamed from a file, never read
//! whole, and the file is reopened by each attempt of [`put_retrying`]. [`put_reader`] is the same
//! PUT for a body that is held open rather than named by a path.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// How long the tool waits to reach the store.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the whole PUT may take, so a store that stalls cannot hold the tool.
const PUT_TIMEOUT: Duration = Duration::from_secs(900);

/// How many times a 503 is retried after the first attempt.
pub const MAX_RETRIES: u32 = 7;

/// The longest `Retry-After` the tool honours before one retry, however long the store asks; the
/// wait is that moved by the jitter, so it can be half as long or half as long again.
pub const MAX_WAIT: Duration = Duration::from_secs(30);

/// The most the waits before the retries of one PUT add up to: when they have, the 503 is the
/// failure, even if retries are left, and the wait that reaches it is cut short to end there.
pub const MAX_TOTAL_WAIT: Duration = Duration::from_secs(60);

/// How long the tool waits before a retry when the 503 names no usable `Retry-After`.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(2);

/// The largest large object: a blob over the record cap
/// ([`kbf_task_pack::MAX_RECORD_BYTES`]) is PUT alone, as raw bytes, and this is the most such a
/// PUT may carry. It covers the largest toolchain blob (about 165 MiB today) with room for the
/// next toolchain bump, and equals the cap the store and the frontend enforce on the same key
/// (the frontend's `--task-large-max-bytes`); a value that differs from theirs makes a task fail
/// at the PUT instead of at the plan.
pub const MAX_LARGE_BYTES: u64 = 512 << 20;

/// The string that stands in for a URL's query string and fragment.
const REDACTED: &str = "<redacted>";

/// Why the pack was not stored.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum PutError {
    /// The store answered 412: the key already holds an object.
    #[error("the object already exists at {url}")]
    Exists {
        /// The URL, redacted.
        url: String,
    },
    /// The store answered 412 to a retry that followed an attempt of the same PUT whose
    /// connection broke ([`PutError::Interrupted`]): the object may be the one that attempt
    /// stored, or another writer's; the task cannot tell which. It is a failure like
    /// [`PutError::Exists`] (same exit code); the message is distinct so that it can be counted.
    #[error(
        "412 after an interrupted attempt of this put; the earlier attempt may have stored it: the object already exists at {url}"
    )]
    ExistsAfterInterrupt {
        /// The URL, redacted.
        url: String,
    },
    /// The store answered 503: it had no room, and nothing was stored.
    #[error("the store answered 503 to the PUT of {url}{}", retry_text(.retry_after_secs))]
    Unavailable {
        /// The URL, redacted.
        url: String,
        /// The `Retry-After` the store gave, in seconds; `None` when the header is absent or is
        /// not a whole number of seconds (an HTTP date is not read).
        retry_after_secs: Option<u64>,
    },
    /// The store answered with a status other than 200, 201, 412 or 503.
    #[error("the store answered {status} to the PUT of {url}")]
    Status {
        /// The HTTP status.
        status: u16,
        /// The URL, redacted.
        url: String,
    },
    /// The file to send is larger than the cap for a large object; nothing was sent.
    #[error("the file has {size} bytes, more than the {limit} a large object may hold")]
    TooLarge {
        /// The file's size.
        size: u64,
        /// The cap.
        limit: u64,
    },
    /// The file to send could not be opened or measured; nothing was sent.
    #[error("{path}: {reason}")]
    File {
        /// The file.
        path: String,
        /// The operating system's reason.
        reason: String,
    },
    /// The request did not complete: the store was not reached, or the connection failed.
    #[error("the PUT of {url} failed: {reason}")]
    Transport {
        /// The URL, redacted.
        url: String,
        /// What went wrong, with the URL's secret parts removed.
        reason: String,
    },
    /// The connection was reset or aborted, or its writing end was closed, after it was made. A
    /// store that refuses a PUT with a 503 before it reads the body (tierstore does when it is
    /// taking as many puts as it may) closes the connection while the client is still sending a
    /// body that does not fit the socket buffers, and the client then sees this and not the 503. [`put_retrying`] treats it as a 503 with no `Retry-After`.
    #[error("the connection of the PUT of {url} broke before an answer came: {reason}")]
    Interrupted {
        /// The URL, redacted.
        url: String,
        /// What went wrong, with the URL's secret parts removed.
        reason: String,
    },
}

/// The error of a request that did not complete: [`PutError::Interrupted`] for an I/O error of
/// kind reset, abort or broken pipe, [`PutError::Transport`] for any other (a store not reached,
/// a bad URL, a timeout, a protocol error, a clean close before any answer). The classification
/// is by the kind of the error alone, so a reset while the connection was being made is
/// `Interrupted` too; retrying that is harmless, since nothing was sent. On macOS, an I/O error
/// whose OS error is `EINVAL` or `EPROTOTYPE` is `Interrupted` as well: a socket the peer has
/// reset refuses `setsockopt` (ureq sets the write and read timeout between the chunks of a
/// body) with `EINVAL`, and a write to it can fail with `EPROTOTYPE`, where Linux reports a
/// reset or a broken pipe. A genuine `EINVAL` on macOS is therefore retried to the bound of
/// [`put_retrying`] too, and then reported as `Interrupted`. Other systems do not widen it.
fn transport_error(error: &ureq::Error, url: &str) -> PutError {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset};
    let (url, reason) = (redact(url), scrub(&error.to_string(), url));
    match error {
        // `|`, not `||`: the macOS codes are empty elsewhere, and a short circuit would leave a
        // branch that no other system can take.
        ureq::Error::Io(e)
            if matches!(e.kind(), BrokenPipe | ConnectionAborted | ConnectionReset)
                | is_macos_reset_symptom(e) =>
        {
            PutError::Interrupted { url, reason }
        }
        _ => PutError::Transport { url, reason },
    }
}

/// The OS errors macOS gives for an operation on a socket the peer has reset: `EINVAL` from
/// `setsockopt`, `EPROTOTYPE` from a write.
#[cfg(target_os = "macos")]
const MACOS_RESET_CODES: &[i32] = &[libc::EINVAL, libc::EPROTOTYPE];

/// None on other systems.
#[cfg(not(target_os = "macos"))]
const MACOS_RESET_CODES: &[i32] = &[];

/// Whether `e` is what macOS gives for an operation on a socket the peer has reset. Always false
/// on other systems.
fn is_macos_reset_symptom(e: &std::io::Error) -> bool {
    e.raw_os_error()
        .is_some_and(|code| MACOS_RESET_CODES.contains(&code))
}

/// The part of a 503's message that tells what the store asked for.
fn retry_text(secs: &Option<u64>) -> String {
    secs.map_or_else(String::new, |s| format!(" (retry after {s}s)"))
}

/// `url` with its user information, query string and fragment removed: the credentials before an
/// `@` in the authority are dropped, and a query or fragment is replaced by `<redacted>`. Only the
/// part before the first `?` or `#` is kept.
pub fn redact(url: &str) -> String {
    let (head, secret) = match url.find(['?', '#']) {
        Some(at) => (&url[..at], true),
        None => (url, false),
    };
    let mut out = head.to_string();
    if let Some(start) = head.find("://").map(|i| i + 3) {
        let end = head[start..].find('/').map_or(head.len(), |i| start + i);
        if let Some(at) = head[start..end].rfind('@') {
            out.replace_range(start..=start + at, "");
        }
    }
    if secret {
        out.push('?');
        out.push_str(REDACTED);
    }
    out
}

/// `message` with every occurrence of `url`, and of its query string, replaced by the redacted
/// form, for the text of an error a lower layer wrote.
fn scrub(message: &str, url: &str) -> String {
    let mut out = message.replace(url, &redact(url));
    if let Some(at) = url.find(['?', '#']) {
        let secret = &url[at + 1..];
        if !secret.is_empty() {
            out = out.replace(secret, REDACTED);
        }
    }
    out
}

/// What a status means for the PUT: `Ok` for 200 and 201, [`PutError::Exists`] for 412,
/// [`PutError::Unavailable`] for 503 (with `retry_after`, the value of the `Retry-After` header,
/// read as whole seconds) and [`PutError::Status`] for any other. `url` is the redacted URL.
fn classify(status: u16, url: &str, retry_after: Option<&str>) -> Result<(), PutError> {
    match status {
        200 | 201 => Ok(()),
        412 => Err(PutError::Exists {
            url: url.to_string(),
        }),
        503 => Err(PutError::Unavailable {
            url: url.to_string(),
            retry_after_secs: retry_after.and_then(|v| v.trim().parse().ok()),
        }),
        status => Err(PutError::Status {
            status,
            url: url.to_string(),
        }),
    }
}

/// What the store's `response` to a PUT means; `shown` is the redacted URL.
fn answer(response: &ureq::http::Response<ureq::Body>, shown: &str) -> Result<(), PutError> {
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok());
    classify(response.status().as_u16(), shown, retry_after)
}

/// The TLS settings of a client that trusts the given DER certificates and nothing else, with
/// the `ring` crypto provider.
pub fn tls_config(roots: &[Vec<u8>]) -> ureq::tls::TlsConfig {
    let roots = roots
        .iter()
        .map(|der| ureq::tls::Certificate::from_der(der).to_owned())
        .collect();
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(roots)))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build()
}

/// The HTTP client of the PUT: no proxy whatever the environment says, no redirect followed, a
/// non-2xx status returned as a response, a limit on the whole request, and `tls` for `https`
/// URLs. With `Some(tls)` only `https` URLs are used, so the signature never travels in the
/// clear; `None` trusts nothing and reaches only `http` URLs (tests).
pub fn agent(tls: Option<ureq::tls::TlsConfig>) -> ureq::Agent {
    let https_only = tls.is_some();
    build(tls, https_only)
}

/// [`agent`] with `tls` for `https` URLs that also reaches `http` URLs: for a store reached over a
/// private network without TLS, where the presigned signature crosses that network in the clear.
/// The caller opts in explicitly (`--allow-plain-http-put`).
pub fn agent_with_plain_http(tls: ureq::tls::TlsConfig) -> ureq::Agent {
    build(Some(tls), false)
}

fn build(tls: Option<ureq::tls::TlsConfig>, https_only: bool) -> ureq::Agent {
    let mut config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(PUT_TIMEOUT));
    if let Some(tls) = tls {
        config = config.tls_config(tls).https_only(https_only);
    }
    config.build().into()
}

/// Store `body` at the presigned `url`: one PUT, create-only.
///
/// Returns `Ok` on a 200 or 201, [`PutError::Exists`] on a 412, [`PutError::Unavailable`] on a 503
/// and [`PutError::Status`] on any other status. Nothing in the returned error contains the URL's
/// query string.
pub fn put(agent: &ureq::Agent, url: &str, body: &[u8]) -> Result<(), PutError> {
    let shown = redact(url);
    let response = agent
        .put(url)
        .header("If-None-Match", "*")
        .send(body)
        .map_err(|e| transport_error(&e, url))?;
    answer(&response, &shown)
}

/// Store the file at `path` at the presigned `url`: one create-only PUT whose body is streamed
/// from the file, with `Content-Length` taken from the opened file's metadata. The file is never
/// read whole, and it is opened afresh by each call, so [`put_retrying`] re-sends all of it after
/// a 503. Statuses map as in [`put`], and no error contains the URL's query string.
///
/// A file over [`MAX_LARGE_BYTES`] is refused before the store is contacted.
///
/// # Errors
/// [`PutError::TooLarge`] and [`PutError::File`] for a file that is too big or cannot be
/// opened; otherwise as [`put`].
pub fn put_file(agent: &ureq::Agent, url: &str, path: &Path) -> Result<(), PutError> {
    put_file_capped(agent, url, path, MAX_LARGE_BYTES)
}

/// [`put_file`] with the cap as a parameter, so a test needs no file of the real cap's size.
fn put_file_capped(agent: &ureq::Agent, url: &str, path: &Path, cap: u64) -> Result<(), PutError> {
    let (mut file, len) = File::open(path)
        .and_then(|file| file.metadata().map(|meta| (file, meta.len())))
        .map_err(|e| PutError::File {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
    put_stream(agent, url, len, &mut file, cap)
}

/// [`put_file`] for a body that is not a path: the first `len` bytes of `body` are streamed as
/// one create-only PUT with `Content-Length: len`. A `len` over [`MAX_LARGE_BYTES`] is refused
/// before the store is contacted. A caller that retries makes a fresh reader for each attempt.
///
/// # Errors
/// [`PutError::TooLarge`], else as [`put`]; a `body` shorter than `len` is a
/// [`PutError::Transport`].
pub fn put_reader(
    agent: &ureq::Agent,
    url: &str,
    len: u64,
    body: &mut dyn Read,
) -> Result<(), PutError> {
    put_stream(agent, url, len, body, MAX_LARGE_BYTES)
}

fn put_stream(
    agent: &ureq::Agent,
    url: &str,
    len: u64,
    body: &mut dyn Read,
    cap: u64,
) -> Result<(), PutError> {
    if len > cap {
        return Err(PutError::TooLarge {
            size: len,
            limit: cap,
        });
    }
    // `take` keeps a body that grew after the measure from sending more than it announced.
    let mut body = body.take(len);
    let response = agent
        .put(url)
        .header("If-None-Match", "*")
        .header("Content-Length", len.to_string())
        .send(ureq::SendBody::from_reader(&mut body))
        .map_err(|e| transport_error(&e, url))?;
    answer(&response, &redact(url))
}

/// What waits between the attempts of [`put_retrying`]; a parameter so tests record the waits
/// instead of making them.
pub trait Sleeper {
    /// Returns after `wait`.
    fn sleep(&self, wait: Duration);
}

/// The real [`Sleeper`]: the thread sleeps.
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, wait: Duration) {
        std::thread::sleep(wait);
    }
}

/// How long to wait before a retry: the store's `Retry-After`, at most [`MAX_WAIT`], else
/// [`DEFAULT_WAIT`] when it gave none.
fn wait_for(retry_after_secs: Option<u64>) -> Duration {
    retry_after_secs.map_or(DEFAULT_WAIT, |s| Duration::from_secs(s).min(MAX_WAIT))
}

/// Where the randomness of a retry's wait comes from; a parameter so tests give the numbers.
pub trait Jitter {
    /// A number from 0 up to 1. A value outside that range is taken as the nearest end of it, and
    /// one that is not a number as 0.5.
    fn unit(&self) -> f64;
}

/// The real [`Jitter`]: a number that differs from run to run and from process to process (the
/// random keys the standard library gives each hash map).
pub struct RandomJitter;

impl Jitter for RandomJitter {
    fn unit(&self) -> f64 {
        use std::hash::{BuildHasher, Hasher};
        let bits = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        // The top 53 bits are what an f64 holds exactly: a number from 0 up to, not including, 1.
        (bits >> 11) as f64 / (1_u64 << 53) as f64
    }
}

/// A [`Jitter`] that gives the same number every time, for the tests: 0.5 leaves a wait as it is.
#[cfg(test)]
pub(crate) struct FixedJitter(pub f64);

#[cfg(test)]
impl Jitter for FixedJitter {
    fn unit(&self) -> f64 {
        self.0
    }
}

/// `wait` moved by `unit` (0 to 1) to a point between half of it and one and a half times it:
/// `unit` 0 gives half, 0.5 the wait itself, 1 one and a half times.
fn jittered(wait: Duration, unit: f64) -> Duration {
    let unit = if unit.is_finite() {
        unit.clamp(0.0, 1.0)
    } else {
        0.5
    };
    wait.mul_f64(0.5 + unit)
}

/// Runs `attempt` (one PUT) and runs it again after a 503 or a [`PutError::Interrupted`]
/// connection, up to [`MAX_RETRIES`] more times, waiting with `sleeper` before each: the store's
/// `Retry-After` (at most [`MAX_WAIT`]; 2 s when it gave none, and always for an interrupted
/// connection) moved by `jitter` to between half and one and a half times as long. The waits
/// add up to at most [`MAX_TOTAL_WAIT`]: the wait that would pass it is cut to end there, and
/// when they have added up to it no retry follows. Any other outcome is returned at once: 429,
/// 500, 502 and 504 are deliberately not retried, since the store gives none of them for a full
/// hot tier or a busy put cap, and a 412 or a transport error that is not an interrupted
/// connection is final.
///
/// A retry is safe because every attempt is the create-only PUT (`If-None-Match: *`) of the same
/// key: a 503 stored nothing, and if an earlier attempt had in fact stored the object (an
/// interrupted connection may follow a store that did), the retry meets a 412. It is not taken
/// for success: the task fails (exit 13) instead of overwriting. A 412 on the first attempt, or
/// after only 503s, is [`PutError::Exists`]; a 412 after an attempt that was interrupted is
/// [`PutError::ExistsAfterInterrupt`], the same failure with a message that says so.
///
/// The URL's expiry is not checked. The waits of this function add up to at most
/// [`MAX_TOTAL_WAIT`] (60 s), but the time from the first attempt to the last is longer: each
/// attempt that the store refuses can also wait for a put permit at the store before it is
/// answered (`--put-permit-wait` of tierstore, 15 s by default), so the last attempt begins at
/// most about `MAX_RETRIES` times that wait plus 60 s after the first (about 165 s by default),
/// plus the time any refused attempt spent sending bytes. What bounds it is the URL itself: it is
/// good for the run's wall limit plus the frontend's slack (`--task-url-slack-secs`), which must
/// cover this, and a store checks the signature before it waits, so an attempt that begins after
/// the URL expired is refused with a 403, which is final. A late retry therefore fails the task
/// and stores nothing.
///
/// # Errors
/// The last attempt's [`PutError`].
pub fn put_retrying(
    mut attempt: impl FnMut() -> Result<(), PutError>,
    sleeper: &dyn Sleeper,
    jitter: &dyn Jitter,
) -> Result<(), PutError> {
    let mut retries = 0;
    let mut waited = Duration::ZERO;
    let mut interrupted = false;
    loop {
        let outcome = match attempt() {
            Err(PutError::Exists { url }) if interrupted => {
                Err(PutError::ExistsAfterInterrupt { url })
            }
            other => other,
        };
        interrupted |= matches!(outcome, Err(PutError::Interrupted { .. }));
        let retry_after = match &outcome {
            Err(PutError::Unavailable {
                retry_after_secs, ..
            }) => Some(*retry_after_secs),
            Err(PutError::Interrupted { .. }) => Some(None),
            _ => None,
        };
        match retry_after {
            Some(retry_after_secs) if retries < MAX_RETRIES && waited < MAX_TOTAL_WAIT => {
                let wait = jittered(wait_for(retry_after_secs), jitter.unit())
                    .min(MAX_TOTAL_WAIT - waited);
                sleeper.sleep(wait);
                waited += wait;
                retries += 1;
            }
            _ => return outcome,
        }
    }
}

#[cfg(test)]
#[path = "put_file_tests.rs"]
mod file_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    #[test]
    fn redact_drops_the_query_and_the_fragment() {
        assert_eq!(
            redact("https://h/p/k?X-Sig=secret&a=b"),
            "https://h/p/k?<redacted>"
        );
        assert_eq!(redact("https://h/p/k#frag"), "https://h/p/k?<redacted>");
        assert_eq!(redact("https://h/p/k?a#b"), "https://h/p/k?<redacted>");
        assert_eq!(redact("https://h/p/k"), "https://h/p/k");
        // User information is a credential too.
        assert_eq!(redact("https://user:pw@h/p/k"), "https://h/p/k");
        assert_eq!(
            redact("https://user:pw@h:9/p?a=b@c"),
            "https://h:9/p?<redacted>"
        );
        assert_eq!(redact("https://h/p/a@b"), "https://h/p/a@b");
        assert_eq!(redact("https://u:p@h"), "https://h");
    }

    #[test]
    fn scrub_removes_the_url_and_the_query_from_a_message() {
        let url = "http://h/k?sig=secret";
        assert_eq!(
            scrub("bad http://h/k?sig=secret here", url),
            "bad http://h/k?<redacted> here"
        );
        assert_eq!(
            scrub("query sig=secret alone", url),
            "query <redacted> alone"
        );
        assert_eq!(scrub("nothing", "http://h/k"), "nothing");
        // A URL ending in `?` has no secret to replace; the empty query must not match
        // everywhere.
        assert_eq!(scrub("abc", "http://h/k?"), "abc");
    }

    #[test]
    fn classify_maps_each_status() {
        assert_eq!(classify(200, "u", None), Ok(()));
        assert_eq!(classify(201, "u", None), Ok(()));
        assert_eq!(
            classify(412, "u", Some("5")),
            Err(PutError::Exists { url: "u".into() })
        );
        for status in [204, 301, 400, 403, 409, 500] {
            assert_eq!(
                classify(status, "u", Some("5")),
                Err(PutError::Status {
                    status,
                    url: "u".into()
                })
            );
        }
    }

    /// Catches a 503 being read as an ordinary status (so never retried), and a `Retry-After`
    /// that is not whole seconds being believed.
    #[test]
    fn classify_reads_the_retry_after_of_a_503() {
        let unavailable = |retry_after_secs| {
            Err(PutError::Unavailable {
                url: "u".into(),
                retry_after_secs,
            })
        };
        assert_eq!(classify(503, "u", Some("7")), unavailable(Some(7)));
        assert_eq!(classify(503, "u", Some(" 7 ")), unavailable(Some(7)));
        assert_eq!(classify(503, "u", None), unavailable(None));
        assert_eq!(classify(503, "u", Some("soon")), unavailable(None));
        assert_eq!(classify(503, "u", Some("-1")), unavailable(None));
        assert_eq!(
            classify(503, "u", Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            unavailable(None)
        );
    }

    #[test]
    fn errors_name_the_status_and_the_redacted_url() {
        let text = PutError::Status {
            status: 403,
            url: "http://h/k?<redacted>".into(),
        }
        .to_string();
        assert!(text.contains("403"));
        assert!(text.contains("http://h/k?<redacted>"));
        assert!(
            PutError::Exists { url: "u".into() }
                .to_string()
                .contains("already exists")
        );
        assert!(
            PutError::Transport {
                url: "u".into(),
                reason: "boom".into()
            }
            .to_string()
            .contains("boom")
        );
    }

    /// A [`Sleeper`] that records the waits it was asked for and does not wait.
    #[derive(Default)]
    struct Recorded(std::sync::Mutex<Vec<Duration>>);

    impl Sleeper for Recorded {
        fn sleep(&self, wait: Duration) {
            self.0.lock().expect("lock").push(wait);
        }
    }

    impl Recorded {
        fn waits(&self) -> Vec<Duration> {
            self.0.lock().expect("lock").clone()
        }
    }

    /// The attempts of a PUT that answers with `answers` in turn, and how many it was asked for.
    fn answering(
        answers: Vec<Result<(), PutError>>,
    ) -> (
        impl FnMut() -> Result<(), PutError>,
        std::rc::Rc<std::cell::Cell<usize>>,
    ) {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = calls.clone();
        let mut answers = answers.into_iter();
        (
            move || {
                calls.set(calls.get() + 1);
                answers.next().expect("more attempts than answers")
            },
            seen,
        )
    }

    fn busy(retry_after_secs: Option<u64>) -> Result<(), PutError> {
        Err(PutError::Unavailable {
            url: "u".into(),
            retry_after_secs,
        })
    }

    /// What the test server saw of one request.
    struct Seen {
        request_line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// Serve one connection on a local port: read the request, answer `status`.
    fn serve(status: u16) -> (String, JoinHandle<Seen>) {
        serve_with(status, "")
    }

    /// [`serve`] with `extra` header lines (each ending in CRLF) in the reply.
    fn serve_with(status: u16, extra: &'static str) -> (String, JoinHandle<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').expect("header colon");
                headers.push((name.to_string(), value.trim().to_string()));
            }
            let length: usize = headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, v)| v.parse().expect("length"));
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("body");
            let reply = format!(
                "HTTP/1.1 {status} X\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            reader.get_mut().write_all(reply.as_bytes()).expect("reply");
            Seen {
                request_line: request_line.trim_end().to_string(),
                headers,
                body,
            }
        });
        (base, handle)
    }

    #[test]
    fn put_sends_the_bytes_create_only_and_accepts_200_and_201() {
        for status in [200, 201] {
            let (base, server) = serve(status);
            let url = format!("{base}/pack/key?X-Sig=secret");
            let body = b"pack bytes".to_vec();
            assert_eq!(put(&agent(None), &url, &body), Ok(()));
            let seen = server.join().expect("server");
            assert_eq!(seen.request_line, "PUT /pack/key?X-Sig=secret HTTP/1.1");
            assert_eq!(seen.header("if-none-match"), Some("*"));
            assert_eq!(seen.header("content-length"), Some("10"));
            assert_eq!(seen.body, body);
        }
    }

    #[test]
    fn put_reports_an_existing_object_as_exists() {
        let (base, server) = serve(412);
        let url = format!("{base}/k?X-Sig=secret");
        let err = put(&agent(None), &url, b"x").unwrap_err();
        server.join().expect("server");
        assert_eq!(
            err,
            PutError::Exists {
                url: format!("{base}/k?<redacted>")
            }
        );
        assert!(!err.to_string().contains("secret"));
    }

    #[test]
    fn put_reports_another_status_without_the_query() {
        let (base, server) = serve(403);
        let url = format!("{base}/k?X-Sig=secret");
        let err = put(&agent(None), &url, b"x").unwrap_err();
        server.join().expect("server");
        assert_eq!(
            err,
            PutError::Status {
                status: 403,
                url: format!("{base}/k?<redacted>")
            }
        );
        assert!(!err.to_string().contains("secret"));
    }

    #[test]
    fn put_reports_an_unreachable_store_without_the_query() {
        // Bind a port, then free it: nothing listens there.
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let url = format!("http://127.0.0.1:{port}/k?X-Sig=secret");
        let err = put(&agent(None), &url, b"x").unwrap_err();
        assert!(matches!(err, PutError::Transport { .. }), "{err:?}");
        assert!(!err.to_string().contains("secret"), "{err}");
    }

    #[test]
    fn a_tls_agent_refuses_a_plain_http_url() {
        // The refusal comes before any connection: nothing listens at the address.
        let agent = agent(Some(tls_config(&[])));
        let err = put(&agent, "http://127.0.0.1:9/k?X-Sig=secret", b"x").unwrap_err();
        assert!(matches!(err, PutError::Transport { .. }), "{err:?}");
        assert!(!err.to_string().contains("secret"), "{err}");
    }

    #[test]
    fn the_plain_http_agent_reaches_an_http_store() {
        let (base, server) = serve(201);
        let url = format!("{base}/pack/key?X-Sig=secret");
        let agent = agent_with_plain_http(tls_config(&[]));
        assert_eq!(put(&agent, &url, b"pack"), Ok(()));
        let seen = server.join().expect("server");
        assert_eq!(seen.header("if-none-match"), Some("*"));
        assert_eq!(seen.body, b"pack");
    }

    #[test]
    fn put_reports_a_malformed_url_without_the_query() {
        let err = put(&agent(None), "not a url?X-Sig=secret", b"x").unwrap_err();
        assert!(matches!(err, PutError::Transport { .. }), "{err:?}");
        assert!(!err.to_string().contains("secret"), "{err}");
    }

    #[test]
    fn tls_config_builds_with_and_without_roots() {
        let _ = agent(Some(tls_config(&[])));
        // A certificate that is not parseable is kept as given; the handshake would refuse it.
        let _ = agent(Some(tls_config(&[vec![1, 2, 3]])));
    }

    /// Catches a 503's `Retry-After` header not reaching the error: it is read from the real
    /// response, in seconds, and a 503 without one carries none.
    #[test]
    fn put_reports_a_503_with_its_retry_after() {
        for (header, expected) in [("Retry-After: 12\r\n", Some(12)), ("", None)] {
            let (base, server) = serve_with(503, header);
            let url = format!("{base}/k?X-Sig=secret");
            let err = put(&agent(None), &url, b"x").unwrap_err();
            server.join().expect("server");
            assert_eq!(
                err,
                PutError::Unavailable {
                    url: format!("{base}/k?<redacted>"),
                    retry_after_secs: expected
                }
            );
            assert!(!err.to_string().contains("secret"));
        }
    }

    /// Catches the message of a 503 hiding what the store asked for.
    #[test]
    fn a_503_names_the_wait_the_store_asked_for() {
        let text = busy(Some(9)).unwrap_err().to_string();
        assert!(text.contains("503"), "{text}");
        assert!(text.contains("retry after 9s"), "{text}");
        assert!(!busy(None).unwrap_err().to_string().contains("retry after"));
    }

    /// Catches a 503 failing the task after the whole build ran: one 503 and then a 201 is
    /// stored, after one wait equal to the `Retry-After`.
    #[test]
    fn a_503_then_a_201_is_stored_after_the_wait_the_store_asked_for() {
        let sleeper = Recorded::default();
        let (attempt, calls) = answering(vec![busy(Some(5)), Ok(())]);
        assert_eq!(put_retrying(attempt, &sleeper, &MIDDLE), Ok(()));
        assert_eq!(calls.get(), 2);
        assert_eq!(sleeper.waits(), [Duration::from_secs(5)]);
    }

    /// The jitter that leaves a wait as the store asked for it.
    const MIDDLE: FixedJitter = FixedJitter(0.5);

    /// A [`Jitter`] that gives the numbers it was given, one for each draw, and fails the test
    /// when it is asked for more.
    struct Sequence(std::sync::Mutex<std::collections::VecDeque<f64>>);

    impl Sequence {
        fn of(units: &[f64]) -> Self {
            Self(std::sync::Mutex::new(units.iter().copied().collect()))
        }
    }

    impl Jitter for Sequence {
        fn unit(&self) -> f64 {
            self.0
                .lock()
                .expect("lock")
                .pop_front()
                .expect("a draw that was given")
        }
    }

    /// Catches a wait that leaves the band around the `Retry-After`: for each `Retry-After`
    /// (the cap included, and past it) and each number the jitter may give (the ends, the
    /// middle, numbers outside 0 to 1, and not a number) the one wait is from half of the
    /// `Retry-After` (cut at `MAX_WAIT`) to one and a half times it.
    #[test]
    fn a_wait_is_within_half_and_one_and_a_half_times_the_retry_after() {
        for secs in [0_u64, 1, 5, 29, 30, 3600] {
            let base = Duration::from_secs(secs).min(MAX_WAIT);
            for unit in [0.0, 0.1, 0.25, 0.5, 0.75, 0.999, 1.0, -3.0, 7.0, f64::NAN] {
                let sleeper = Recorded::default();
                let (attempt, _) = answering(vec![busy(Some(secs)), Ok(())]);
                let jitter = FixedJitter(unit);
                assert_eq!(put_retrying(attempt, &sleeper, &jitter), Ok(()));
                let waits = sleeper.waits();
                assert_eq!(waits.len(), 1);
                assert!(
                    waits[0] >= base.mul_f64(0.5),
                    "{secs} {unit}: {:?}",
                    waits[0]
                );
                assert!(
                    waits[0] <= base.mul_f64(1.5),
                    "{secs} {unit}: {:?}",
                    waits[0]
                );
            }
        }
    }

    /// Catches the jitter being ignored or mis-scaled: a draw of 0 halves the `Retry-After`, 0.5
    /// leaves it, 1 makes it one and a half times, and a quarter and three quarters are on the
    /// line between them. Each wait of one PUT takes a draw of its own, in order.
    #[test]
    fn each_wait_takes_its_own_draw_and_scales_the_retry_after_with_it() {
        let sleeper = Recorded::default();
        let units = Sequence::of(&[0.0, 0.5, 1.0, 0.25, 0.75]);
        let answers = vec![busy(Some(10)); 5];
        let (attempt, calls) = answering([answers, vec![Ok(())]].concat());
        assert_eq!(put_retrying(attempt, &sleeper, &units), Ok(()));
        assert_eq!(calls.get(), 6);
        let secs = |s: f64| Duration::from_secs_f64(s);
        assert_eq!(
            sleeper.waits(),
            [secs(5.0), secs(10.0), secs(15.0), secs(7.5), secs(12.5)]
        );
        assert!(units.0.lock().expect("lock").is_empty());
    }

    /// Catches waits that add up past `MAX_TOTAL_WAIT`, or that go on when it is reached: a store
    /// that asks for 30 s and a jitter that stretches it to 45 s gets waits of 45 s and then 15 s
    /// (the wait that would pass the cap is cut), then a third attempt, which is the last, and the
    /// 503 it answers is the failure, the same error as when the retries run out.
    #[test]
    fn the_waits_stop_at_the_total_cap_and_the_last_503_is_the_failure() {
        let sleeper = Recorded::default();
        let answers = (0..=MAX_RETRIES).map(|_| busy(Some(30))).collect();
        let (attempt, calls) = answering(answers);
        let err = put_retrying(attempt, &sleeper, &FixedJitter(1.0)).unwrap_err();
        assert_eq!(
            err,
            PutError::Unavailable {
                url: "u".into(),
                retry_after_secs: Some(30)
            }
        );
        assert_eq!(calls.get(), 3);
        let waits = sleeper.waits();
        assert_eq!(waits, [Duration::from_secs(45), Duration::from_secs(15)]);
        assert_eq!(waits.iter().sum::<Duration>(), MAX_TOTAL_WAIT);
    }

    /// Catches a store's reset of a connection that was still sending a body being final: an
    /// interrupted connection and then a 201 is stored after one wait of the default length,
    /// and one that keeps breaking is asked `MAX_RETRIES` + 1 times and is the error.
    #[test]
    fn an_interrupted_connection_is_retried_like_a_503_without_a_retry_after() {
        let broken = || {
            Err(PutError::Interrupted {
                url: "u".into(),
                reason: "reset".into(),
            })
        };
        let sleeper = Recorded::default();
        let (attempt, calls) = answering(vec![broken(), Ok(())]);
        assert_eq!(put_retrying(attempt, &sleeper, &MIDDLE), Ok(()));
        assert_eq!(calls.get(), 2);
        assert_eq!(sleeper.waits(), [DEFAULT_WAIT]);

        let sleeper = Recorded::default();
        let (attempt, calls) = answering((0..=MAX_RETRIES).map(|_| broken()).collect());
        assert_eq!(put_retrying(attempt, &sleeper, &MIDDLE), broken());
        assert_eq!(calls.get(), MAX_RETRIES as usize + 1);
        assert_eq!(sleeper.waits().len(), MAX_RETRIES as usize);
    }

    /// Catches a 412 that follows an interrupted connection being taken for success or hidden:
    /// the retry that meets the object an earlier attempt stored is the failure, and it is
    /// `ExistsAfterInterrupt`, with a message that says so, also when 503s lie between; a 412 on
    /// the first attempt, or after only 503s, is the plain `Exists` with its old message.
    #[test]
    fn a_412_after_an_interrupted_connection_is_the_failure_with_its_own_reason() {
        let exists = || PutError::Exists { url: "u".into() };
        let broken = || PutError::Interrupted {
            url: "u".into(),
            reason: "reset".into(),
        };
        let after = PutError::ExistsAfterInterrupt { url: "u".into() };
        for (answers, calls_made) in [
            (vec![Err(broken()), Err(exists())], 2),
            (vec![Err(broken()), busy(Some(1)), Err(exists())], 3),
        ] {
            let sleeper = Recorded::default();
            let (attempt, calls) = answering(answers);
            assert_eq!(put_retrying(attempt, &sleeper, &MIDDLE), Err(after.clone()));
            assert_eq!(calls.get(), calls_made);
        }
        let text = after.to_string();
        assert!(
            text.contains("412 after an interrupted attempt of this put"),
            "{text}"
        );
        assert!(
            text.contains("the earlier attempt may have stored it"),
            "{text}"
        );
        assert_ne!(text, exists().to_string());

        for answers in [vec![Err(exists())], vec![busy(Some(1)), Err(exists())]] {
            let sleeper = Recorded::default();
            let (attempt, _) = answering(answers);
            assert_eq!(put_retrying(attempt, &sleeper, &MIDDLE), Err(exists()));
        }
        assert_eq!(exists().to_string(), "the object already exists at u");
    }

    /// Catches the wrong errors being retried: a reset, an abort and a broken pipe are
    /// `Interrupted`; a refused connection, a timeout and a bad URL are `Transport` (final), and
    /// neither message holds the URL's query.
    #[test]
    fn only_a_connection_that_broke_after_it_was_made_is_interrupted() {
        use std::io::{Error, ErrorKind};
        let url = "http://h/k?X-Sig=secret";
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
        ] {
            let got = transport_error(&ureq::Error::Io(Error::from(kind)), url);
            assert!(
                matches!(got, PutError::Interrupted { .. }),
                "{kind:?}: {got:?}"
            );
            assert!(!got.to_string().contains("secret"), "{got}");
        }
        // What macOS gives for a reset socket: interrupted there, final everywhere else.
        for code in [libc::EINVAL, libc::EPROTOTYPE] {
            let got = transport_error(&ureq::Error::Io(Error::from_raw_os_error(code)), url);
            let interrupted = matches!(got, PutError::Interrupted { .. });
            assert_eq!(
                interrupted,
                cfg!(target_os = "macos"),
                "os error {code}: {got:?}"
            );
            assert!(
                matches!(
                    got,
                    PutError::Interrupted { .. } | PutError::Transport { .. }
                ),
                "os error {code}: {got:?}"
            );
        }
        for error in [
            ureq::Error::Io(Error::from(ErrorKind::ConnectionRefused)),
            ureq::Error::Io(Error::from(ErrorKind::UnexpectedEof)),
            ureq::Error::Timeout(ureq::Timeout::Global),
            ureq::Error::HostNotFound,
        ] {
            let got = transport_error(&error, url);
            assert!(
                matches!(got, PutError::Transport { .. }),
                "{error:?}: {got:?}"
            );
            assert!(!got.to_string().contains("secret"), "{got}");
        }
    }

    /// Catches a store that closes the connection cleanly before it answers being retried: the
    /// error ureq gives for it (an end of file, "Peer disconnected") is `Transport`, which is
    /// final, so one attempt is made and no wait follows; and the same through a real socket,
    /// where the stub reads the whole request, then closes without a byte of answer.
    #[test]
    fn a_store_that_closes_before_answering_is_a_final_transport_error() {
        use std::io::{Error, ErrorKind};
        let url = "http://h/k?X-Sig=secret";
        let eof = ureq::Error::Io(Error::new(ErrorKind::UnexpectedEof, "Peer disconnected"));
        let sleeper = Recorded::default();
        let (attempt, calls) = answering(vec![Err(transport_error(&eof, url)), Ok(())]);
        let err = put_retrying(attempt, &sleeper, &MIDDLE).unwrap_err();
        assert!(matches!(err, PutError::Transport { .. }), "{err:?}");
        assert_eq!(calls.get(), 1);
        assert!(sleeper.waits().is_empty());

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("a line");
                if line.trim_end().is_empty() {
                    break;
                }
            }
            // The one byte of the body, so that nothing unread makes the close a reset.
            let mut body = [0_u8; 1];
            reader.read_exact(&mut body).expect("the body");
        });
        let url = format!("{base}/k?X-Sig=secret");
        let sleeper = Recorded::default();
        let client = agent(None);
        let mut attempts = 0;
        let err = put_retrying(
            || {
                attempts += 1;
                put(&client, &url, b"x")
            },
            &sleeper,
            &MIDDLE,
        )
        .unwrap_err();
        server.join().expect("server");
        assert!(matches!(err, PutError::Transport { .. }), "{err:?}");
        assert_eq!(attempts, 1);
        assert!(sleeper.waits().is_empty());
    }

    /// Catches the real jitter being constant or outside 0 to under 1: 64 draws are all in range
    /// and are not all the same number.
    #[test]
    fn the_random_jitter_draws_from_zero_up_to_one_and_varies() {
        let draws: Vec<f64> = (0..64).map(|_| RandomJitter.unit()).collect();
        assert!(draws.iter().all(|u| (0.0..1.0).contains(u)), "{draws:?}");
        assert!(draws.iter().any(|u| *u != draws[0]), "{draws:?}");
    }

    /// Catches an unbounded retry: a store that stays full is asked `MAX_RETRIES` + 1 times,
    /// no more, then the 503 is the failure.
    #[test]
    fn a_store_that_stays_full_is_asked_a_bounded_number_of_times() {
        let sleeper = Recorded::default();
        let answers = (0..=MAX_RETRIES).map(|_| busy(Some(1))).collect();
        let (attempt, calls) = answering(answers);
        let err = put_retrying(attempt, &sleeper, &MIDDLE).unwrap_err();
        assert_eq!(
            err,
            PutError::Unavailable {
                url: "u".into(),
                retry_after_secs: Some(1)
            }
        );
        assert_eq!(calls.get(), MAX_RETRIES as usize + 1);
        assert_eq!(sleeper.waits().len(), MAX_RETRIES as usize);
    }

    /// Catches a 412 being retried: the key exists and is final, so one PUT and no wait.
    #[test]
    fn a_412_is_not_retried() {
        let sleeper = Recorded::default();
        let exists = PutError::Exists { url: "u".into() };
        let (attempt, calls) = answering(vec![Err(exists.clone())]);
        assert_eq!(put_retrying(attempt, &sleeper, &MIDDLE), Err(exists));
        assert_eq!(calls.get(), 1);
        assert!(sleeper.waits().is_empty());
    }

    /// Catches a retry that hides what the later attempt said: after a 503, the retry meets a
    /// 412 (an earlier attempt's object, or another writer's, holds the key), and that 412 is the
    /// failure, after exactly one wait and with no further attempt.
    #[test]
    fn a_412_after_a_503_is_the_failure_and_ends_the_retries() {
        let sleeper = Recorded::default();
        let exists = PutError::Exists { url: "u".into() };
        let (attempt, calls) = answering(vec![busy(Some(4)), Err(exists.clone())]);
        assert_eq!(put_retrying(attempt, &sleeper, &MIDDLE), Err(exists));
        assert_eq!(calls.get(), 2);
        assert_eq!(sleeper.waits(), [Duration::from_secs(4)]);
    }

    /// Catches a transport error or another status being retried: each is reported on the first
    /// attempt.
    #[test]
    fn a_transport_error_and_another_status_are_not_retried() {
        for error in [
            PutError::Transport {
                url: "u".into(),
                reason: "reset".into(),
            },
            PutError::Status {
                status: 500,
                url: "u".into(),
            },
        ] {
            let sleeper = Recorded::default();
            let (attempt, calls) = answering(vec![Err(error.clone())]);
            assert_eq!(put_retrying(attempt, &sleeper, &MIDDLE), Err(error));
            assert_eq!(calls.get(), 1);
            assert!(sleeper.waits().is_empty());
        }
    }

    /// Catches the wait being unbounded or missing: no usable `Retry-After` waits the default,
    /// and a long one is cut to the cap.
    #[test]
    fn the_wait_is_the_retry_after_capped_or_a_default() {
        let sleeper = Recorded::default();
        let (attempt, _) = answering(vec![busy(None), busy(Some(3600)), busy(Some(0)), Ok(())]);
        assert_eq!(put_retrying(attempt, &sleeper, &MIDDLE), Ok(()));
        assert_eq!(sleeper.waits(), [DEFAULT_WAIT, MAX_WAIT, Duration::ZERO]);
    }

    /// Catches the real sleeper not being the thread's sleep (a zero wait returns at once).
    #[test]
    fn the_thread_sleeper_returns_after_its_wait() {
        ThreadSleeper.sleep(Duration::ZERO);
    }
}
