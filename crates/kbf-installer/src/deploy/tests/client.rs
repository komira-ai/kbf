//! The client of the API: how an answer is read, and how a TLS session to the public port is
//! set up and what it refuses.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::Ordering::SeqCst;

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use super::tls::{NAME, TOKEN, with_tls};
use super::*;
use crate::deploy::api::{Https, Trust, parse_reply, read_all};
use crate::deploy::smoke_tests::pki;

const SECS: Duration = Duration::from_secs(10);

/// Proves a reply is read whole as the plain or the chunked body it announces, and that a
/// head with no end, no status, a bad chunk, a header that is not a number or a body of
/// another length than `Content-Length` says is an error. Catches an answer cut short taken
/// for a whole one, and a chunked body read as plain.
///
/// Mutants planted: the length check dropped; `chunked` set by any `transfer-encoding`; the
/// header names compared with their case.
#[test]
fn replies_are_parsed_plain_and_chunked() {
    let plain = parse_reply(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}").unwrap();
    assert_eq!((plain.status, plain.body.as_slice()), (201, &b"{}"[..]));
    assert_eq!(plain.json(), Some(json!({})));
    let lower = parse_reply(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nx: y\r\n\r\n{}").unwrap();
    assert_eq!(lower.body, b"{}");
    let chunked = parse_reply(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n",
    )
    .unwrap();
    assert_eq!(chunked.body, b"abcde");
    // Another encoding is no chunking: the body is the bytes after the head.
    let other = parse_reply(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: identity\r\n\r\nabc").unwrap();
    assert_eq!(other.body, b"abc");
    // No length is read to the end.
    assert_eq!(
        parse_reply(b"HTTP/1.1 200 OK\r\n\r\nabc").unwrap().body,
        b"abc"
    );
    for (bad, why) in [
        (&b"HTTP/1.1 200 OK"[..], "no end of headers"),
        (b"nonsense\r\n\r\n", "no status"),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            "a bad chunked body",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: two\r\n\r\n{}",
            "Content-Length is not a number",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}",
            "not as long as Content-Length says",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n{}",
            "not as long as Content-Length says",
        ),
    ] {
        let err = parse_reply(bad).unwrap_err();
        assert!(err.to_string().contains(why), "{why}: {err}");
    }
}

/// A reader that gives these results in turn, then the end.
struct Script(Vec<io::Result<Vec<u8>>>);

impl Read for Script {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.0.is_empty() {
            return Ok(0);
        }
        match self.0.remove(0) {
            Ok(bytes) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Err(e) => Err(e),
        }
    }
}

/// Proves an answer is read to its end, that a peer which closes the connection without
/// the TLS goodbye (an `UnexpectedEof`) has ended it too, that any other failure is a
/// failure, and that an answer over the limit is refused rather than held. Catches an
/// answer lost to a missing goodbye, a reset read as the end, and a reader without a limit.
///
/// Mutants planted: every error read as the end; the limit not applied; the limit off by one.
#[test]
fn an_answer_is_read_to_its_end_or_its_limit() {
    let whole = |script: Vec<io::Result<Vec<u8>>>, max| read_all(&mut Script(script), max);
    assert_eq!(whole(vec![Ok(b"ab".to_vec())], 8).unwrap(), b"ab");
    let no_goodbye = io::Error::from(io::ErrorKind::UnexpectedEof);
    assert_eq!(
        whole(vec![Ok(b"ab".to_vec()), Err(no_goodbye)], 8).unwrap(),
        b"ab"
    );
    let reset = io::Error::from(io::ErrorKind::ConnectionReset);
    let err = whole(vec![Ok(b"ab".to_vec()), Err(reset)], 8).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(whole(vec![Ok(b"abcd".to_vec())], 4).unwrap(), b"abcd");
    let err = whole(vec![Ok(b"abcde".to_vec())], 4).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

fn reachable(https: &Https, path: &str) -> io::Result<Reply> {
    https.send(&Request::get(path))
}

/// Proves a request reaches the server over TLS with the bearer token, the content type its
/// body has, and no ALPN but `http/1.1`, and that the answer comes back. Catches a token
/// left off, a release upload sent as JSON, and a session that offers h2 to a client that
/// speaks HTTP/1.1 only.
///
/// Mutants planted: the token left out; the content types swapped; `h2` offered.
#[test]
fn a_request_goes_over_tls_with_the_token_the_content_type_and_http1() {
    let f = farm(two_nodes());
    with_tls(&f, |https, served| {
        assert_eq!(reachable(https, "/v1/members").unwrap().status, 200);
        let post = Request::post_json("/v1/update/frontend", &json!({"commit": "x"}));
        assert_eq!(https.send(&post).unwrap().status, 400);
        let put = Request::put_raw("/v1/releases/x", b"abc".to_vec());
        assert_eq!(https.send(&put).unwrap().status, 201);
        let bearer = format!("Bearer {TOKEN}");
        assert_eq!(*served.auth.lock().unwrap(), vec![bearer; 3]);
        assert_eq!(
            *served.kinds.lock().unwrap(),
            ["", "application/json", "application/octet-stream"]
        );
        assert_eq!(
            *served.alpn.lock().unwrap(),
            vec![Some(b"http/1.1".to_vec()); 3]
        );
    });
    // The server saw the PUT's body, not just its head, and the POST's.
    assert_eq!(f.requests("PUT /v1/releases/x"), 1);
    assert_eq!(f.requests("POST /v1/update/frontend"), 1);
}

/// Proves a chunked answer and an answer whose connection closes without the TLS goodbye are
/// read, and an answer cut before its last byte is an error. Catches a client that needs a
/// `Content-Length` or the goodbye, and one that takes half an answer for the answer.
///
/// Mutants planted: chunked bodies refused; the missing goodbye an error; the length not checked.
#[test]
fn chunked_and_goodbyeless_answers_are_read_and_cut_ones_are_not() {
    let f = farm(two_nodes());
    with_tls(&f, |https, served| {
        let whole = reachable(https, "/v1/update").unwrap();
        served.chunked.store(true, SeqCst);
        assert_eq!(reachable(https, "/v1/update").unwrap(), whole);
        served.chunked.store(false, SeqCst);
        served.abrupt.store(true, SeqCst);
        assert_eq!(reachable(https, "/v1/update").unwrap(), whole);
        served.cut.store(true, SeqCst);
        let err = reachable(https, "/v1/update").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("Content-Length"), "{err}");
    });
}

fn client(addr: SocketAddr, trust: &Trust) -> Https {
    Https::new(addr, trust, TOKEN.to_owned(), SECS).unwrap()
}

/// Proves the certificate is verified: a CA that did not sign it, a name it does not carry,
/// and the address when no name is given (the test certificate has no IP) are each refused,
/// and the right CA with the right name is not. Catches a client that trusts any
/// certificate, ignores the name, or sends the token before it knows who it is talking to.
///
/// Mutants planted: verification off; the name taken from the address; the extra CA ignored.
#[test]
fn a_certificate_is_checked_for_its_ca_and_its_name() {
    let f = farm(two_nodes());
    with_tls(&f, |_, served| {
        let ca = served.ca.clone().into_bytes();
        let trust = |name: Option<&str>, ca: Vec<u8>| Trust {
            server_name: name.map(str::to_owned),
            ca_pem: Some(ca),
        };
        let good = client(served.addr, &trust(Some(NAME), ca.clone()));
        assert_eq!(reachable(&good, "/v1/members").unwrap().status, 200);

        let other_ca = pki(NAME).0.into_bytes();
        let wrong_ca = client(served.addr, &trust(Some(NAME), other_ca));
        let err = reachable(&wrong_ca, "/v1/members").unwrap_err();
        // The two test CAs have one name, so the issuer is found and its signature is wrong.
        assert!(err.to_string().contains("BadSignature"), "{err}");

        let wrong_name = client(served.addr, &trust(Some("other.example"), ca.clone()));
        let err = reachable(&wrong_name, "/v1/members").unwrap_err();
        assert!(
            err.to_string()
                .contains("not valid for name \"other.example\""),
            "{err}"
        );

        let by_address = client(served.addr, &trust(None, ca));
        let err = reachable(&by_address, "/v1/members").unwrap_err();
        assert!(
            err.to_string().contains("not valid for name \"127.0.0.1\""),
            "{err}"
        );
    });
    // The token went to nobody who failed: the farm answered only the good client.
    assert_eq!(f.requests("GET"), 1);
}

/// Proves a server that does not speak TLS, a port nothing listens on, and a server that
/// accepts and says nothing are each an error and not a hang. Catches a client that sends
/// the token in clear to a plain-text port and one that waits for ever.
///
/// Mutants planted: the read timeout dropped.
#[test]
fn a_server_without_tls_a_closed_port_and_a_silent_server_are_errors() {
    let (ca, ..) = pki(NAME);
    let trust = Trust {
        server_name: Some(NAME.to_owned()),
        ca_pem: Some(ca.into_bytes()),
    };
    let plain = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = plain.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = plain.accept().unwrap();
        let mut head = [0u8; 16];
        let _ = s.read(&mut head);
        let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
        head
    });
    let err = reachable(&client(addr, &trust), "/v1/members").unwrap_err();
    // What the client sent first is a TLS handshake, not the request or the token.
    assert_eq!(server.join().unwrap()[0], 0x16, "{err}");

    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let err = reachable(&client(addr, &trust), "/v1/members").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);

    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = silent.local_addr().unwrap();
    let quick = Https::new(addr, &trust, TOKEN.to_owned(), Duration::from_millis(300)).unwrap();
    let started = std::time::Instant::now();
    let err = reachable(&quick, "/v1/members").unwrap_err();
    assert!(
        matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ),
        "{err}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(silent);
}

/// Proves what the client refuses before it connects: an address that is not loopback, a
/// certificate name that is not a name, a CA file with no certificate or a certificate that
/// is not one, and a host that trusts nothing. Catches a token sent off the host and a
/// client that starts with no way to verify the server.
///
/// Mutants planted: the loopback check dropped; an empty root store accepted; the CA file
/// not required to hold a certificate.
#[test]
fn a_client_that_cannot_verify_or_may_not_dial_is_refused() {
    let (ca, ..) = pki(NAME);
    let der = CertificateDer::pem_slice_iter(ca.as_bytes())
        .next()
        .unwrap()
        .unwrap();
    let trust = |name: Option<&str>, ca: Option<&[u8]>| Trust {
        server_name: name.map(str::to_owned),
        ca_pem: ca.map(<[u8]>::to_vec),
    };
    let new = |addr: &str, trust: &Trust, system: Vec<CertificateDer<'static>>| {
        Https::with_roots(addr.parse().unwrap(), trust, system, "t".into(), SECS)
    };
    let own = trust(None, Some(ca.as_bytes()));
    assert!(new("127.0.0.1:1", &own, vec![]).is_ok());
    assert!(new("[::1]:1", &own, vec![]).is_ok());
    assert!(new("[::ffff:127.0.0.1]:1", &own, vec![]).is_ok());
    // The host's own roots are enough.
    assert!(new("127.0.0.1:1", &Trust::default(), vec![der]).is_ok());
    for (addr, trust, why) in [
        ("192.0.2.1:80", &own, "must be loopback"),
        ("[2001:db8::1]:80", &own, "must be loopback"),
        (
            "127.0.0.1:1",
            &trust(Some("not a name!"), Some(ca.as_bytes())),
            "not a DNS name",
        ),
        (
            "127.0.0.1:1",
            &trust(None, Some(b"not pem")),
            "holds no PEM certificate",
        ),
        (
            "127.0.0.1:1",
            &trust(
                None,
                Some(b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"),
            ),
            "",
        ),
        ("127.0.0.1:1", &Trust::default(), "trusts no root"),
    ] {
        let err = new(addr, trust, vec![]).err().unwrap();
        assert!(err.to_string().contains(why), "{addr}: {why}: {err}");
    }
}

/// Proves the command line dials the API as a TLS client with the CA and the certificate name
/// that its flags or, for what they leave out, the host's config give, a flag winning over
/// the config, and sends the token from its file. Catches a command line that builds the
/// client without the CA file, ignores the name, or lets the config override a flag.
///
/// Mutants planted: the `api_ca` key of the config not read; the config's name preferred to
/// the flag's; the token file not read.
#[test]
fn the_command_line_dials_the_api_under_the_ca_and_name_of_its_flags_or_its_config() {
    use std::os::unix::fs::PermissionsExt;

    use crate::deploy::cli;

    let f = farm(two_nodes());
    with_tls(&f, |_, served| {
        let (_, dir) = crate::testkit::host_root("cli-tls-api");
        let write = |name: &str, text: &str, mode: u32| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path.to_str().unwrap().to_owned()
        };
        let ca = write("ca.pem", &served.ca, 0o644);
        let token = write("token", &format!("{TOKEN}\n"), 0o600);
        let config = |name: &str| {
            write(
                "deploy.json",
                &format!(
                    "{{\"api_ca\": {ca:?}, \"api_server_name\": {name:?}, \"token_file\": {token:?}}}"
                ),
                0o644,
            )
        };
        let lock = dir.join("run/lock");
        let addr = served.addr.to_string();
        let run = |config: &str, extra: &[&str]| -> (i32, String) {
            let mut args = vec![
                "--commit",
                TARGET,
                "--config",
                config,
                "--lock",
                lock.to_str().unwrap(),
                "--api",
                &addr,
                // The fixture release is complete for the platforms of `two_nodes()`; the
                // frontend host's own platform must be one of them whatever machine runs this.
                "--platform",
                "linux-x86_64",
            ];
            args.extend_from_slice(extra);
            let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = cli::main_as(&args, &mut &b""[..], &mut out, &mut err, false);
            (code, String::from_utf8(out).unwrap())
        };
        // The CA, the name and the token from the config.
        let (code, out) = run(&config(NAME), &[]);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("\"result\":\"plan\""), "{out}");
        // A config with another name: the certificate is not for it.
        let wrong = config("other.example");
        let (code, out) = run(&wrong, &[]);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("\"reason\":\"api_error\""), "{out}");
        // The flag wins over the config's name, and the flags need no config for the rest.
        let (code, out) = run(&wrong, &["--api-server-name", NAME]);
        assert_eq!(code, 0, "{out}");
        let all = [
            "--api-ca",
            ca.as_str(),
            "--api-server-name",
            NAME,
            "--token-file",
            token.as_str(),
            "--cas",
            "http://127.0.0.1:1",
            "--cas-ca",
            ca.as_str(),
            "--cas-server-name",
            NAME,
        ];
        let (code, out) = run("/no/such/deploy.json", &all);
        assert_eq!(code, 0, "{out}");
        let bearer = format!("Bearer {TOKEN}");
        let seen = served.auth.lock().unwrap();
        assert!(!seen.is_empty() && seen.iter().all(|a| *a == bearer));
    });
}
