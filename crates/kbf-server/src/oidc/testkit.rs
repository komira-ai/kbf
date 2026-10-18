//! A fake GitHub Actions issuer for tests: RSA keys generated here, a TLS server that
//! serves a discovery document and a JWKS, and a token minter. Nothing is shared with
//! the code under test but its public interface.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rsa::pkcs8::EncodePrivateKey as _;
use rsa::traits::PublicKeyParts as _;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_rustls::TlsAcceptor;

use super::{Clock, HttpsKeys, OidcConfig, Roots, Verifier};

/// The fixed "now" of the tests, in unix seconds.
pub(crate) const NOW: u64 = 1_800_000_000;

pub(crate) const AUDIENCE: &str = "kbf-gamma";
pub(crate) const WORKFLOW: &str =
    "komira-ai/komira-build-farm/.github/workflows/gamma.yml@refs/heads/main";

/// An RSA signing key and the JWK that publishes it.
pub(crate) struct TestKey {
    pub kid: String,
    pair: RsaKeyPair,
    n: Vec<u8>,
    e: Vec<u8>,
}

impl TestKey {
    fn generate(kid: &str) -> Self {
        let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("a key");
        let der = private.to_pkcs8_der().expect("PKCS#8");
        Self {
            kid: kid.to_owned(),
            pair: RsaKeyPair::from_pkcs8(der.as_bytes()).expect("ring reads the key"),
            n: private.n().to_bytes_be(),
            e: private.e().to_bytes_be(),
        }
    }

    pub(crate) fn jwk(&self) -> Value {
        json!({"kty": "RSA", "kid": self.kid, "use": "sig", "alg": "RS256",
               "n": B64.encode(&self.n), "e": B64.encode(&self.e)})
    }

    pub(crate) fn n(&self) -> &[u8] {
        &self.n
    }

    fn sign(&self, input: &str) -> Vec<u8> {
        let mut out = vec![0; self.pair.public().modulus_len()];
        self.pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut out,
            )
            .expect("a signature");
        out
    }
}

/// Three keys, generated once per test binary (2048-bit generation is slow in a debug build):
/// the issuer's, a second one for a forged signature, and one for a rotation.
pub(crate) fn keys() -> &'static [TestKey; 3] {
    static KEYS: OnceLock<[TestKey; 3]> = OnceLock::new();
    KEYS.get_or_init(|| {
        let handles: Vec<_> = ["k1", "k2", "k3"]
            .into_iter()
            .map(|kid| std::thread::spawn(move || TestKey::generate(kid)))
            .collect();
        let mut made = handles.into_iter().map(|h| h.join().expect("keygen"));
        [
            made.next().expect("k1"),
            made.next().expect("k2"),
            made.next().expect("k3"),
        ]
    })
}

pub(crate) fn jwks_of(keys: &[&TestKey]) -> String {
    json!({"keys": keys.iter().map(|k| k.jwk()).collect::<Vec<_>>()}).to_string()
}

/// What the fake issuer serves and what it saw.
pub(crate) struct Served {
    pub jwks: String,
    /// Answer every request 503.
    pub fail: bool,
    /// The `jwks_uri` the discovery document names, if not the issuer's own.
    pub jwks_uri: Option<String>,
    /// How many times the JWKS was requested.
    pub jwks_requests: usize,
}

/// A TLS server on loopback acting as `token.actions.githubusercontent.com`.
pub(crate) struct FakeIssuer {
    pub url: String,
    pub roots: Roots,
    pub served: Arc<Mutex<Served>>,
}

impl FakeIssuer {
    pub(crate) async fn start(jwks: String) -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("a certificate");
        let der = certified.cert.der().clone();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key)
            .expect("a server config");
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let url = format!("https://localhost:{port}");
        let served = Arc::new(Mutex::new(Served {
            jwks,
            fail: false,
            jwks_uri: None,
            jwks_requests: 0,
        }));
        let (state, issuer) = (Arc::clone(&served), url.clone());
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let (acceptor, state, issuer) =
                    (acceptor.clone(), Arc::clone(&state), issuer.clone());
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head);
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let (status, body) = {
                        let mut s = state.lock().expect("state");
                        if s.fail {
                            ("503 Service Unavailable", String::new())
                        } else if path == "/.well-known/openid-configuration" {
                            let uri = s
                                .jwks_uri
                                .clone()
                                .unwrap_or_else(|| format!("{issuer}/.well-known/jwks"));
                            (
                                "200 OK",
                                json!({"issuer": issuer, "jwks_uri": uri}).to_string(),
                            )
                        } else if path == "/.well-known/jwks" {
                            s.jwks_requests += 1;
                            ("200 OK", s.jwks.clone())
                        } else {
                            ("404 Not Found", String::new())
                        }
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = tls.write_all(response.as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        Self {
            url,
            roots: Roots::Given(vec![der]),
            served,
        }
    }

    pub(crate) fn set_jwks(&self, jwks: String) {
        self.served.lock().expect("state").jwks = jwks;
    }

    pub(crate) fn set_fail(&self, fail: bool) {
        self.served.lock().expect("state").fail = fail;
    }

    pub(crate) fn jwks_requests(&self) -> usize {
        self.served.lock().expect("state").jwks_requests
    }
}

/// The allow-list text for `issuer`.
pub(crate) fn config_text(issuer: &str) -> String {
    format!(
        r#"
[remote_operator]
issuer = "{issuer}"
audience = "{AUDIENCE}"
repository_id = "4242"
repository = "komira-ai/komira-build-farm"
job_workflow_ref = ["{WORKFLOW}"]
refs = ["refs/heads/main"]
environments = ["gamma"]
frontend_image_repository = "registry.example/kbf/frontend"
"#
    )
}

/// A settable clock.
pub(crate) fn clock() -> (Clock, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(NOW));
    let shared = Arc::clone(&now);
    (Arc::new(move || shared.load(Ordering::SeqCst)), now)
}

/// A verifier over `issuer`, with a 30 s refetch interval and a clock the test sets.
pub(crate) fn verifier(issuer: &FakeIssuer) -> (Arc<Verifier>, Arc<AtomicU64>) {
    let config = OidcConfig::parse(&config_text(&issuer.url)).expect("a config");
    let source = HttpsKeys::new(
        &issuer.url,
        issuer.roots.clone(),
        std::time::Duration::from_secs(5),
    )
    .expect("a source");
    let (clock, now) = clock();
    (
        Arc::new(Verifier::new(
            config,
            Arc::new(source),
            std::time::Duration::from_secs(30),
            clock,
        )),
        now,
    )
}

/// The claims of a token CI would send, valid at [`NOW`] for the issuer at `issuer`.
pub(crate) fn good_claims(issuer: &str) -> Value {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    json!({
        "iss": issuer, "aud": AUDIENCE, "sub": "repo:komira-ai/komira-build-farm:environment:gamma",
        "exp": NOW + 300, "iat": NOW, "nbf": NOW,
        "jti": format!("jti-{}", SERIAL.fetch_add(1, Ordering::SeqCst)),
        "repository_id": "4242", "repository": "komira-ai/komira-build-farm",
        "job_workflow_ref": WORKFLOW, "ref": "refs/heads/main", "environment": "gamma",
        "event_name": "push", "runner_environment": "github-hosted",
        "sha": "0123456789abcdef0123456789abcdef01234567",
    })
}

/// `claims` signed by `key` under its own `kid`.
pub(crate) fn mint(key: &TestKey, claims: &Value) -> String {
    mint_with(
        key,
        &json!({"alg": "RS256", "typ": "JWT", "kid": key.kid}),
        claims,
    )
}

/// `claims` under `header`, signed by `key` as RS256 whatever the header says.
pub(crate) fn mint_with(key: &TestKey, header: &Value, claims: &Value) -> String {
    let input = format!(
        "{}.{}",
        B64.encode(header.to_string()),
        B64.encode(claims.to_string())
    );
    let signature = key.sign(&input);
    format!("{input}.{}", B64.encode(signature))
}

/// A token whose header and signature are whatever the caller makes them.
pub(crate) fn mint_raw(header: &Value, claims: &Value, signature: &[u8]) -> String {
    format!(
        "{}.{}.{}",
        B64.encode(header.to_string()),
        B64.encode(claims.to_string()),
        B64.encode(signature)
    )
}
