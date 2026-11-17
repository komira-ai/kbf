//! A restart as the `kbf-server` binary has it today ([`Cell::cold_restart`]): the old
//! process stopped, a new one over the same bucket with nothing of the old one's
//! memory (issue #156, items 4 and 6).
//!
//! These tests pin what is lost, not what should be. Under the
//! in-memory metadata log (tests only; the binary always runs the Raft log) the action
//! cache and the CAS index are in memory, so a restart forgets them; under the Raft log
//! ([`Cell::start_raft`]) they are kept, along
//! with the log id and the object key prefix. The jobs of the job service go on across a
//! restart: the new executor is over the same job service. A change that makes any of it
//! durable turns the matching assertion red and must flip it in the same change.

mod support;

use std::sync::Arc;
use std::time::Duration;

use support::{Cell, Job, PROMPT, Ran, done, response};
use tonic::Code;

/// Catches a cold restart that is not one: a helper that shares the old process's
/// cache (as [`Cell::restart`] does) answers the action-cache entry and the blobs
/// written before, and one that leaves the old process running keeps its daemon's
/// stream open and its cache alive. Also catches a restart that writes under the
/// earlier start's key prefix: the first upload after it replaces an object the old
/// process wrote (the store has no conditional writes, as `conditional-put` off).
///
/// Today, after a restart, the action cache misses and the CAS reports every blob
/// missing, while their bytes are still in the bucket: the index is in memory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_restart_forgets_the_action_cache_and_the_cas_index() {
    let before = Cell::start().await;
    let job = Job::new("before the restart", &[]);
    before.upload(&job.blobs()).await;
    before
        .direct
        .will_run(&job.action, Ran::writes("out", "ran before the restart"));
    let mut ops = before.execute(&job.action).await;
    before.direct.complete(&job.action).await;
    let result = response(&done(&mut ops).await).result.expect("a result");
    assert_eq!(before.cached(&job.action).await, Ok(result));
    assert!(before.holds(&job.input).await);
    let written = before.store.objects().await;
    assert!(!written.is_empty(), "the old process wrote no object");
    let old_cache = Arc::downgrade(&before.cache);

    let after = before.cold_restart().await;

    drop(ops);
    let deadline = tokio::time::Instant::now() + PROMPT;
    while old_cache.strong_count() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old process's cache is still held"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        after.cached(&job.action).await,
        Err(Code::NotFound),
        "an action-cache entry survived the restart"
    );
    for blob in job.blobs() {
        assert!(
            !after.holds(blob).await,
            "{:?} survived the restart",
            blob.proto
        );
    }

    // The new process works, and the old objects are still in the bucket, unchanged. Its
    // executor submits a job of its own, though the old process's job is in the job service.
    let fresh = Job::new("after the restart", &[]);
    after.upload(&fresh.blobs()).await;
    after
        .direct
        .will_run(&fresh.action, Ran::writes("out", "ran after the restart"));
    let mut ops = after.execute(&fresh.action).await;
    after.direct.complete(&fresh.action).await;
    let result = response(&done(&mut ops).await).result.expect("a result");
    assert_eq!(result.output_files[0].path, "out");
    assert_eq!(after.direct.submitted_jobs(), 2, "one job for each process");
    let now = after.store.objects().await;
    for (key, bytes) in &written {
        assert_eq!(now.get(key), Some(bytes), "object {key} changed");
    }
    assert!(now.len() > written.len(), "the new process wrote no object");
}

/// The same restart under the Raft metadata log: the new process reopens the Raft log the
/// old one left.
///
/// Catches: a restart that draws a new log id (it commits a second `Bootstrap`, or
/// refuses its own log); one that writes under a start-time prefix instead of the
/// log's `kbf/<log id>/`; farm time that starts again from zero (the process clock)
/// instead of the committed farm time; and a log that does not keep the action cache
/// and the CAS index (the cache misses, FindMissingBlobs reports blobs missing).
/// The new process writes in a new epoch, so it replaces none of the old objects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_restart_under_the_raft_log_keeps_the_action_cache_and_the_cas_index() {
    let before = Cell::start_raft().await;
    let (log_id, prefix) = (before.raft_log().log_id(), before.raft_log().store_prefix());
    assert_eq!(prefix.as_str(), format!("kbf/{log_id}/"));
    assert_eq!(before.raft_log().bootstraps(), 1);
    let job = Job::new("before the raft restart", &[]);
    before.upload(&job.blobs()).await;
    before.direct.will_run(
        &job.action,
        Ran::writes("out", "ran before the raft restart"),
    );
    let mut ops = before.execute(&job.action).await;
    before.direct.complete(&job.action).await;
    let result = response(&done(&mut ops).await).result.expect("a result");
    assert_eq!(before.cached(&job.action).await, Ok(result.clone()));
    let farm_time = before.raft_log().tick().await.expect("a tick");
    let written = before.store.objects().await;
    assert!(!written.is_empty(), "the old process wrote no object");
    let old_cache = Arc::downgrade(&before.cache);

    let after = before.cold_restart().await;

    drop(ops);
    let deadline = tokio::time::Instant::now() + PROMPT;
    while old_cache.strong_count() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old process's cache is still held"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let log = after.raft_log();
    assert_eq!(log.log_id(), log_id, "the restart changed the log id");
    assert_eq!(log.bootstraps(), 1, "a second Bootstrap was committed");
    assert_eq!(log.store_prefix(), prefix, "the restart changed the prefix");
    assert!(
        log.now() >= farm_time,
        "farm time went back across the restart"
    );
    assert_eq!(
        after.cached(&job.action).await,
        Ok(result),
        "the action-cache entry was lost in the restart"
    );
    for blob in job.blobs() {
        assert!(after.holds(blob).await, "{:?} was lost", blob.proto);
    }

    let job = Job::new("after the raft restart", &[]);
    after.upload(&job.blobs()).await;
    after.direct.will_run(
        &job.action,
        Ran::writes("out", "ran after the raft restart"),
    );
    let mut ops = after.execute(&job.action).await;
    after.direct.complete(&job.action).await;
    let result = response(&done(&mut ops).await).result.expect("a result");
    assert_eq!(result.output_files[0].path, "out");
    let now = after.store.objects().await;
    for (key, bytes) in &written {
        assert_eq!(now.get(key), Some(bytes), "object {key} changed");
    }
    assert!(now.len() > written.len(), "the new process wrote no object");
    for key in now.keys() {
        assert!(
            key.starts_with(prefix.as_str()),
            "{key} is outside {prefix:?}"
        );
    }
}
