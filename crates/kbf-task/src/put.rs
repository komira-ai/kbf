//! Storing the pack: one create-only HTTP PUT of its bytes to a presigned URL.
//!
//! The URL is taken as given and is a credential (its query string carries the signature), so it
//! is never printed: every error, and every message a lower layer produced, names the URL only
//! as [`redact`] renders it. The PUT carries `Content-Length` and `If-None-Match: *`, so the
//! store refuses it when the key already holds an object. It is made once: a retry with the same
//! key can only meet that refusal, so a failure is reported, not retried.

use std::sync::Arc;
use std::time::Duration;

/// How long the tool waits to reach the store.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the whole PUT may take, so a store that stalls cannot hold the tool.
const PUT_TIMEOUT: Duration = Duration::from_secs(900);

/// The string that stands in for a URL's query string and fragment.
const REDACTED: &str = "<redacted>";

/// Why the pack was not stored.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PutError {
    /// The store answered 412: the key already holds an object.
    #[error("the object already exists at {url}")]
    Exists {
        /// The URL, redacted.
        url: String,
    },
    /// The store answered with a status other than 200, 201 or 412.
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

/// What a status means for the PUT: `Ok` for 200 and 201, [`PutError::Exists`] for 412, and
/// [`PutError::Status`] for any other. `url` is the redacted URL.
fn classify(status: u16, url: &str) -> Result<(), PutError> {
    match status {
        200 | 201 => Ok(()),
        412 => Err(PutError::Exists {
            url: url.to_string(),
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
    let mut config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(PUT_TIMEOUT));
    if let Some(tls) = tls {
        config = config.tls_config(tls).https_only(true);
    }
    config.build().into()
}

/// Store `body` at the presigned `url`: one PUT, create-only, never retried.
///
/// Returns `Ok` on a 200 or 201, [`PutError::Exists`] on a 412 and [`PutError::Status`] on any
/// other status. Nothing in the returned error contains the URL's query string.
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
    classify(response.status().as_u16(), &shown)
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
        assert_eq!(classify(200, "u"), Ok(()));
        assert_eq!(classify(201, "u"), Ok(()));
        assert_eq!(
            classify(412, "u"),
            Err(PutError::Exists { url: "u".into() })
        );
        for status in [204, 301, 400, 403, 409, 500, 503] {
            assert_eq!(
                classify(status, "u"),
                Err(PutError::Status {
                    status,
                    url: "u".into()
                })
            );
        }
    }

    #[test]
    fn errors_name_the_status_and_the_redacted_url() {
        let text = PutError::Status {
            status: 403,
            url: "http://h/k?<redacted>".into(),
        }
        .to_string();
        assert!(text.contains("403") && text.contains("http://h/k?<redacted>"));
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
            let reply =
                format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
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
}
