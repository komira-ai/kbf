//! A daemon tells its runtimes what every Welcome says of placement by measured load
//! (`Welcome.placement_load`), on each stream and before that stream's first Heartbeat.

mod support;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{Welcome, server_message};
use support::{Harness, INTERVAL, Peer};

/// A runtime that records what it is told and runs nothing.
#[derive(Default)]
struct Told(Mutex<Vec<bool>>);

impl Told {
    fn all(&self) -> Vec<bool> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Runtime for Told {
    fn driver(&self) -> &'static str {
        "told"
    }

    fn serves(&self, kind: &str) -> bool {
        kind == "action"
    }

    async fn run(&self, _work: Work) -> Result<ActionResult, RuntimeError> {
        Ok(ActionResult::default())
    }

    async fn kill(&self, _lease_id: kbf_types::LeaseId) {}

    async fn welcomed_placement_load(&self, on: bool) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(on);
    }
}

/// Asserts that each Welcome is passed to the runtime once, as true only when it says true:
/// absent and false are both false, a reconnect with another mode is told again, and the
/// runtime has been told by the time the stream's first Heartbeat arrives (the Heartbeat
/// reports what the runtime wrote). Catches a daemon that tells the runtime only at the first
/// Welcome (a reconnect to a server with another mode would keep the old limit), absent read
/// as true, and a runtime told after the first Heartbeat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_welcome_tells_the_runtime_the_mode() {
    let told = Arc::new(Told::default());
    let mut h =
        Harness::with_runtime("placement-load", Arc::clone(&told), Duration::from_secs(40)).await;
    let mut expected = Vec::new();
    for (sent, heard) in [
        (Some(true), true),
        (None, false),
        (Some(false), false),
        (Some(true), true),
    ] {
        let mut peer = h.session().await;
        peer.hello().await;
        assert_eq!(told.all(), expected, "nothing is told before a Welcome");
        welcome_placement_load(&peer, sent);
        peer.heartbeat().await;
        expected.push(heard);
        assert_eq!(told.all(), expected, "Welcome {sent:?}");
        peer.close();
    }
}

/// A Welcome that carries `placement_load` as given (`None`: absent).
fn welcome_placement_load(peer: &Peer, placement_load: Option<bool>) {
    peer.send(server_message::Message::Welcome(Welcome {
        protocol_version: 1,
        heartbeat_interval_ms: INTERVAL.as_millis() as u64,
        placement_load,
        ..Welcome::default()
    }));
}
