//! The end of a failed job's standard error in the frontend's log: the line at WARN, its bound,
//! and what never reaches it.

use super::{arg_of, running_job, step};
use crate::executor::test_support::{Harness, Log};

/// What a failed job wrote to standard error, which a log reader needs to see why it failed.
const DIED: &[u8] = b"kbf-task: run: /.kbf-run: cannot make the run directory: Permission denied\n";

/// Fails the job `id` of `harness` with `stdout` and `stderr` and steps its worker, returning
/// what the frontend logged.
async fn failed_with(harness: &Harness, stdout: &[u8], stderr: &[u8]) -> String {
    let (mut worker, messages) = harness.worker();
    let id = running_job(harness, &mut worker, &messages).await;
    assert!(harness.fake.write_logs(&id, stdout, stderr));
    assert!(harness.fake.finish(&id, 11));
    let log = Log::default();
    let guard = log.install();
    step(harness, &mut worker).await;
    drop(guard);
    log.text()
}

/// Catches a failed task's reason staying out of the frontend's log, which only held lengths:
/// the end of the standard error is one line at WARN, with the lease and the job, its line
/// break and quotes escaped so the text cannot start a log line, and the standard output is
/// not in the log.
#[tokio::test(start_paused = true)]
async fn the_end_of_a_failed_jobs_stderr_is_logged_at_warn() {
    let harness = Harness::new().await;
    let text = failed_with(&harness, b"compiling\n", DIED).await;
    let line = text
        .lines()
        .find(|line| line.contains("the end of the standard error of a job"))
        .unwrap_or_else(|| panic!("no warning in {text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(
        line.contains(
            r#"stderr="kbf-task: run: /.kbf-run: cannot make the run directory: Permission denied""#
        ),
        "{line}"
    );
    assert!(line.contains("cut=false"), "{line}");
    assert!(!text.contains("compiling"), "{text}");
}

/// Catches the excerpt ignoring its bound: with `failed_stderr_log_bytes` at 6 the line holds
/// the last 6 bytes of the stream and says it cut; at 0 there is no such line, though the
/// lengths line stays.
#[tokio::test(start_paused = true)]
async fn the_excerpt_is_bounded_by_its_config_key() {
    let mut harness = Harness::new().await;
    harness.cfg.failed_stderr_log_bytes = 6;
    let text = failed_with(&harness, b"", b"first line\nthe end\n").await;
    assert!(text.contains(r#"stderr="e end""#), "{text}");
    assert!(text.contains("cut=true"), "{text}");
    assert!(!text.contains("first line"), "{text}");

    let mut off = Harness::new().await;
    off.cfg.failed_stderr_log_bytes = 0;
    let text = failed_with(&off, b"", DIED).await;
    assert!(text.contains("stderr_bytes=75"), "{text}");
    assert!(!text.contains("Permission denied"), "{text}");
}

/// Catches a grant of the job reaching the log through the excerpt: the pack url and the read
/// token the job was handed, written to standard error, are each replaced by the marker.
#[tokio::test(start_paused = true)]
async fn the_excerpt_holds_none_of_the_jobs_grants() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let id = running_job(&harness, &mut worker, &messages).await;
    let url = arg_of(&harness, &id, "--pack-url");
    let token = arg_of(&harness, &id, "--grant-token");
    let stderr = format!("put {url} refused with {token}\n");
    assert!(harness.fake.write_logs(&id, b"", stderr.as_bytes()));
    assert!(harness.fake.finish(&id, 13));
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    let text = log.text();
    assert!(
        text.contains(r#"stderr="put [redacted] refused with [redacted]""#),
        "{text}"
    );
    assert!(!text.contains(&url) && !text.contains(&token), "{text}");
}
