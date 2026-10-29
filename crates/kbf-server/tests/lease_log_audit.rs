//! The platform an action asked for and the exit code it ended with, on the `lease
//! granted` and `lease finished` lines, so an audit can find the Linux actions leased
//! to a Mac and the actions that exited 126. The message text of the lines is what
//! existing greps key on; these tests read the fields only.

mod support;

use std::io::Write;
use std::sync::{Mutex, Once, PoisonError};

use support::{Cell, FakeDaemon, Job, done, failed, hello_on, output, ran};
use tonic::Code;

/// Every line logged in this test binary. Each test names its own nodes and reads only
/// the lines that name them.
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct SharedLog;

impl Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOG.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs the subscriber that writes to [`LOG`], once.
fn capture() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| SharedLog)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
    });
    // Tests run in parallel: a callsite another test's thread first reached while this
    // subscriber was being installed may have cached "never" (tests/thread_log_capture.rs).
    tracing_core::callsite::rebuild_interest_cache();
}

/// The logged lines that contain every one of `parts`.
fn lines_with(parts: &[&str]) -> Vec<String> {
    let log = LOG.lock().unwrap_or_else(PoisonError::into_inner);
    String::from_utf8_lossy(&log)
        .lines()
        .filter(|line| parts.iter().all(|p| line.contains(p)))
        .map(str::to_owned)
        .collect()
}

/// The one line with `message` for `node`, read as space-separated words.
fn the_line(message: &str, node: &str) -> Vec<String> {
    let node = format!("node={node}");
    let lines = lines_with(&[message, &node]);
    assert_eq!(lines.len(), 1, "{message} for {node}: {lines:?}");
    lines[0].split(' ').map(str::to_owned).collect()
}

fn has(words: &[String], field: &str) -> bool {
    words.iter().any(|w| w == field)
}

const LINUX: &[(&str, &str)] = &[("arch", "x86_64"), ("os", "linux")];
const MAC: &[(&str, &str)] = &[("arch", "arm64"), ("os", "macos")];

/// Runs a job asking `platform` on a node `name` with `caps`; returns after its grant.
async fn granted(cell: &Cell, name: &str, caps: &[(&str, &str)], platform: &[(&str, &str)]) {
    let mut daemon = FakeDaemon::connect(cell.worker_addr, hello_on(name, 4, 8, caps))
        .await
        .expect("registered");
    let job = Job::new(name, platform);
    cell.upload(&job.blobs()).await;
    let _ops = cell.execute(&job.action).await;
    let _start = daemon.start().await;
}

/// Catches: a `lease granted` line that does not say which OS family and ISA the action
/// asked for (an audit could not tell a Linux action leased to a Mac), one that logs the
/// request's raw property text, or one that writes a value for an action that named
/// none (`-`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_granted_lease_names_the_os_family_and_isa_the_action_asked_for() {
    capture();
    // One cell per action: with two idle nodes the scheduler may pick either.
    granted(
        &Cell::start().await,
        "audit-linux",
        LINUX,
        &[("OSFamily", "Linux"), ("ISA", "x86-64")],
    )
    .await;
    granted(
        &Cell::start().await,
        "audit-mac",
        MAC,
        &[("OSFamily", "Darwin")],
    )
    .await;
    granted(&Cell::start().await, "audit-none", LINUX, &[]).await;

    let linux = the_line("lease granted", "audit-linux");
    assert!(has(&linux, "os_family=linux"), "{linux:?}");
    assert!(has(&linux, "isa=x86_64"), "{linux:?}");
    let mac = the_line("lease granted", "audit-mac");
    assert!(has(&mac, "os_family=macos"), "{mac:?}");
    assert!(has(&mac, "isa=-"), "{mac:?}");
    let none = the_line("lease granted", "audit-none");
    assert!(has(&none, "os_family=-"), "{none:?}");
    assert!(has(&none, "isa=-"), "{none:?}");
}

/// Catches: a `lease finished ok` line without the action's exit code (126 and 0 could
/// not be told apart), and a `lease finished failed` line, for a lease that failed
/// before the action ran, that makes up a code instead of `-`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "known-different-on-js: the js core's lease finished line has no exit_code= field"
)]
async fn a_finished_lease_names_the_exit_code_or_a_dash() {
    capture();
    let cell = Cell::start().await;
    let mut exited = cell.daemon("audit-exit", 4, 8).await;
    let job = Job::new("exits 126", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = exited.start().await;
    let result = output(&cell, "not executable", 126).await;
    assert!(exited.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    // Another cell: with two idle nodes the second action may go to the first.
    let cell = Cell::start().await;
    let mut broken = cell.daemon("audit-infra", 4, 8).await;
    let job = Job::new("never runs", &[]);
    cell.upload(&job.blobs()).await;
    let _ops = cell.execute(&job.action).await;
    let start = broken.start().await;
    assert!(
        broken
            .report(failed(start.lease_id, Code::Internal))
            .await
            .accepted
    );

    let ok = the_line("lease finished ok", "audit-exit");
    assert!(has(&ok, "exit_code=126"), "{ok:?}");
    let infra = the_line("lease finished failed", "audit-infra");
    assert!(has(&infra, "exit_code=-"), "{infra:?}");
}
