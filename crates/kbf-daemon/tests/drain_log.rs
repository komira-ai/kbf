//! What the daemon's drain tells the operator. Alone in its test binary: a tracing
//! callsite that another test's thread reached first, with no subscriber installed, is
//! cached as uninteresting, and a subscriber installed after would miss it.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::FakeRuntime;
use support::{Harness, PROMPT};

/// Catches a drain the operator cannot see: the first signal logs that the node is
/// draining and with which deadline, and the return logs why it stopped. (A
/// current-thread runtime, so the test's subscriber sees the daemon's tasks.)
#[tokio::test]
async fn the_drain_logs_its_start_with_the_deadline_and_why_it_stopped() {
    let log = Log::default();
    let _installed = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    let runtime = Arc::new(FakeRuntime::new(Duration::from_millis(400)));
    let (mut h, signal) = Harness::with_drain("drain-log", runtime, Duration::from_secs(60)).await;
    let _peer = h.welcomed().await;
    assert_eq!(log.count("draining: no new leases"), 0, "before any signal");

    signal.send(()).expect("daemon runs");
    assert!(
        h.exited(PROMPT).await,
        "an idle node returns at the first signal"
    );
    assert_eq!(log.count("draining: no new leases"), 1);
    assert_eq!(log.count("deadline=60s"), 1);
    assert_eq!(log.count("drained: stopping"), 1);
}

/// What a subscriber wrote, for the test above.
#[derive(Clone, Default)]
struct Log(Arc<std::sync::Mutex<Vec<u8>>>);

impl Log {
    fn count(&self, needle: &str) -> usize {
        let text = String::from_utf8(self.0.lock().expect("lock").clone()).expect("UTF-8");
        text.matches(needle).count()
    }
}

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
