//! The events the durable log exists for, at INFO and WARN, one line each: a run that ended
//! for the farm's reason and runs again, and the operation failing once its reruns are spent.
//! The log is read through a global subscriber, in a test binary of its own, which no
//! other test's events can reach or race.

mod support;

use std::io::Write;
use std::sync::{Mutex, Once, PoisonError};

use kbf_server::write_tier::report_round;
use support::{Cell, Job, done};

/// How many times the executor runs an operation again after its first run (outcome::RESUBMITS).
const RESUBMITS: u32 = 2;
/// Every line logged in this test binary. Each test names its own nodes and operations
/// and reads only the lines that name them.
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct SharedLog;

impl Write for SharedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut log = LOG.lock().unwrap_or_else(PoisonError::into_inner);
        log.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs the subscriber that writes to [`LOG`], at INFO (the server's level), once.
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
    // subscriber was being installed may have cached "never" (tests/thread_log_capture.rs),
    // so re-evaluate every callsite against it now that it is the global default.
    tracing_core::callsite::rebuild_interest_cache();
}

/// The logged lines that contain every one of `parts`.
fn lines_with(parts: &[&str]) -> Vec<String> {
    SharedLog.flush().expect("a flush");
    let log = LOG.lock().unwrap_or_else(PoisonError::into_inner);
    String::from_utf8_lossy(&log)
        .lines()
        .filter(|line| parts.iter().all(|p| line.contains(p)))
        .map(str::to_owned)
        .collect()
}

/// The REAPI name of the operation `ops` answers, as the log names it.
async fn name_of(
    ops: &mut tonic::codec::Streaming<kbf_proto::google::longrunning::Operation>,
) -> String {
    ops.message()
        .await
        .expect("a stream")
        .expect("an update")
        .name
}

/// How the log names the operation of the REAPI name `name` (`operations/<id>`): `operation=op-<id>`.
fn logged_as(name: &str) -> String {
    let id = name.strip_prefix("operations/").expect("an operation name");
    format!("operation=op-{id}")
}

/// Catches: a run that ended for the farm's reason and was run again with no WARN naming its
/// operation and run (an operator reading the log could not tell a rerun from a slow job), and
/// a rerun logged under another operation's name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_job_that_runs_again_is_logged_once_with_its_operation() {
    capture();
    let cell = Cell::start().await;
    let job = Job::new("logged rerun", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let name = name_of(&mut ops).await;
    cell.direct.lose(&job.action, 0).await;
    let second = cell.direct.job(&job.action, 1).await;
    cell.direct.run_job(&second);
    assert!(cell.direct.fake.finish(&second, 0));
    done(&mut ops).await;

    let operation = logged_as(&name);
    let again = lines_with(&[" WARN ", "the operation runs again", &operation]);
    assert_eq!(again.len(), 1, "{:?}", lines_with(&[&operation]));
    let failed = lines_with(&["the operation finished failed", &operation]);
    assert!(failed.is_empty(), "{failed:?}");
}

/// Catches: an operation that fails for good once its reruns are spent without a WARN naming
/// it, and one that logs a failure for each of the reruns before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operation_whose_reruns_are_spent_is_logged_failed_once() {
    capture();
    let cell = Cell::start().await;
    let job = Job::new("logged failure", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let name = name_of(&mut ops).await;
    for attempt in 0..=RESUBMITS {
        cell.direct.lose(&job.action, attempt).await;
    }
    done(&mut ops).await;

    let operation = logged_as(&name);
    let again = lines_with(&[" WARN ", "the operation runs again", &operation]);
    assert_eq!(
        again.len(),
        RESUBMITS as usize,
        "{:?}",
        lines_with(&[&operation])
    );
    let failed = lines_with(&[" WARN ", "the operation finished failed", &operation]);
    assert_eq!(failed.len(), 1, "{:?}", lines_with(&[&operation]));
}

/// Catches: a flusher round that found a corrupt or conflicting object and said
/// nothing (the round's report was dropped, so a lost record was invisible), and one
/// that logs every ordinary round (a line a second, forever).
#[test]
fn a_flush_round_with_corrupt_or_conflicting_objects_warns_and_a_quiet_one_does_not() {
    capture();
    report_round(&kbf_front::FlushRound::default());
    assert_eq!(lines_with(&["flush found objects"]).len(), 0);

    let round = kbf_front::FlushRound {
        conflicts: vec![object(1)],
        corrupt: vec![object(2), object(3)],
        ..kbf_front::FlushRound::default()
    };
    report_round(&round);
    let warned = lines_with(&[
        " WARN ",
        "flush found objects",
        "conflicts=1",
        "corrupt=2",
        "dropped_blobs=0",
    ]);
    assert_eq!(warned.len(), 1, "{:?}", lines_with(&["flush"]));
}

fn object(seq: u64) -> kbf_meta::ObjectId {
    kbf_meta::ObjectId::new(kbf_meta::Epoch::new(1), seq)
}
