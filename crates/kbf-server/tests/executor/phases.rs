//! The phases of a job, as the executor stamps them and `GET /metrics` shows them
//! (`kbf_executor_phase_seconds`). The tests assert which phases were observed, never how long
//! anything took.

use std::time::Duration;

use crate::rig::{HOST, Rig};

/// How many observations the metric holds for `phase`.
fn observed(counted: &str, phase: &str) -> String {
    let series = format!("kbf_executor_phase_seconds_count{{phase=\"{phase}\"}} ");
    let line = counted
        .lines()
        .find(|line| line.starts_with(&series))
        .unwrap_or_else(|| panic!("no {phase} phase in {counted}"));
    line[series.len()..].to_owned()
}

/// Catches a phase not stamped on the path a job takes: a job that is read back as running and
/// then as ended is counted once in each of the phases of an operation that ran (`prepare`,
/// `start`, `queue`, `task`, `run`, `ingest`, `submit` and `execute`).
#[tokio::test]
async fn a_job_seen_running_records_every_phase_once() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(7);
    let job = rig.first_job(7).await;
    rig.host_runs(job.id(), HOST);
    caller.executing().await;
    assert!(rig.fake.finish(job.id(), 0), "{} finishes", job.id());
    caller.done().await;

    let counted = rig.counted();
    for phase in [
        "prepare", "start", "queue", "task", "run", "ingest", "submit", "execute",
    ] {
        assert_eq!(observed(&counted, phase), "1", "{phase}");
    }
}

/// Catches a phase invented for a job that ended between two reads of it: first read back as
/// ended, it has a `run` and no `queue` or `task`, and the phases before and after it are
/// still counted.
#[tokio::test]
async fn a_job_first_seen_ended_records_no_queue_or_task_phase() {
    // The executor reads the job at its submit and not again until the job has ended.
    let rig = Rig::start(&[HOST], |cfg| cfg.poll = Duration::from_millis(400)).await;
    let mut caller = rig.submit(7);
    let job = rig.first_job(7).await;
    rig.host_finishes(job.id(), HOST, 0);
    caller.done().await;

    let counted = rig.counted();
    for (phase, count) in [
        ("prepare", "1"),
        ("start", "1"),
        ("queue", "0"),
        ("task", "0"),
        ("run", "1"),
        ("ingest", "1"),
        ("submit", "1"),
        ("execute", "1"),
    ] {
        assert_eq!(observed(&counted, phase), count, "{phase}");
    }
}
