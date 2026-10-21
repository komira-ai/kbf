//! The registered store's secret in the process's logs, in a test binary of its own: the
//! log is read through a global subscriber at TRACE, which no other test's events can
//! reach or race.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/store_config.rs"]
mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use kbf_map::DenyEntry;
use support::{SENTINEL, Source, cold, farm, membership, registration, want};

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Catches: the secret printed by a trace of the transport or the service (tonic, hyper,
/// h2, tower and this crate, all at TRACE), by a failure path (a denied node, a refused
/// role, a secret that cannot be read) or by a `Debug` of the answer, which the log lines
/// of the call paths reach. The test first asserts the secret is in the answer it fetched,
/// so a scan that finds nothing is not a scan of nothing.
///
/// Mutant seen red: `Debug` of the store printing its secret (the hand-written one prints
/// `Secret(..)`); the log line the test writes carries the answer's `Debug`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_secret_is_in_the_answer_and_in_no_log_line() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let source = Source::new(9);
    let farm = farm(source.clone(), false).await;
    let channel = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    membership(&channel)
        .register(registration("node-a"))
        .await
        .expect("registers");
    let got = cold(&channel)
        .get_store_config(want(0))
        .await
        .expect("fetched")
        .into_inner();
    assert_eq!(
        got.config.as_ref().expect("a store").secret_access_key,
        SENTINEL
    );
    tracing::info!("the fetched answer: {got:?}");
    cold(&channel)
        .get_store_config(want(9))
        .await
        .expect("unchanged");

    let daemon = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("daemon"), &["node-a"]))
        .await;
    cold(&daemon)
        .get_store_config(want(0))
        .await
        .expect_err("role");

    farm.members
        .deny(&DenyEntry::Node("node-a".to_owned()), "revoked")
        .expect("deny");
    cold(&channel)
        .get_store_config(want(0))
        .await
        .expect_err("denied");
    source.break_secret();
    let other = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-b"]))
        .await;
    cold(&other)
        .get_store_config(want(0))
        .await
        .expect_err("unreadable");

    let text = String::from_utf8_lossy(&log.0.lock().expect("log")).into_owned();
    assert!(
        text.contains("the fetched answer"),
        "the log is being read: {} bytes",
        text.len()
    );
    assert!(
        text.contains("Secret(..)"),
        "the answer's Debug is in the log, redacted"
    );
    assert!(!text.contains(SENTINEL), "the secret reached the log");
}
