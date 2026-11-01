//! The retired `--release-poll-*` flags on the non-frontend start (`kbf_server::start`),
//! in a test binary of its own: the log is read through a global subscriber, which no
//! other test's events can reach or race. The frontend start is pinned in
//! `tests/frontend_warns.rs`.

use std::io::Write;
use std::sync::{Arc, Mutex};

use clap::Parser;
use kbf_server::Args;

/// Where the global subscriber writes: every line, without colour.
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

/// Catches: the scheduler/all-in-one start (`start` -> `assemble`) accepting the retired
/// `--release-poll-*` flags in silence, because only the frontend start warned. A start
/// given `--release-poll-repo` serves and logs one WARN naming it; one given none logs
/// no such line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_start_warns_once_for_a_retired_release_poll_flag() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "retired-poll-warns");
    let start = |more: &[&str], name: &str| {
        let at = |leaf: &str| dir.join(name).join(leaf).display().to_string();
        let mut argv = vec![
            "kbf-server".to_owned(),
            "--listen=127.0.0.1:0".to_owned(),
            "--worker-listen=127.0.0.1:0".to_owned(),
            format!("--cold-store=file://{}", at("cold")),
            "--cold-store-floor-bytes=1".to_owned(),
            format!("--meta-dir={}", at("meta")),
        ];
        argv.extend(more.iter().map(|s| (*s).to_owned()));
        let args = Args::try_parse_from(argv).expect("flags");
        async move {
            kbf_server::start(&args, || Ok(std::future::pending::<()>()))
                .await
                .expect("starts")
        }
    };
    let text = |log: &Log| String::from_utf8(log.0.lock().expect("log").clone()).expect("UTF-8");

    let _plain = start(&[], "plain").await;
    assert!(!text(&log).contains("no longer read"), "{}", text(&log));
    let _old = start(&["--release-poll-repo=komira-ai/kbf"], "old").await;
    let text = text(&log);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("no longer read"))
        .collect();
    let [line] = lines.as_slice() else {
        panic!("one retirement line expected: {text}");
    };
    assert!(line.contains("WARN"), "{line}");
    assert!(
        line.contains("--release-poll-repo is no longer read: the release poller was removed"),
        "{line}"
    );
}
