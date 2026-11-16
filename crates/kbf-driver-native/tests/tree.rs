//! The whole process tree ends with the lease: after a normal exit, a timeout, a kill,
//! a dropped run and the memory watch, no process the action started is left alive,
//! including one that left the process group and was orphaned. The memory watch kills
//! an action whose processes together hold more than the lease's limit and reports it
//! out of memory; one within it runs to the end.
//!
//! Every way a run ends is in this one test binary: the async body that runs an action
//! is compiled into each test binary that calls it, and llvm-cov's summary counts the
//! best-covered copy, not the union of the copies.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_native::MemoryPolicy;
use kbf_proto::worker::ResourceUsage;
use kbf_types::LeaseId;
use support::{
    MemoryCas, PROMPT, Spec, alive, config, no_leases, pid_in, run, runtime, scratch, work,
};

/// Act steps that start a child which leaves the process group (`setsid`) and
/// sleeps, and print its pid; the leader waits until a few polls have seen the child,
/// then exits, so the child is orphaned and reparented.
const ESCAPER: &str = "spawn-child --pid - setsid ;; sleep 300 ; sleep 0.3 ; exit 0";

/// Where a test action writes its pid: in its working directory.
const PIDFILE: &str = "pid";

/// The pid file of lease `1.seq` while it runs, as the test sees it.
fn in_lease(config: &kbf_driver_native::NativeConfig, seq: u64) -> std::path::PathBuf {
    config
        .scratch
        .join(format!("lease-1-{seq}"))
        .join("root")
        .join(PIDFILE)
}

/// The pid an action printed on its stdout.
fn pid_of(cas: &MemoryCas, result: &kbf_proto::reapi::ActionResult) -> i32 {
    let out = support::stdout(cas, result);
    out.trim()
        .parse()
        .unwrap_or_else(|_| panic!("no pid in {out:?}"))
}

/// Catches: background processes an action leaves behind surviving its lease (they
/// would run on, and could change outputs while they are read). Covers a plain
/// background child in the group, and one that left the group with `setsid` and was
/// orphaned when its parent exited.
#[tokio::test]
async fn processes_left_behind_after_exit_are_killed() {
    let dir = scratch("left-behind");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::act("spawn-child --pid - sleep 300 ; exit 0");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    let pid = pid_of(&cas, &result);
    assert!(!alive(pid), "the background child {pid} outlived the lease");

    let spec = Spec::act(ESCAPER);
    let result = run(&rt, &cas, 2, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", support::stderr(&cas, &result));
    let pid = pid_of(&cas, &result);
    assert!(!alive(pid), "the setsid child {pid} outlived the lease");
    assert!(no_leases(&config));
}

/// Catches: a timeout that ends only the leader (a grandchild runs on), or that is
/// not reported as one.
#[tokio::test]
async fn a_timeout_ends_the_whole_tree() {
    let dir = scratch("timeout");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::act(&format!(
        "spawn-child spawn-child --pid {PIDFILE} sleep 300 ;; sleep 300 ; sleep 300"
    ))
    .timeout(Duration::from_millis(500));
    let pidfile = in_lease(&config, 1);
    let (outcome, pid) = tokio::join!(run(&rt, &cas, 1, &spec), pid_in(&pidfile));
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut { .. })),
        "{outcome:?}"
    );
    assert!(!alive(pid), "the grandchild {pid} outlived the timeout");
    assert!(no_leases(&config));
}

/// Catches: the daemon's kill ending only the leader, or returning before the tree
/// is gone and the lease directory removed.
#[tokio::test]
async fn a_kill_ends_the_whole_tree_before_it_returns() {
    let dir = scratch("kill");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));
    let pidfile = in_lease(&config, 1);
    let action = Spec::act(&format!(
        "spawn-child --pid {PIDFILE} sleep 300 ; sleep 300"
    ))
    .store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(work(1, action, 0)).await }
    });
    let pid = pid_in(&pidfile).await;
    rt.kill(kbf_types::LeaseId::new(1, 1)).await;
    assert!(!alive(pid), "the child {pid} outlived the kill");
    assert!(no_leases(&config), "the kill returned before the clean");
    let outcome = running.await.expect("run task");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
}

/// Catches: a run the daemon drops (its task cancelled) leaving the action's
/// processes or its lease directory behind.
#[tokio::test]
async fn a_dropped_run_ends_the_tree_and_cleans() {
    let dir = scratch("dropped");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let pidfile = in_lease(&config, 1);
    let action = Spec::act(&format!(
        "spawn-child --pid {PIDFILE} sleep 300 ; sleep 300"
    ))
    .store(&cas);
    let run = rt.run(work(1, action, 0));
    let watched = async {
        let pid = pid_in(&pidfile).await;
        // Long enough for the watch to have seen the tree.
        tokio::time::sleep(Duration::from_millis(100)).await;
        pid
    };
    let pid = tokio::select! {
        outcome = run => panic!("the run ended: {outcome:?}"),
        pid = watched => pid,
    };
    assert!(!alive(pid), "the child {pid} outlived the dropped run");
    assert!(no_leases(&config));
}

/// Catches: processes that outlive the kill wait reported as a clean end. With no
/// wait at all, the first round of SIGKILL is never confirmed, so the lease fails.
#[tokio::test]
async fn processes_that_outlive_the_kill_wait_fail_the_lease() {
    let dir = scratch("survivors");
    let mut config = config(&dir);
    config.kill_wait = Duration::ZERO;
    // Without the grace, the first thing the lease sends is SIGKILL.
    config.term_grace = Duration::ZERO;
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let outcome = run(&rt, &cas, 1, &Spec::act("spawn-child sleep 300 ; exit 0")).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("survived SIGKILL")),
        "{outcome:?}"
    );
    assert!(no_leases(&config));
}

/// The steps of a child that fills and holds 256 MiB, then sleeps.
const HOG: &str = "alloc 256 ;; sleep 300";

/// Catches: the memory watch skipped (the hog runs to its timeout), memory counted
/// for the leader only (the hog is the leader's child), the hog left alive after the
/// kill, and an out-of-memory end reported as anything but OutOfMemory with the limit.
#[tokio::test]
async fn an_action_past_its_limit_is_killed_whole() {
    let dir = scratch("oom");
    let mut config = config(&dir);
    config.memory = MemoryPolicy {
        percent: 100,
        headroom_bytes: 0,
    };
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let action = Spec::act(&format!("spawn-child --pid {PIDFILE} {HOG} ; sleep 300"))
        .timeout(Duration::from_secs(60))
        .store(&cas);
    let limit = 64 << 20;
    let pidfile = in_lease(&config, 1);
    let (outcome, pid) = tokio::join!(
        tokio::time::timeout(PROMPT * 3, rt.run(work(1, action, limit))),
        pid_in(&pidfile)
    );
    let outcome = outcome.expect("the watch ends the run long before its timeout");
    match outcome {
        Err(RuntimeError::OutOfMemory { used, limit: l }) => {
            assert_eq!(l, limit);
            assert!(used > limit, "{used} <= {limit}");
        }
        other => panic!("not out of memory: {other:?}"),
    }
    assert!(!alive(pid), "the hog {pid} outlived the kill");
    assert!(no_leases(&config));
}

/// Catches: a limit applied to an action within it (killed for memory it does not
/// hold), or a limit applied when nothing was booked.
#[tokio::test]
async fn an_action_within_its_limit_runs_to_the_end() {
    let dir = scratch("within");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    for (seq, booked) in [(1, 64_u64 << 20), (2, 0)] {
        let action = Spec::act("alloc 32 ; sleep 0.3").store(&cas);
        let result = rt.run(work(seq, action, booked)).await.expect("ran");
        assert_eq!(result.exit_code, 0, "booked {booked}");
    }
}

/// Catches a usage invented for the plain-process runtime, which keeps no footprint
/// counters and no cgroup: a finished lease carries none in its result metadata, and
/// a lease that ended in an error (here a timeout) has none for the daemon to ask for.
#[tokio::test]
async fn a_lease_reports_no_usage() {
    let dir = scratch("usage-none");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config(&dir), &cas);
    let result = run(&rt, &cas, 1, &Spec::act("burn 0.2"))
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0, "{}", support::stderr(&cas, &result));
    assert_eq!(ResourceUsage::in_result(&result), None);
    assert_eq!(
        rt.failed_usage(LeaseId::new(1, 1)),
        None,
        "a result has none"
    );
    let spec = Spec::act("burn 30").timeout(Duration::from_millis(500));
    let outcome = run(&rt, &cas, 2, &spec).await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut { .. })),
        "{outcome:?}"
    );
    assert_eq!(
        rt.failed_usage(LeaseId::new(1, 2)),
        None,
        "a timeout has none"
    );
}

/// A fresh name prefix for the shared-memory names of one test.
fn shm_prefix(tag: &str) -> String {
    format!("kd{}{tag}", std::process::id() % 100_000)
}

/// Starts `shm-owner` as lease `1.1`, waits until its four names exist, and kills the
/// lease; returns how the run ended and the names left once the kill has returned.
async fn kill_an_owner(
    config: kbf_driver_native::NativeConfig,
    tag: &str,
) -> (String, Vec<String>) {
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));
    let prefix = shm_prefix(tag);
    kbf_testkit::shm::unlink(&prefix, 2);
    let action = Spec::act(&format!("shm-owner {prefix} 2")).store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(work(1, action, 0)).await }
    });
    let until = tokio::time::Instant::now() + PROMPT;
    while kbf_testkit::shm::present(&prefix, 2).len() < 4 {
        assert!(tokio::time::Instant::now() < until, "the names never came");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The tracker is ready before the owner says so; give the action's first poll time.
    tokio::time::sleep(Duration::from_millis(200)).await;
    rt.kill(kbf_types::LeaseId::new(1, 1)).await;
    let left = kbf_testkit::shm::present(&prefix, 2);
    kbf_testkit::shm::unlink(&prefix, 2);
    let outcome = running.await.expect("run task");
    assert!(no_leases(&config));
    (format!("{outcome:?}"), left)
}

/// Catches: a lease that sends SIGKILL to its whole tree at once, which leaves a
/// resource tracker no time to unlink the shared memory and semaphores the workload
/// made. With the default grace, SIGTERM goes first and every name is gone when the
/// kill returns.
#[tokio::test]
async fn a_tracker_unlinks_its_names_when_the_lease_ends() {
    let dir = scratch("tracker-term");
    let (outcome, left) = kill_an_owner(config(&dir), "a").await;
    assert!(outcome.contains("Killed"), "{outcome}");
    assert_eq!(left, Vec::<String>::new());
}

/// Catches: a grace of zero that still waits or sends SIGTERM (zero must mean SIGKILL at
/// once), and a test double that cannot show a leak: with nothing to unlink them, all
/// four names are left.
#[tokio::test]
async fn a_grace_of_zero_kills_at_once_and_leaves_the_names() {
    let dir = scratch("tracker-kill");
    let mut config = config(&dir);
    config.term_grace = Duration::ZERO;
    let (outcome, left) = kill_an_owner(config, "b").await;
    assert!(outcome.contains("Killed"), "{outcome}");
    assert_eq!(left.len(), 4, "{left:?}");
}

/// Catches: a process that ignores SIGTERM left out of the result, one that ends on
/// SIGTERM named as if it had not, a count or a pid that is not the survivor's, a note
/// missing the program's name and arguments, a survivor left alive after the lease, an
/// action failed for it (its exit code stands), and a grace that is waited past its
/// bound or not at all.
#[tokio::test]
async fn a_process_that_ignores_sigterm_is_named_in_the_result() {
    let dir = scratch("ignores");
    let mut config = config(&dir);
    config.term_grace = Duration::from_millis(400);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::act(
        "spawn-child sleep 300 ; \
         spawn-child --pid - ignore-signal 15 ;; sleep 300 ; sleep 0.5 ; exit 3",
    );
    let started = std::time::Instant::now();
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    let took = started.elapsed();
    let pid = pid_of(&cas, &result);
    let stderr = support::stderr(&cas, &result);
    assert_eq!(result.exit_code, 3, "{stderr}");
    assert!(!alive(pid), "the process {pid} outlived the lease");
    assert!(
        stderr.contains(&format!(
            "kbf: 1 process(es) outlasted SIGTERM and were killed: pid {pid} "
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains("[kbf-act ignore-signal 15 ; sleep 300]"),
        "{stderr}"
    );
    assert!(took >= Duration::from_millis(900), "no grace: {took:?}");
    // An unbounded grace would wait out the child's `sleep 300`.
    assert!(
        took < Duration::from_secs(60),
        "the grace was not bounded: {took:?}"
    );
    assert!(no_leases(&config));
}

/// Catches: a note added for an action whose processes all ended on SIGTERM, which
/// would name every lease that leaves a background process.
#[tokio::test]
async fn processes_that_end_on_sigterm_are_not_reported() {
    let dir = scratch("obeys");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config(&dir), &cas);
    let spec = Spec::act("spawn-child --pid - sleep 300 ; sleep 0.3 ; echo --stderr done");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    let stderr = support::stderr(&cas, &result);
    assert_eq!(stderr, "done\n");
    assert!(!alive(pid_of(&cas, &result)));
}
