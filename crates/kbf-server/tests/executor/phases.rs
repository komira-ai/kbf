//! The phases of a job, as the executor stamps them and `GET /metrics` shows them
//! (`kbf_executor_phase_seconds`). Time is tokio's paused clock, moved by hand: the tests assert
//! which phases were observed, never how long anything took.

use std::time::Duration;

use crate::rig::Rig;

const HOST: &str = "host-1";

/// How many observations the metric holds for `phase`.
fn observed(counted: &str, phase: &str) -> String {
    let series = format!("kbf_executor_phase_seconds_count{{phase=\"{phase}\"}} ");
    let line = counted
        .lines()
        .find(|line| line.starts_with(&series))
        .unwrap_or_else(|| panic!("no {phase} phase in {counted}"));
    line[series.len()..].to_owned()
}

/// Catches a phase not stamped on the path a job takes: a job that is read back as running on
/// one pass and as ended on a later one is counted once in each of the six phases (`prepare`,
/// `start`, `queue`, `task`, `run` and `ingest`).
#[tokio::test(start_paused = true)]
async fn a_job_seen_running_records_every_phase_once() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(7);
    rig.pump().await;
    let jobs = rig.jobs();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let id = jobs[0].id();
    rig.host_runs(id, HOST);
    rig.advance(Duration::from_secs(5)).await;
    rig.pump().await;
    assert!(rig.fake.finish(id, 0), "{id} finishes");
    rig.until_done(&caller, 4).await;

    let counted = rig.counted();
    for phase in ["prepare", "start", "queue", "task", "run", "ingest"] {
        assert_eq!(observed(&counted, phase), "1", "{phase}");
    }
}

/// Catches a phase invented for a job that ended between two reads of it: first read back as
/// ended, it has a `run` and no `queue` or `task`, and the phases before and after it are
/// still counted.
#[tokio::test(start_paused = true)]
async fn a_job_first_seen_ended_records_no_queue_or_task_phase() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(7);
    rig.pump().await;
    let jobs = rig.jobs();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    rig.host_finishes(jobs[0].id(), HOST, 0);
    rig.until_done(&caller, 4).await;

    let counted = rig.counted();
    for (phase, count) in [
        ("prepare", "1"),
        ("start", "1"),
        ("queue", "0"),
        ("task", "0"),
        ("run", "1"),
        ("ingest", "1"),
    ] {
        assert_eq!(observed(&counted, phase), count, "{phase}");
    }
}
