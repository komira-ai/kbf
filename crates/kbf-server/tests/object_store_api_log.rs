//! What the object-store routes log, at TRACE, while handling a secret. One test in its own
//! binary: the subscriber is global, because the routes run on the blocking pool's threads.

#[path = "object_store_api/harness.rs"]
mod harness;

use std::io;
use std::sync::{Arc, Mutex};

use harness::{BIND_BODY, Farm, SENTINEL, conn_body};

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lines").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Catches a handler that logs the request (the body holds the secret key), a parse error, or a
// store's error text, at any level a subscriber can ask for: the calls below carry the
// sentinel in every way a request can, and succeed, are refused and fail inside the store.
// The scan is not vacuous: a failing call does log its closed code.
#[tokio::test(flavor = "multi_thread")]
async fn the_routes_log_no_request_body_and_no_secret() {
    let lines = Lines::default();
    let writer = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let farm = Farm::new();
    let put = |name: &str| format!("/v1/object-stores/connections/{name}");
    assert_eq!(
        farm.call("PUT", &put("cold"), &conn_body(SENTINEL))
            .await
            .status,
        201
    );
    assert_eq!(
        farm.call("PUT", "/v1/object-stores/bindings/cold", BIND_BODY)
            .await
            .status,
        201
    );
    assert_eq!(
        farm.call("POST", &format!("{}:verify", put("cold")), "")
            .await
            .status,
        200
    );
    let wrong = conn_body(SENTINEL).replace("}", &format!(r#","conditional_put":"{SENTINEL}"}}"#));
    assert_eq!(farm.call("PUT", &put("cold"), &wrong).await.status, 400);
    let big = conn_body(SENTINEL).replace("}", &format!(r#","region":"{}"}}"#, "x".repeat(20_000)));
    assert_eq!(farm.call("PUT", &put("cold"), &big).await.status, 413);
    assert_eq!(
        farm.call(
            "PUT",
            &put("cold"),
            &conn_body(&format!("{SENTINEL}-2")).replace("s3.example", "elsewhere.example")
        )
        .await
        .status,
        409
    );
    farm.probe.fail_at_get();
    assert_eq!(
        farm.call("PUT", &put("cold"), &conn_body(&format!("{SENTINEL}-3")))
            .await
            .status,
        422
    );
    farm.flaky
        .fail_writes
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let down = farm
        .call(
            "PUT",
            &put("cold"),
            &conn_body(&format!("{SENTINEL}-4")).replace("}", r#","verify":false}"#),
        )
        .await;
    assert_eq!(down.status, 503, "{}", down.body);

    let text = String::from_utf8(lines.0.lock().expect("lines").clone()).expect("UTF-8");
    assert!(
        text.contains("state_unavailable"),
        "a failing call logs its code: {text:?}"
    );
    assert!(!text.contains("SENTINEL"), "{text}");
    assert!(!text.contains("secret_access_key"), "{text}");
}
