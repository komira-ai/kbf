//! What a frontend start logs when it cannot route by the storage map. Alone in its test
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
    .expect("parses")
}

/// Catches a frontend with a state store and no TLS flags that goes on storing to its one
/// `--storage` node in silence, when the members its daemons place blobs on are others
/// (the failure is an OK result whose outputs are not stored): the start logs a warning
/// saying so; without a state store there are no members and it does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_state_store_without_tls_flags_warns_that_members_are_not_routed_to() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    let start = |f: &frontend::FrontendFlags| {
        tracing::dispatcher::with_default(&dispatch, || {
            start_frontend(f, || Ok(pending::<()>())).expect("starts")
        })
    };

    let _plain = start(&flags(&[]));
    assert!(
        !log.text().contains("members cannot be dialled"),
        "{}",
        log.text()
    );
    let state =
        kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-warns-state");
    let store = format!(
        "--state-store=sqlite://{}",
        state.join("state.db").display()
    );
    let _stored = start(&flags(&[&store]));
    assert!(
        log.text().contains("members cannot be dialled"),
        "{}",
        log.text()
    );
}

/// Catches: the retired `--release-poll-*` flags and `--releases-dir` (a unit or `kbf.toml`
/// written before the poller and the release store were removed carries them) being refused
/// at start, or accepted without a word: a frontend given all four starts and logs one
/// warning for each naming the flag, and with none of them logs none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_retired_release_flags_start_and_warn_once_each() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    let start = |f: &frontend::FrontendFlags| {
        tracing::dispatcher::with_default(&dispatch, || {
            start_frontend(f, || Ok(pending::<()>())).expect("starts")
        })
    };

    let _plain = start(&flags(&[]));
    assert!(!log.text().contains("no longer read"), "{}", log.text());
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-warns-store");
    let _old = start(&flags(&[
        "--release-poll-every-secs=300",
        "--release-poll-repo=komira-ai/kbf",
        "--release-poll-refs=main",
        &format!("--releases-dir={}", dir.join("releases").display()),
    ]));
    let text = log.text();
    for (flag, gone) in [
        ("--release-poll-every-secs", "the release poller"),
        ("--release-poll-repo", "the release poller"),
        ("--release-poll-refs", "the release poller"),
        ("--releases-dir", "the release store"),
    ] {
        let line = format!("{flag} is no longer read: {gone} was removed");
        assert_eq!(text.matches(&line).count(), 1, "{text}");
    }
}

/// Asserts that the flags of the removed handover and admin port (a unit or `kbf.toml`
/// written before the removal carries them) start a frontend and log one warning for each,
/// naming the flag, and that a frontend given none logs none of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_retired_handover_flags_start_and_warn_once_each() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    let start = |f: &frontend::FrontendFlags| {
        tracing::dispatcher::with_default(&dispatch, || {
            start_frontend(f, || Ok(pending::<()>())).expect("starts")
        })
    };

    let _plain = start(&flags(&[]));
    assert!(
        !log.text().contains("the handover was removed"),
        "{}",
        log.text()
    );
    let given = [
        "--handover-socket=/run/kbf/control.sock",
        "--instance=a",
        "--handover-quiesce-secs=10",
        "--handover-follow-interval-ms=200",
        "--handover-watchdog-secs=15",
        "--handover-peer-name=kbf.example.internal",
        "--envoy-check-interval-ms=1000",
        "--envoy-check-timeout-ms=1000",
        "--envoy-unhealthy-threshold=2",
        "--admin-listen=127.0.0.1:18983",
        "--await-takeover",
    ];
    let _old = start(&flags(&given));
    let text = log.text();
    for flag in given {
        let flag = flag.split('=').next().expect("a flag");
        let line = format!("{flag} is no longer read: the handover was removed");
        assert_eq!(text.matches(&line).count(), 1, "{text}");
    }
}

/// Catches: a frontend started with the retired `--releases-dir` on a directory that holds
/// an old release store changing it (opening it for writes, making files in it, or deleting
/// it): the store is operator data. The directory, a nested file and its bytes are as they
/// were after the start, and the old upload route is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_given_an_old_release_store_dir_leaves_it_alone() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "frontend-old-store");
    let store = dir.join("releases");
    let nested = store.join("0123456789abcdef0123456789abcdef01234567/linux-x86_64");
    std::fs::create_dir_all(&nested).expect("tree");
    std::fs::write(nested.join("kbf"), b"an old tarball").expect("file");
    let before = tree(&store);
    let started = start_frontend(
        &flags(&[&format!("--releases-dir={}", store.display())]),
        || Ok(pending::<()>()),
    )
    .expect("starts");
    let api = started.api.expect("an API");
    tokio::spawn(started.serve());
    let mut stream = tokio::net::TcpStream::connect(api).await.expect("connect");
    stream
        .write_all(
            b"PUT /v1/releases/0123456789abcdef0123456789abcdef01234567/linux-x86_64/kbf HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Length: 1\r\n\r\nx",
        )
        .await
        .expect("write");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert_eq!(tree(&store), before);
    assert_eq!(
        std::fs::read(nested.join("kbf")).expect("kept"),
        b"an old tarball"
    );
}

/// Every path under `root` with its length, sorted.
fn tree(root: &std::path::Path) -> Vec<(std::path::PathBuf, u64)> {
    let mut found = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(at) = todo.pop() {
        for entry in std::fs::read_dir(&at).expect("read_dir") {
            let path = entry.expect("entry").path();
            let meta = std::fs::metadata(&path).expect("meta");
            if meta.is_dir() {
                todo.push(path.clone());
            }
            found.push((path, meta.len()));
        }
    }
    found.sort();
    found
}
