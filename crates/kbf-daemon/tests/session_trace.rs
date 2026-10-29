//! One scripted session as text: Hello, Welcome, three heartbeats, a Start, its Result and
//! the acknowledgement, compared with a golden file.
//!
//! The text lists what crosses the stream in the order the script drives it, then the
//! events the daemon emitted. It leaves out what differs between hosts and runs (the
//! capabilities, the instance id, the build, times). Catches a change to the session
//! that reorders or reshapes what the daemon sends or emits for the same script, which no
//! test of one message would notice. The golden was captured by running the code before
//! the session hooks moved behind a trait, and is committed with that change.

mod support;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{
    Daemon, DaemonConfig, Event, FakeRuntime, NoExtras, NodeReport, Runtime, Runtimes,
};
use kbf_proto::worker::daemon_message;
use support::{Harness, PROMPT, Peer};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const GOLDEN: &str = include_str!("golden/session_trace.txt");

fn lease(id: Option<&kbf_proto::worker::LeaseId>) -> String {
    id.map_or("none".to_owned(), |l| format!("{}.{}", l.term, l.seq))
}

/// One line for a message the daemon sent. `hello_hash` is the report hash of the Hello.
fn daemon_line(message: &daemon_message::Message, hello_hash: &[u8]) -> String {
    match message {
        daemon_message::Message::Hello(h) => format!(
            "daemon> Hello node={} protocol={} phase={} instance_len={} build={} capabilities={}",
            h.node_id,
            h.protocol_version,
            h.phase.as_ref().map_or("serving", |_| "other"),
            h.instance_id.len(),
            h.daemon_version == kbf_daemon::DAEMON_VERSION,
            !h.capabilities.is_empty(),
        ),
        daemon_message::Message::NodeStatus(s) => format!(
            "daemon> NodeStatus xcodes={} images={}",
            s.xcodes.len(),
            s.container_images.len()
        ),
        daemon_message::Message::Heartbeat(h) => format!(
            "daemon> Heartbeat seq={} running=[{}] report_hash_is_hellos={} pack_reads={} pack_writes={}",
            h.seq,
            h.running
                .iter()
                .map(|l| lease(Some(l)))
                .collect::<Vec<_>>()
                .join(","),
            h.report_hash == hello_hash,
            h.pack_reads.is_some(),
            h.pack_writes.is_some(),
        ),
        daemon_message::Message::Result(r) => format!(
            "daemon> Result lease={} code={} action_echoed={}",
            lease(r.lease_id.as_ref()),
            r.status.as_ref().map_or(-1, |s| s.code),
            r.action_digest == Some(support::digest()),
        ),
        other => format!("daemon> {other:?}"),
    }
}

fn event_line(event: &Event) -> String {
    match event {
        Event::LeaseEnded(p) => format!(
            "event> LeaseEnded lease={} driver={} ok={}",
            p.lease, p.driver, p.ok
        ),
        other => format!("event> {other:?}"),
    }
}

/// The trace the script produces on the daemon whose session is `peer` and whose events
/// arrive on `events`.
async fn script(mut peer: Peer, events: &mut mpsc::UnboundedReceiver<Event>) -> String {
    let mut out = String::new();
    let hello = peer.hello().await;
    writeln!(
        out,
        "{}",
        daemon_line(&daemon_message::Message::Hello(hello.clone()), &[])
    )
    .expect("string");
    peer.welcome();
    writeln!(
        out,
        "server> Welcome version=1 interval={:?}",
        support::INTERVAL
    )
    .expect("string");
    // NodeStatus, then the heartbeats: every message up to the third heartbeat.
    let mut beats = 0;
    peer.expect(PROMPT, |m| {
        writeln!(out, "{}", daemon_line(m, &hello.report_hash)).expect("string");
        if matches!(m, daemon_message::Message::Heartbeat(_)) {
            beats += 1;
        }
        (beats == 3).then_some(())
    })
    .await
    .expect("three heartbeats");
    peer.start(1, 1, "action");
    writeln!(out, "server> Start lease=1.1 kind=action").expect("string");
    // Heartbeats while the lease runs differ in how many there are; the Result does not.
    let (_, result) = peer
        .expect(PROMPT, |m| match m {
            daemon_message::Message::Result(_) => Some(daemon_line(m, &hello.report_hash)),
            _ => None,
        })
        .await
        .expect("a Result");
    writeln!(out, "{result}").expect("string");
    peer.ack_result(Some((1, 1)), true);
    writeln!(out, "server> ResultAck lease=1.1 accepted=true").expect("string");
    loop {
        let event = timeout(PROMPT, events.recv())
            .await
            .expect("the acknowledgement is taken")
            .expect("the daemon runs");
        writeln!(out, "{}", event_line(&event)).expect("string");
        if matches!(event, Event::ResultAcknowledged { .. }) {
            return out;
        }
    }
}

/// Catches a session whose messages or events for the scripted run differ from the
/// recorded ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scripted_session_leaves_the_recorded_trace() {
    let mut h = Harness::start("session-trace", Duration::ZERO, Duration::from_secs(60)).await;
    let peer = h.session().await;
    assert_eq!(script(peer, &mut h.events).await, GOLDEN);
}

/// A daemon with [`NoExtras`] and the fake server it connects to.
struct Bare {
    daemon: Daemon<NoExtras>,
    sessions: mpsc::UnboundedReceiver<Peer>,
    server: JoinHandle<()>,
    events: mpsc::UnboundedReceiver<Event>,
}

impl Bare {
    /// A fake runtime whose leases take `run_for`.
    async fn new(name: &str, run_for: Duration) -> Self {
        let pki = support::pki(name);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        let (sessions, server) = support::serve_worker(pki.server_tls(), listener);
        let runtime = Arc::new(FakeRuntime::new(run_for));
        let report = NodeReport::detect(&[runtime.driver()]).expect("detect this node");
        let mut config = DaemonConfig::new(
            format!("https://127.0.0.1:{port}"),
            pki.client,
            "node-1".to_owned(),
        );
        config.fence_after = Duration::from_secs(60);
        let (events_tx, events) = mpsc::unbounded_channel();
        let daemon = Daemon::with_extras(config, Runtimes::new(runtime), report, NoExtras)
            .expect("daemon config")
            .with_events(events_tx);
        Self {
            daemon,
            sessions,
            server,
            events,
        }
    }
}

/// Catches a session loop that sends or emits, for a node with no extras, something
/// the golden does not hold. The trace shows the same text as a kbf node's because the
/// fields it reads (phase, counts, lists) do not depend on the extras; the `NodeStatus`
/// of a node with no extras carries no software, which the trace counts and does not read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_session_without_extras_leaves_the_same_trace() {
    let mut bare = Bare::new("session-trace-bare", Duration::ZERO).await;
    let daemon = tokio::spawn(bare.daemon.run(std::future::pending()));
    let peer = timeout(PROMPT, bare.sessions.recv())
        .await
        .expect("the daemon connects")
        .expect("server running");
    let trace = script(peer, &mut bare.events).await;
    daemon.abort();
    bare.server.abort();
    assert_eq!(trace, GOLDEN);
}

/// Catches a daemon with no extras that takes the drain for something it has no phase for:
/// its Hello must say it is draining, as a kbf node's does, and the daemon must stop once
/// nothing runs and its Result is acknowledged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_without_extras_drains() {
    let mut bare = Bare::new("session-drain-bare", Duration::from_secs(600)).await;
    let (signals, signals_rx) = mpsc::unbounded_channel();
    let daemon = tokio::spawn(
        bare.daemon
            .run_draining(signals_rx, Some(Duration::from_secs(30))),
    );
    let mut peer = timeout(PROMPT, bare.sessions.recv())
        .await
        .expect("the daemon connects")
        .expect("server running");
    assert!(
        peer.hello().await.phase.is_none(),
        "serving before the signal"
    );
    peer.welcome();
    peer.start(1, 1, "action");
    // The lease is running, listed in a heartbeat, so the drain has something to wait for.
    peer.expect(PROMPT, |m| match m {
        daemon_message::Message::Heartbeat(hb) => (!hb.running.is_empty()).then_some(()),
        _ => None,
    })
    .await
    .expect("a heartbeat that lists the lease");
    signals.send(()).expect("the daemon listens");
    let drained = peer
        .expect(PROMPT, |m| match m {
            daemon_message::Message::Hello(h) => h.phase.clone(),
            _ => None,
        })
        .await
        .expect("a Hello that names the phase")
        .1;
    assert_eq!(drained.phase, kbf_proto::worker::Phase::Draining as i32);
    // The drain waits for the run; a cancel ends it, and its Result is acknowledged.
    peer.cancel(Some((1, 1)));
    peer.result(PROMPT).await.expect("the lease reports");
    peer.ack_result(Some((1, 1)), true);
    timeout(PROMPT, daemon)
        .await
        .expect("the daemon stops once drained")
        .expect("the daemon task");
    bare.server.abort();
}
