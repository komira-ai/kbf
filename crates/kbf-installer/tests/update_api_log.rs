//! The update API's bearer check logs a denial and never a credential.
//!
//! This is a test binary of its own, with one test: `tracing` caches whether a call site
//! is enabled, and a test that installs a subscriber while other tests in the same process
//! reach the same call site without one sees that cache race.

use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use hyper::Request;
use kbf_installer::proxy::api::auth::{
    Authorize, BearerAuth, RateLimit, Secret, Sha256Hasher, TokenSource,
};
use kbf_installer::proxy::api::traits::Clock;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Tokens(Mutex<Option<String>>);

impl TokenSource for Tokens {
    fn token(&self) -> Option<Secret> {
        self.0.lock().unwrap().clone().map(Secret::new)
    }
}

struct Frozen;

impl Clock for Frozen {
    fn mono_ms(&self) -> u64 {
        0
    }

    fn wall_s(&self) -> u64 {
        0
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

fn request(uri: &str, headers: &[&str]) -> Request<Bytes> {
    let mut b = Request::builder().uri(uri);
    for h in headers {
        b = b.header("authorization", *h);
    }
    b.body(Bytes::new()).unwrap()
}

#[test]
fn no_token_reaches_the_log() {
    let capture = Capture::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    )
    .unwrap();
    let tokens = Arc::new(Tokens(Mutex::new(Some(TOKEN.to_owned()))));
    struct Shared(Arc<Tokens>);
    impl TokenSource for Shared {
        fn token(&self) -> Option<Secret> {
            self.0.token()
        }
    }
    let auth = BearerAuth::new(
        Box::new(Shared(tokens.clone())),
        Box::new(Sha256Hasher),
        Arc::new(Frozen),
        RateLimit {
            burst: 6,
            per_second: 1,
        },
    );
    let wrong = "SECRETWRONGTOKEN0123456789abcdef0123456789abcdef";
    let path = "/v1/update/frontend";
    let bearer = |t: &str| format!("Bearer {t}");
    let _ = auth.authorize(&request(path, &[&bearer(wrong)]));
    let _ = auth.authorize(&request(path, &[&bearer(TOKEN), &bearer(wrong)]));
    let _ = auth.authorize(&request(
        &format!("{path}?token={TOKEN}"),
        &[&bearer(wrong)],
    ));
    let _ = auth.authorize(&request(path, &["Basic SECRETBASIC"]));
    let _ = auth.authorize(&request(path, &[]));
    *tokens.0.lock().unwrap() = None;
    let _ = auth.authorize(&request(path, &[&bearer(TOKEN)]));
    // The seventh call is rate limited.
    let _ = auth.authorize(&request(path, &[&bearer(TOKEN)]));
    assert_eq!(format!("{:?}", Secret::new(TOKEN)), "Secret(..)");

    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    for reason in [
        "wrong token",
        "token in the query string",
        "not a bearer",
        "no credentials",
        "no token is configured",
        "rate limited",
    ] {
        assert!(log.contains(reason), "{reason:?} is not logged:\n{log}");
    }
    assert!(log.contains(path));
    for secret in [TOKEN, wrong, "SECRETBASIC", "Bearer", "token="] {
        assert!(!log.contains(secret), "{secret:?} is in the log:\n{log}");
    }
}
