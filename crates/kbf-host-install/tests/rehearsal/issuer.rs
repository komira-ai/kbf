//! A stand-in for the two things of GitHub the release's gamma job talks to, on loopback over
//! TLS: the OIDC issuer (its discovery document and its keys, which the coordinator fetches to
//! check a token) and the runner's token service (which `cargo xtask deploy-farm` asks for a
//! token for an audience). The tokens are the testkit's: RS256, signed by a fixture key the
//! issuer publishes, with the claims of a `workflow_run` of the release workflow on `main`.
//!
//! The coordinator and `deploy-farm` are not told anything but the issuer's address and the
//! one certificate that vouches for it, as they would be for GitHub's: the same code runs
//! against the real issuer in the release.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use kbf_oidc::testkit::{jwks_of, keys, mint};
use serde_json::json;

use super::farm::REPOSITORY_ID;

/// The bearer the token service wants, as the runner's request token is.
pub const REQUEST_TOKEN: &str = "rehearsal-request-token";

/// A running issuer and token service: they stop with the process.
pub struct Issuer {
    /// `https://localhost:<port>`: the issuer, as the allow-list names it.
    pub url: String,
    /// The certificate that vouches for it, PEM: the one root of `SSL_CERT_FILE`.
    pub cert_pem: String,
}

impl Issuer {
    /// Where the token service asks: `<url>/token`.
    pub fn token_url(&self) -> String {
        format!("{}/token", self.url)
    }

    pub fn start() -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let cert_pem = certified.cert.pem();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key)
            .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let issuer = url.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (config, issuer) = (Arc::clone(&config), issuer.clone());
                std::thread::spawn(move || {
                    let _ = serve(&config, stream, &issuer);
                });
            }
        });
        Self { url, cert_pem }
    }
}

/// The path and the `Authorization` header of the request in `head`.
fn parse(head: &str) -> (String, Option<String>) {
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let bearer = head.lines().find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().strip_prefix("Bearer ").map(str::to_owned))?
    });
    (path, bearer)
}

/// The value of `audience` in a query, percent-decoded.
fn audience(query: &str) -> Option<String> {
    let raw = query.split('&').find_map(|p| p.strip_prefix("audience="))?;
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A token for `audience`, as a `workflow_run` of the release workflow on `main` in the
/// `release` environment holds one, valid now.
fn token(issuer: &str, audience: &str) -> String {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let workflow = format!(
        "{}/.github/workflows/release.yml@refs/heads/main",
        super::REPO
    );
    let claims = json!({
        "iss": issuer, "aud": audience,
        "sub": format!("repo:{}:environment:release", super::REPO),
        "exp": now + 300, "iat": now, "nbf": now,
        "jti": format!("rehearsal-{}-{now}", SERIAL.fetch_add(1, Ordering::SeqCst)),
        "repository_id": REPOSITORY_ID, "repository": super::REPO,
        "job_workflow_ref": workflow, "ref": "refs/heads/main", "environment": "release",
        "event_name": "workflow_run", "runner_environment": "github-hosted",
        "sha": "0123456789abcdef0123456789abcdef01234567",
    });
    mint(&keys()[0], &claims)
}

fn serve(
    config: &Arc<rustls::ServerConfig>,
    mut tcp: std::net::TcpStream,
    issuer: &str,
) -> std::io::Result<()> {
    let mut conn =
        rustls::ServerConnection::new(Arc::clone(config)).map_err(std::io::Error::other)?;
    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tls.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let (target, bearer) = parse(&String::from_utf8_lossy(&head));
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let (status, body) = match path {
        "/.well-known/openid-configuration" => (
            "200 OK",
            json!({"issuer": issuer, "jwks_uri": format!("{issuer}/.well-known/jwks")}).to_string(),
        ),
        "/.well-known/jwks" => ("200 OK", jwks_of(&[&keys()[0]])),
        "/token" if bearer.as_deref() != Some(REQUEST_TOKEN) => ("401 Unauthorized", String::new()),
        "/token" => match audience(query) {
            Some(aud) => ("200 OK", json!({"value": token(issuer, &aud)}).to_string()),
            None => ("400 Bad Request", String::new()),
        },
        _ => ("404 Not Found", String::new()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(response.as_bytes())?;
    tls.conn.send_close_notify();
    tls.flush()
}
