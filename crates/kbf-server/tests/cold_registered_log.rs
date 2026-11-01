//! What a node on `--cold-store registered` says while it waits for the frontend, in a test
//! binary of its own: the log is read through a global subscriber, which no other test's
//! events can reach or race. The frontend address is one nothing listens on, so every ask
//! fails the way a down frontend does.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/cold_node.rs"]
mod node;
#[path = "support/store_config.rs"]
mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use node::{ID, SECRET, flags, no_keys, registered, rig, spin_until, started};

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

impl Log {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log")).into_owned()
    }
}

/// Catches a wait an operator cannot read (defects: no line when the wait starts, a warning
/// with no code and no reason, no count): the node says at INFO that it is waiting and for
/// which frontend, and each failed ask says at WARN which attempt it was, the status code and
/// the reason, so a down frontend, a refused certificate and a wrong address read differently.
/// The frontend here is `localhost:1`, where nothing listens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_waits_says_for_whom_and_why_each_ask_failed() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let rig = rig(3, |s3| registered(s3, "bkt", ID, SECRET)).await;
    let mut node = flags(&rig, "waits-says", "registered", no_keys);
    node.frontend = Some("https://localhost:1".to_owned());
    let mut starting = Box::pin(started(&node));
    let warning = spin_until(|| {
        let text = log.text();
        text.lines()
            .find(|l| l.contains("WARN") && l.contains("trying again"))
            .map(str::to_owned)
    });
    let warned = tokio::select! {
        done = &mut starting => panic!("the node gave up: {:?}", done.map(drop)),
        line = warning => line,
    };
    let text = log.text();
    let info = text
        .lines()
        .find(|l| l.contains("INFO") && l.contains("waiting for the registered cold store"))
        .unwrap_or_else(|| panic!("no line when the wait starts:\n{text}"));
    assert!(info.contains("https://localhost:1"), "{info}");
    assert!(warned.contains("attempt=1"), "{warned}");
    assert!(warned.contains("code=Unavailable"), "{warned}");
    assert!(warned.contains("error="), "{warned}");
}
