//! What a start over a `sqlite://` state store logs. Alone in its test binary: a log event
//! is seen by a subscriber set for one thread only if no other thread has already made the
//! callsite uninteresting, which other opens of a store in the same process would do.

use std::future::pending;
use std::sync::{Arc, Mutex, PoisonError};

use kbf_server::StateStoreUri;
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

/// The first words of the warning a start over SQLite logs.
const WARNING: &str = "the SQLite state store is removed in the next release";

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
    .expect("parses")
}

/// Asserts that a frontend started over `--state-store=sqlite://...` starts and logs, once,
/// a warning that SQLite is removed in the next release and where to read how to move; and
/// that an open of a Postgres store, which here fails for want of a server, logs no such
/// warning.
#[test]
fn a_sqlite_state_store_starts_and_warns_once_and_a_postgres_one_does_not() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "state-store-warns");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let sqlite = format!("--state-store=sqlite://{}", dir.join("state.db").display());
    let _front = runtime.block_on(async {
        tracing::dispatcher::with_default(&dispatch, || {
            start_frontend(&flags(&[&sqlite]), || Ok(pending::<()>())).expect("starts")
        })
    });
    let text = log.text();
    assert_eq!(text.matches(WARNING).count(), 1, "{text}");
    assert!(text.contains("see the upgrade notes"), "{text}");
    let line = text.lines().find(|l| l.contains(WARNING)).expect("a line");
    assert!(line.contains("WARN"), "{line}");

    let socket = dir.join("no-socket");
    let postgres: StateStoreUri = format!("postgres:///kbf?host={}", socket.display())
        .parse()
        .expect("a socket URL");
    tracing::dispatcher::with_default(&dispatch, || {
        postgres.open().err().expect("nothing listens");
    });
    assert_eq!(log.text().matches(WARNING).count(), 1, "{}", log.text());
}
