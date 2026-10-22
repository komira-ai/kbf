//! The HTTPS client of kbf-proxy's port: what it trusts, what it refuses at start, and what it
//! makes of a connection that ends in each way.

use std::time::Duration;

use crate::https::{CA_FLAG, ProxyApi, ProxyTls};
use crate::ports::PortErrorKind;
use crate::testutil::{FakeProxy, TOKEN, fake_proxy, file, http, pki, scratch};

const NAME: &str = "proxy.example";

fn tls(proxy: &FakeProxy, name: Option<&str>) -> ProxyTls {
    ProxyTls {
        addr: proxy.addr,
        ca_file: file(&scratch("https-ca"), "ca.pem", &proxy.ca, 0o644),
        server_name: name.map(str::to_owned),
    }
}

fn token() -> std::path::PathBuf {
    file(&scratch("https-tok"), "token", TOKEN, 0o600)
}

fn api(tls: &ProxyTls) -> ProxyApi {
    ProxyApi::new(tls, token())
        .expect("api")
        .with_timeout(Duration::from_millis(500))
}

/// Catches a client that does not verify the server by the CA file and the name: the right
/// pair gets the answer; a CA that did not sign the certificate, and a name the certificate
/// does not carry, are refused as an answer that says no (`Other`, naming the flags), not as
/// a connection that failed; the system's roots are not consulted (an empty CA file is no
/// client at all, below).
#[test]
fn the_server_is_verified_against_the_ca_file_and_the_name() {
    let proxy = fake_proxy(NAME, true, |_, _| Some(http(200, "{\"ok\":true}")));
    let reply = api(&tls(&proxy, Some(NAME))).get("/x").expect("reply");
    assert_eq!((reply.status, reply.body), (200, b"{\"ok\":true}".to_vec()));

    let mut elsewhere = tls(&proxy, Some(NAME));
    elsewhere.ca_file = file(&scratch("https-other"), "ca.pem", &pki(NAME).0, 0o644);
    let wrong_name = tls(&proxy, Some("other.example"));
    for bad in [elsewhere, wrong_name] {
        let e = api(&bad).get("/x").expect_err("refused");
        assert_eq!(e.kind(), PortErrorKind::Other, "{e}");
        assert!(
            e.to_string().contains("TLS was refused") && e.to_string().contains(CA_FLAG),
            "{e}"
        );
    }
    // Nothing reached the application after a refused handshake.
    assert_eq!(proxy.requests().len(), 1);
}

/// Catches the name defaulting to something but the address: with no `--proxy-server-name`
/// the certificate must carry the IP of the address.
#[test]
fn without_a_name_the_certificate_must_carry_the_ip() {
    let proxy = fake_proxy("127.0.0.1", true, |_, _| Some(http(200, "{}")));
    assert_eq!(
        api(&tls(&proxy, None)).get("/x").expect("reply").status,
        200
    );
    let named = fake_proxy(NAME, true, |_, _| Some(http(200, "{}")));
    let e = api(&tls(&named, None)).get("/x").expect_err("refused");
    assert_eq!(e.kind(), PortErrorKind::Other, "{e}");
}

/// Catches a start-up that goes on with a CA file or a name or a token it cannot use: each is
/// an error at once, naming the flag and holding neither the path nor the token.
#[test]
fn start_up_refuses_a_ca_file_a_name_or_a_token_it_cannot_use() {
    let proxy = fake_proxy(NAME, true, |_, _| Some(http(200, "{}")));
    let dir = scratch("https-bad");
    let garbage_cert = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    let broken_pem = "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
    for (name, content, why) in [
        ("empty.pem", "", "it holds no certificate"),
        ("text.pem", "just words\n", "it holds no certificate"),
        ("broken.pem", broken_pem, "it is not PEM"),
        (
            "garbage.pem",
            garbage_cert,
            "it holds a certificate that cannot be a trust anchor",
        ),
    ] {
        let mut t = tls(&proxy, Some(NAME));
        t.ca_file = file(&dir, name, content, 0o644);
        let e = ProxyApi::new(&t, token()).err().expect(name).to_string();
        assert_eq!(e, format!("{CA_FLAG}: {why}"), "{name}");
    }
    let mut missing = tls(&proxy, Some(NAME));
    missing.ca_file = dir.join("absent.pem");
    let e = ProxyApi::new(&missing, token())
        .err()
        .expect("missing")
        .to_string();
    assert_eq!(e, format!("{CA_FLAG}: entity not found"));
    assert!(!e.contains("absent"));

    let e = ProxyApi::new(&tls(&proxy, Some("not a name!")), token())
        .err()
        .expect("name")
        .to_string();
    assert_eq!(e, "--proxy-server-name must be a DNS name or an IP");

    let short = file(&dir, "short-token", "short", 0o600);
    let e = ProxyApi::new(&tls(&proxy, Some(NAME)), short)
        .err()
        .expect("token")
        .to_string();
    assert!(e.contains("32 to 4096") && !e.contains("short\n"), "{e}");
}

/// Catches the token read once at start and then kept: a token that goes bad later is refused
/// at the call, with the flag in the text, and the request is not sent.
#[test]
fn the_token_is_read_on_every_call() {
    let proxy = fake_proxy(NAME, true, |_, _| Some(http(200, "{}")));
    let dir = scratch("https-rot");
    let path = file(&dir, "token", TOKEN, 0o600);
    let client = ProxyApi::new(&tls(&proxy, Some(NAME)), path.clone()).expect("api");
    assert_eq!(client.get("/x").expect("reply").status, 200);
    std::fs::write(&path, "short").expect("rotate");
    let e = client.get("/x").expect_err("bad token");
    assert!(e.to_string().contains("--token-file"), "{e}");
    assert_eq!(proxy.requests().len(), 1);
}

/// Catches the end of a connection mistaken: a server that closes the TCP connection without
/// the TLS goodbye after a whole answer is an answer; one that does so in the middle of the
/// body, by its `Content-Length`, is no answer (`Unreachable`).
#[test]
fn a_close_without_the_goodbye_is_an_answer_only_if_it_is_whole() {
    let whole = fake_proxy(NAME, false, |_, _| Some(http(200, "{\"ok\":true}")));
    assert_eq!(
        api(&tls(&whole, Some(NAME)))
            .get("/x")
            .expect("reply")
            .status,
        200
    );
    let cut = fake_proxy(NAME, false, |_, _| {
        Some(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n{\"ok\"".to_vec())
    });
    let e = api(&tls(&cut, Some(NAME))).get("/x").expect_err("short");
    assert_eq!(e.kind(), PortErrorKind::Unreachable, "{e}");
    assert!(e.to_string().contains("closed the answer short"), "{e}");
}
