//! `kbf frontend --scheduler-core`: taken by the frontend (`legacy` and `js`), and logged at
//! start as the core the farm runs. Alone in its test
//! binary: a log event is seen by a subscriber set for one thread only if no other thread
//! has already made the callsite uninteresting, which other frontend starts in the same
//! process would do.

use std::future::pending;
use std::sync::{Arc, Mutex, PoisonError};

use kbf_server::frontend::{self, start_frontend};

/// A `MakeWriter` into a shared buffer, standing for the log.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Log);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0)
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.clone())
    }
}

impl Log {
    fn text(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
        .expect("UTF-8")
    }
}

/// The flags of `kbf frontend` for a start on loopback ports, and these more.
fn flags(more: &[&str]) -> frontend::FrontendFlags {
    let mut argv = vec![
        "frontend",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
        "--storage=http://127.0.0.1:1",
    ];
    argv.extend_from_slice(more);
    frontend::parse(
        &frontend::command()
            .try_get_matches_from(argv)
            .expect("flags"),
    )
    .expect("a frontend takes --scheduler-core")
}

/// Catches a frontend that refuses `--scheduler-core` as a flag it does not take (a unit
/// file for the js core could not be written for a frontend), a frontend that cannot
/// start on `js`, and a start that does not log the core the farm runs, in the words the
/// all-in-one server uses (`scheduler core: legacy` or `scheduler core: js`, once per start).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_takes_the_scheduler_core_flag_and_logs_the_core_it_runs() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    let start = |f: &frontend::FrontendFlags| {
        tracing::dispatcher::with_default(&dispatch, || start_frontend(f, || Ok(pending::<()>())))
    };

    let _default = start(&flags(&[])).expect("the default core starts");
    assert_eq!(
        log.text().matches("scheduler core: legacy").count(),
        1,
        "{}",
        log.text()
    );
    let _legacy = start(&flags(&["--scheduler-core=legacy"])).expect("legacy starts");
    assert_eq!(
        log.text().matches("scheduler core: legacy").count(),
        2,
        "{}",
        log.text()
    );
    assert!(!log.text().contains("scheduler core: js"));

    let js = flags(&["--scheduler-core=js"]);
    let _js = start(&js).expect("js starts");
    assert_eq!(
        log.text().matches("scheduler core: js").count(),
        1,
        "{}",
        log.text()
    );
}
