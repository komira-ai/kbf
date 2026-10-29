//! Storing the pack: one create-only HTTP PUT of its bytes to a presigned URL.
//!
//! The URL is taken as given and is a credential (its query string carries the signature), so it
//! is never printed: every error, and every message a lower layer produced, names the URL only
//! as [`redact`] renders it. The PUT carries `Content-Length` and `If-None-Match: *`, so the
//! store refuses it when the key already holds an object.
//!
//! One answer is retried: 503, which the store gives when its hot tier is full and nothing was
//! stored ([`put_retrying`]). Every other answer, a 412 (the key exists, and a re-attempt of the
//! task gets a new key, so it is final) and a transport error included, is reported on the first
//! attempt.

use std::sync::Arc;
use std::time::Duration;

/// How long the tool waits to reach the store.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the whole PUT may take, so a store that stalls cannot hold the tool.
const PUT_TIMEOUT: Duration = Duration::from_secs(900);

/// How many times a 503 is retried after the first attempt.
pub const MAX_RETRIES: u32 = 3;

/// The longest the tool waits before one retry, however long the store asks.
pub const MAX_WAIT: Duration = Duration::from_secs(30);

/// How long the tool waits before a retry when the 503 names no usable `Retry-After`.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(2);

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
    /// The request did not complete: the store was not reached, or the connection failed.
    #[error("the PUT of {url} failed: {reason}")]
    Transport {
        /// The URL, redacted.
        url: String,
        /// What went wrong, with the URL's secret parts removed.
        reason: String,
    },
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
        .map_err(|e| PutError::Transport {
            url: shown.clone(),
            reason: scrub(&e.to_string(), url),
        })?;
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok());
    classify(response.status().as_u16(), &shown, retry_after)
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

/// Runs `attempt` (one PUT) and runs it again after a 503, up to [`MAX_RETRIES`] more times,
/// waiting with `sleeper` before each. Any other outcome is returned at once.
///
/// A retry is safe because every attempt is the create-only PUT (`If-None-Match: *`) of the same
/// key: a 503 stored nothing, and if an earlier attempt had in fact stored the object, the retry
/// meets a 412 and fails instead of overwriting it.
///
/// # Errors
/// The last attempt's [`PutError`].
pub fn put_retrying(
    mut attempt: impl FnMut() -> Result<(), PutError>,
    sleeper: &dyn Sleeper,
) -> Result<(), PutError> {
    let mut retries = 0;
    loop {
        match attempt() {
            Err(PutError::Unavailable {
                retry_after_secs, ..
            }) if retries < MAX_RETRIES => {
                sleeper.sleep(wait_for(retry_after_secs));
                retries += 1;
            }
            outcome => return outcome,
        }
    }
}

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
        assert_eq!(put_retrying(attempt, &sleeper), Ok(()));
        assert_eq!(calls.get(), 2);
        assert_eq!(sleeper.waits(), [Duration::from_secs(5)]);
    }

    /// Catches an unbounded retry: a store that stays full is asked `MAX_RETRIES` + 1 times,
    /// no more, then the 503 is the failure.
    #[test]
    fn a_store_that_stays_full_is_asked_a_bounded_number_of_times() {
        let sleeper = Recorded::default();
        let answers = (0..=MAX_RETRIES).map(|_| busy(Some(1))).collect();
        let (attempt, calls) = answering(answers);
        let err = put_retrying(attempt, &sleeper).unwrap_err();
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
        assert_eq!(put_retrying(attempt, &sleeper), Err(exists));
        assert_eq!(calls.get(), 1);
        assert!(sleeper.waits().is_empty());
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
            assert_eq!(put_retrying(attempt, &sleeper), Err(error));
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
        assert_eq!(put_retrying(attempt, &sleeper), Ok(()));
        assert_eq!(sleeper.waits(), [DEFAULT_WAIT, MAX_WAIT, Duration::ZERO]);
    }

    /// Catches the real sleeper not being the thread's sleep (a zero wait returns at once).
    #[test]
    fn the_thread_sleeper_returns_after_its_wait() {
        ThreadSleeper.sleep(Duration::ZERO);
    }
}
