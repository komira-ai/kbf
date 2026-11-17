//! The task that keeps an executor's place in the state store beside the listeners: it writes
//! the process's liveness row, takes up the unfinished operations of processes that are gone,
//! and on its stop deletes its row and cancels nothing.

use std::time::Duration;

use kbf_front::{Dispatch as _, Finished, Stage};
use kbf_server::executor::{Running, parse_stored_name};

use crate::rig::{HOST, PROMPT, Rig, Shared, cancels};

/// Liveness short enough for a test to wait for a sweep: the row is renewed every quarter and
/// the store swept every half of it.
const LIVE: Duration = Duration::from_millis(400);

/// Waits until `until` holds of the keys under `prefix` in `shared`.
async fn keys_until(
    shared: &Shared,
    prefix: &str,
    until: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    let found = async {
        loop {
            let keys = shared.keys(prefix);
            if until(&keys) {
                return keys;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(PROMPT, found)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the keys under {prefix} never were as wanted: {:?}",
                shared.keys(prefix)
            )
        })
}

/// Catches a process that does not say it is alive, or leaves its row behind when it ends, or
/// cancels the work it leaves for another frontend: a running driver writes the one liveness row
/// of its process; stopped, it deletes the row, and the job of the operation it still holds is
/// not cancelled but goes on to its end, which answers the operation's caller.
#[tokio::test]
async fn a_driver_keeps_the_liveness_row_while_it_runs_and_cancels_nothing_when_it_stops() {
    let shared = Shared::new();
    let rig = Rig::over(&[HOST], Some(shared.handle()), |cfg| cfg.live = LIVE).await;
    let running = Running::spawn(&rig.exec);
    let live = format!("exec/live/{}", rig.cfg.instance);
    let keys = keys_until(&shared, "exec/live/", |keys| !keys.is_empty()).await;
    assert_eq!(keys, [live]);

    let mut caller = rig.submit(1);
    let job = rig.first_job(1).await;
    rig.host_runs(job.id(), HOST);
    running.stop().await;
    assert!(shared.keys("exec/live/").is_empty(), "the row was left");
    assert!(
        cancels(&rig.fake.calls()).is_empty(),
        "a stop cancelled a job"
    );

    assert!(rig.fake.finish(job.id(), 0));
    assert!(matches!(caller.done().await, Finished::Ran(_)));
    assert_eq!(rig.jobs_of(1).len(), 1);
}

/// An executor of `rig` over `shared` that is submitted `tag` on a runtime of its own, which
/// then ends: the process that opened the operation is gone, its liveness row never written.
/// The operation's name.
async fn left_by_a_process_that_is_gone(rig: &Rig, shared: &Shared, tag: u8) -> String {
    let gone = rig.other_executor("front-1", Some(shared.handle()));
    let side = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a runtime");
    let name = {
        let _in = side.enter();
        rig.submit_to(&gone, tag, false, None).name
    };
    rig.first_job(tag).await;
    // The process ends: its tasks stop, and nothing of it is written again.
    side.shutdown_background();
    name
}

/// Catches an operation lost with the process that opened it: a process that submitted an
/// operation that writes into a put directory (whose key is in its row) and ended without a
/// word leaves the row, and a driver of another process takes it up, reads its one job where
/// it is, and answers a caller that asks for the operation by name when the job ends: one job,
/// no second submit, the take-up counted once.
#[tokio::test]
async fn a_driver_takes_up_the_put_dir_operation_of_a_process_that_is_gone() {
    let shared = Shared::new();
    let rig = Rig::over(&[HOST], Some(shared.handle()), |cfg| cfg.live = LIVE).await;
    let name = left_by_a_process_that_is_gone(&rig, &shared, 1).await;
    let job = rig.first_job(1).await;
    rig.host_runs(job.id(), HOST);
    assert_eq!(rig.exec.open(), 0);

    let running = Running::spawn(&rig.exec);
    rig.counter_reaches("kbf_executor_operations_reattached_total", 1)
        .await;
    let mut ticket = rig.exec.wait(&name).expect("the operation is known");
    assert!(rig.fake.finish(job.id(), 0));
    let done = tokio::time::timeout(
        PROMPT,
        ticket
            .stage
            .wait_for(|stage| matches!(stage, Stage::Done(_))),
    )
    .await;
    let done = done.expect("answered").expect("open").clone();
    let Stage::Done(finished) = done else {
        unreachable!("waited for done")
    };
    assert!(matches!(finished, Finished::Ran(_)), "{finished:?}");
    assert_eq!(rig.jobs_of(1).len(), 1, "a second job was made");
    assert!(cancels(&rig.fake.calls()).is_empty());
    assert_eq!(rig.counter("kbf_executor_operations_reattached_total"), 1);
    running.stop().await;
}

/// Catches an operation taken up with a pack nobody can find: the row of an operation that
/// holds no put directory leaves no pack to read, so the driver that takes it up cancels the
/// old job and runs the operation again as its next attempt, a new job of a new id, which
/// answers the caller.
#[tokio::test]
async fn a_driver_runs_the_operation_of_a_gone_process_whose_row_names_no_directory_again() {
    let shared = Shared::new();
    let rig = Rig::over(&[HOST], Some(shared.handle()), |cfg| cfg.live = LIVE).await;
    let name = left_by_a_process_that_is_gone(&rig, &shared, 1).await;
    let operation = parse_stored_name(&name).expect("an operation name");
    let left = shared
        .rows("reader")
        .op(operation)
        .expect("read")
        .expect("a row");
    let owner = shared.rows(&left.owner);
    let keyless = kbf_server::executor::rows::OpRow { key: None, ..left };
    owner.put_op(operation, &keyless).expect("rewritten");
    let first = rig.first_job(1).await;
    rig.host_runs(first.id(), HOST);

    let running = Running::spawn(&rig.exec);
    let second = rig.until_jobs(1, 2).await.remove(1);
    assert!(second.id().ends_with("-1"), "{}", second.id());
    assert_eq!(cancels(&rig.fake.calls()), [first.id().to_owned()]);
    let mut ticket = rig.exec.wait(&name).expect("the operation is known");
    rig.host_finishes(second.id(), HOST, 0);
    let done = tokio::time::timeout(
        PROMPT,
        ticket
            .stage
            .wait_for(|stage| matches!(stage, Stage::Done(_))),
    )
    .await;
    let done = done.expect("answered").expect("open").clone();
    let Stage::Done(finished) = done else {
        unreachable!("waited for done")
    };
    assert!(matches!(finished, Finished::Ran(_)), "{finished:?}");
    running.stop().await;
}
