//! The registered store's secret in the logs of a storage node that applies it, in a test
//! binary of its own: the log is read through a global subscriber at TRACE (tonic, hyper,
//! h2, tower, reqwest and this crate), which no other test's events can reach or race.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/cold_node.rs"]
mod node;
#[path = "support/store_config.rs"]
mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use kbf_objstore::s3::fake::FakeS3;
use node::{
    BEAT, ID, Rig, bucket, flags, no_keys, own_keys, registered, reported, round_trip, started,
};
use support::{Source, farm_beating};

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

/// Catches the secret printed by the fetch, the probe, the store client or a failure
/// path of a node: three nodes start at TRACE, one on the registered keys, one whose
/// registered keys fail the probe (the failure path), and one that is refused for want of
/// keys of its own (the error path), and the answer's own `Debug` is written to the log
/// too. The test first asserts that the secret signed the requests the service took (the
/// node holds it) and that the log is being read, so a scan that finds nothing is not a
/// scan of nothing.
///
/// Mutants: `registered_store` logging the config with `{:?}` of a copy that prints the
/// secret (the proto's `Debug` prints `Secret(..)`, so the mutant writes the field), and
/// the probe failure carrying the store's message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_secret_is_in_the_node_and_in_no_log_line() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let secret = "SENTINEL-registered-secret-0e5d";
    let s3 = FakeS3::start().await;
    s3.require_credentials(ID, secret);
    let source = Source::new(2);
    source.serve(registered(&s3, "bkt", ID, secret));
    let farm = farm_beating(source.clone(), false, None, BEAT * 3).await;
    let rig = Rig { farm, s3, source };

    let half = started(&flags(&rig, "log-ok", &bucket(&rig), no_keys))
        .await
        .expect("starts on the registration");
    round_trip(&half, "written on the registered keys").await;
    assert!(
        rig.s3.requests().iter().any(|r| r.starts_with("PUT")),
        "the service took signed writes, so the node holds the secret"
    );
    let _serving = tokio::spawn(half.serve());
    reported(&rig, 2).await;

    // The failure paths: the same secret is now wrong for the service (it takes `ID` with
    // another secret), so the probe fails, once with a pair of the node's own to fall back
    // to and once without.
    rig.s3.require_credentials(ID, "another-secret");
    let fell_back = started(&flags(&rig, "log-bad", &bucket(&rig), |name| match name {
        "AWS_SECRET_ACCESS_KEY" => Some("another-secret".to_owned()),
        other => own_keys(other),
    }))
    .await
    .expect("keeps its own store");
    drop(fell_back);
    let Err(refused) = started(&flags(&rig, "log-none", &bucket(&rig), no_keys)).await else {
        panic!("a node with nothing that works starts");
    };
    let refused = format!("{refused} {refused:?}");

    let text = String::from_utf8_lossy(&log.0.lock().expect("log")).into_owned();
    assert!(
        text.contains("registered") && text.len() > 1000,
        "the log is being read: {} bytes",
        text.len()
    );
    assert!(!text.contains(secret), "the secret reached the log");
    assert!(!refused.contains(secret), "the secret reached the error");
}
