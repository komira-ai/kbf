use std::time::Duration;

use super::*;
use crate::testkit::{http, tls_server};

fn url(s: &str) -> HttpsUrl {
    HttpsUrl::new(s).expect(s)
}

/// The answer `read_answer` makes of `raw`, and the body it wrote.
fn answer(raw: &[u8]) -> (Result<Answer, String>, Vec<u8>) {
    let mut body = Vec::new();
    let got = read_answer(&url("https://h/x"), raw, &mut body).map_err(|e| e.to_string());
    (got, body)
}

/// Catches a URL parser that takes a plain-HTTP URL (the download would not be
/// private), one that keeps a user part (it would be printed), and one that loses the
/// port, the default 443 or the path.
#[test]
fn an_https_url_names_host_port_and_path() {
    let u = url("https://github.com/o/r/releases/download/v1.0/a-v1.0.zip");
    assert_eq!((u.host(), u.port()), ("github.com", 443));
    assert_eq!(u.path(), "/o/r/releases/download/v1.0/a-v1.0.zip");
    assert_eq!(u.file_name(), "a-v1.0.zip");
    assert_eq!(
        u.to_string(),
        "https://github.com/o/r/releases/download/v1.0/a-v1.0.zip"
    );
    let q = url("https://h:8443/scep?operation=GetCACert");
    assert_eq!((q.port(), q.file_name()), (8443, "scep"));
    let bare = url("https://h");
    assert_eq!((bare.path(), bare.file_name()), ("/", ""));
    type Variant = fn(String) -> HttpsUrlError;
    let refused: [(&str, Variant); 7] = [
        ("http://h/", HttpsUrlError::Scheme),
        ("h/", HttpsUrlError::Scheme),
        ("https:///x", HttpsUrlError::Host),
        ("https://user:key@h/x", HttpsUrlError::Host),
        ("https://[::1]/x", HttpsUrlError::Host),
        ("https://h:0/", HttpsUrlError::Port),
        ("https://h:x/", HttpsUrlError::Port),
    ];
    for (u, want) in refused {
        assert_eq!(HttpsUrl::new(u), Err(want(u.to_owned())), "{u}");
    }
}

/// Catches a body read the wrong way for its framing: past its Content-Length, short of
/// it, chunks with their size lines left in, or a close-delimited body cut short.
#[test]
fn a_body_is_read_by_its_framing() {
    let cases: [(&[u8], &[u8]); 4] = [
        (b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcdef", b"abc"),
        (
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: Chunked\r\n\r\n3;x=y\r\nabc\r\n2\r\nde\r\n0\r\nT: 1\r\n\r\n",
            b"abcde",
        ),
        (b"HTTP/1.1 200 OK\r\nServer: x\r\n\r\nto the end", b"to the end"),
        (b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n", b""),
    ];
    for (raw, want) in cases {
        let (got, body) = answer(raw);
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(body, want, "{}", String::from_utf8_lossy(raw));
    }
}

/// Catches an error page or a redirect's body written into the download.
#[test]
fn only_a_2xx_body_is_written() {
    for status in ["404 Not Found", "302 Found", "199 Odd"] {
        let raw = http(status, b"not the file");
        let (got, body) = answer(&raw);
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(body, b"", "{status}");
    }
    let (got, _) =
        answer(b"HTTP/1.1 302 Found\r\nLocation: https://b/y\r\nContent-Length: 0\r\n\r\n");
    let want = Answer {
        status: 302,
        location: Some("https://b/y".to_owned()),
    };
    assert_eq!(got.ok(), Some(want));
}

/// Catches an answer taken as complete when it is cut short or is not HTTP: each must
/// be an error, so a partial download is never checked as if it were whole.
#[test]
fn a_short_or_malformed_answer_is_an_error() {
    let long = format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "a".repeat(HEAD_MAX));
    let cases: [(&[u8], &str); 9] = [
        (b"HTTP/1.1 200 OK\r\n", "end early"),
        (long.as_bytes(), "too long"),
        (b"SSH-2.0-x\r\n\r\n", "not HTTP"),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n",
            "Content-Length \"x\"",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc",
            "end of file",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            "not hex",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
            "end of file",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nabc\r\n0\r\n\r\n",
            "longer than its size",
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            "end of file",
        ),
    ];
    for (raw, want) in cases {
        let (got, _) = answer(raw);
        let err = got.err().unwrap_or_default();
        assert!(err.contains(want), "{err:?} lacks {want:?}");
    }
}

/// Catches a reply body read without a bound: a body of exactly [`REPLY_MAX`] bytes is
/// kept, one byte more is refused, whether it runs to the end of the connection, has a
/// `Content-Length` or comes in chunks.
#[test]
fn a_reply_body_longer_than_its_cap_is_refused_however_framed() {
    let reply = |head: &str, body: &[u8]| {
        let mut raw = format!("HTTP/1.1 400 Bad Request\r\n{head}\r\n").into_bytes();
        raw.extend_from_slice(body);
        read_reply(&url("https://h/x"), &raw[..]).map_err(|e| e.to_string())
    };
    let fits = vec![b'a'; REPLY_MAX];
    let over = vec![b'a'; REPLY_MAX + 1];
    assert_eq!(reply("", &fits), Ok((400, fits.clone())));
    let chunked = |b: &[u8]| {
        let mut raw = format!("{:x}\r\n", b.len()).into_bytes();
        raw.extend_from_slice(b);
        raw.extend_from_slice(b"\r\n0\r\n\r\n");
        raw
    };
    assert_eq!(
        reply("Transfer-Encoding: chunked\r\n", &chunked(&fits)),
        Ok((400, fits.clone()))
    );
    let too_long = format!("https://h/x: the answer's body is longer than {REPLY_MAX} bytes");
    for (head, body) in [
        (String::new(), over.clone()),
        (format!("Content-Length: {}\r\n", over.len()), over.clone()),
        ("Transfer-Encoding: chunked\r\n".to_owned(), chunked(&over)),
    ] {
        assert_eq!(reply(&head, &body), Err(too_long.clone()), "{head:?}");
    }
    let mut capped = Capped {
        bytes: Vec::new(),
        over: false,
    };
    assert!(capped.flush().is_ok());
}

/// Catches a client that trusts no roots without saying so, or one that cannot build
/// from the host's store.
#[test]
fn roots_are_refused_when_none_parses_and_the_host_store_loads() {
    let err = roots(vec![CertificateDer::from(b"not a certificate".to_vec())]).err();
    assert!(matches!(err, Some(HttpsError::NoRoots)));
    assert!(Client::system(Duration::from_secs(1)).is_ok());
}

/// Catches a client that does not check the server's certificate (a server under
/// another CA is accepted), that sends the wrong request line, or that does not
/// connect to the address it is given.
#[test]
fn a_get_checks_the_server_and_reads_its_answer() {
    let server = tls_server(&["mdm.example.com"], |path| http("200 OK", path.as_bytes()));
    let client = server.client();
    let at = server.url("mdm.example.com", "/scep?operation=GetCACert");
    let mut body = Vec::new();
    let got = client
        .get(&at, Some(server.addr), &mut body)
        .expect("served");
    assert_eq!(
        (got.status, body),
        (200, b"/scep?operation=GetCACert".to_vec())
    );

    let stranger = tls_server(&["mdm.example.com"], |_| http("200 OK", b""));
    let err = stranger
        .client()
        .get(&at, Some(server.addr), &mut Vec::new())
        .err();
    let err = err.map(|e| e.to_string()).unwrap_or_default();
    assert!(err.contains("invalid peer certificate"), "{err}");
    let other_name = server.url("other.example.com", "/");
    let err = client
        .get(&other_name, Some(server.addr), &mut Vec::new())
        .err();
    let err = err.map(|e| e.to_string()).unwrap_or_default();
    assert!(err.contains("not valid for name"), "{err}");
}

/// Catches a failure to connect, or a host name rustls cannot check, reported as
/// anything but an error.
#[test]
fn a_get_that_cannot_connect_is_an_error() {
    let server = tls_server(&["127.0.0.1"], |_| http("200 OK", b""));
    let client = server.client();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);
    let gone = url(&format!("https://127.0.0.1:{port}/"));
    assert!(matches!(
        client.get(&gone, None, &mut Vec::new()),
        Err(HttpsError::Io { .. })
    ));
    let bad = url("https://a b/");
    assert!(matches!(
        client.get(&bad, Some(server.addr), &mut Vec::new()),
        Err(HttpsError::Malformed { .. })
    ));
    let unknown = url("https://unresolvable.invalid/");
    assert!(matches!(
        client.get(&unknown, None, &mut Vec::new()),
        Err(HttpsError::Io { .. })
    ));
}

/// Catches a client that stops at a redirect (a GitHub release asset is one), that
/// follows one to plain HTTP, or that follows redirects forever.
#[test]
fn redirects_are_followed_to_https_only_and_not_forever() {
    let server = tls_server(&["127.0.0.1"], |path| {
        let port = path.rsplit('/').next().unwrap_or_default();
        match path {
            _ if path.starts_with("/hop/") => format!(
                "HTTP/1.1 302 Found\r\nLocation: https://127.0.0.1:{port}/final\r\nContent-Length: 0\r\n\r\n"
            )
            .into_bytes(),
            "/a" => b"HTTP/1.1 302 Found\r\nLocation: /b\r\nContent-Length: 0\r\n\r\n".to_vec(),
            "/plain" => {
                b"HTTP/1.1 301 Moved\r\nLocation: http://127.0.0.1/\r\nContent-Length: 0\r\n\r\n"
                    .to_vec()
            }
            "/created" => {
                b"HTTP/1.1 201 Created\r\nLocation: https://127.0.0.1/x\r\nContent-Length: 2\r\n\r\nok"
                    .to_vec()
            }
            _ if path.starts_with("/loop/") => format!(
                "HTTP/1.1 307 Again\r\nLocation: https://127.0.0.1:{port}/loop/{port}\r\nContent-Length: 0\r\n\r\n"
            )
            .into_bytes(),
            _ => http("200 OK", b"the file"),
        }
    });
    let client = server.client();
    let port = server.addr.port();
    let base = format!("https://127.0.0.1:{port}");
    let hop = url(&format!("{base}/hop/{port}"));
    let mut body = Vec::new();
    assert_eq!(client.follow(&hop, &mut body).expect("served").status, 200);
    assert_eq!(body, b"the file");

    let relative = url(&format!("{base}/a"));
    let err = client.follow(&relative, &mut Vec::new()).err();
    assert!(
        matches!(err, Some(HttpsError::Redirect { ref to, .. }) if to == "/b"),
        "{err:?}"
    );
    let plain = url(&format!("{base}/plain"));
    assert!(matches!(
        client.follow(&plain, &mut Vec::new()),
        Err(HttpsError::Redirect { .. })
    ));
    let mut created = Vec::new();
    let made = client.follow(&url(&format!("{base}/created")), &mut created);
    assert_eq!(made.expect("served").status, 201);
    assert_eq!(created, b"ok");
    let looping = url(&format!("{base}/loop/{port}"));
    let err = client.follow(&looping, &mut Vec::new()).err();
    assert!(matches!(err, Some(HttpsError::Redirects { .. })), "{err:?}");
    let served = server.paths.lock().expect("lock").len();
    assert_eq!(served, 2 + 1 + 1 + 1 + REDIRECTS + 1);
}
