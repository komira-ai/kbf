//! The `exec/` rows of the shared executor (`docs/design/n-frontends.md`, sections 4.2 and 10)
//! beside a scheduler that does not know them: a frontend of the release before the rows
//! opens the store they are in, and serves, as it does without them.
//!
//! The farm here is the scheduler of a frontend over the store, which reads the families it
//! knows by their own prefixes and never lists `exec/` (the test `exec_rows_prefixes` holds
//! that no prefix reaches it).

use kbf_server::executor::rows::{ExecRows, JobRow, Liveness, Shape, WorkerRow};

use super::*;

const LIVE: Duration = Duration::from_secs(45);

/// The `exec/` rows of the store, as keys and bytes.
fn exec_rows(disk: &Disk) -> Vec<(String, Vec<u8>)> {
    let rows = disk.store.scan_prefix("exec/").expect("scan");
    rows.into_iter()
        .map(|(key, row)| (key, row.value))
        .collect()
}

/// Writes what a shared executor keeps in the store while it serves: a liveness row, a
/// worker row it holds, and the row of one job of that worker.
fn write_exec_rows(disk: &Disk) {
    let store: Arc<dyn Store> = disk.store.clone();
    let rows = ExecRows::new(store);
    let live = Liveness {
        group: "front-1".to_owned(),
        started_ms: 1_700_000_000_000,
        release: "9.9.9".to_owned(),
        draining: false,
    };
    let worker = "js:front-1.0123abcd-0";
    let held = rows
        .create_worker(
            worker,
            &WorkerRow {
                owner_instance: "0123abcd0123abcd".to_owned(),
                farm_instance: "farm-instance-1".to_owned(),
                shape: Shape {
                    cpu_millis: 8_000,
                    memory_bytes: 32 * GIB,
                    slots: 4,
                    arch: "x86_64".to_owned(),
                },
            },
        )
        .expect("a worker row");
    let job = JobRow {
        lease_term: 3,
        lease_seq: 9,
        action: "aa".repeat(32),
        pack_key: "cas/b/3-9/aa-0".to_owned(),
        submitted_ms: 1_700_000_000_500,
    };
    rows.insert_job(&held, "job-0001", &job).expect("a job row");
    rows.beat("0123abcd0123abcd", &live, LIVE, &[held])
        .expect("a liveness row");
}

/// Asserts that a scheduler opened over a store that holds the three kinds of `exec/` row
/// opens, grants, takes a result and restarts as it does over a store without them, and
/// leaves the rows as it found them: the rows are invisible to it. Catches a scan prefix of
/// the scheduler's families that reaches `exec/`, and a startup that refuses or rewrites a
/// row it does not know.
#[tokio::test(start_paused = true)]
async fn a_farm_over_a_store_holding_exec_rows_serves_and_leaves_them_alone() {
    let mut disk = Disk::new();
    write_exec_rows(&disk);
    let before = exec_rows(&disk);
    assert_eq!(before.len(), 3, "a liveness, a worker and a job row");

    let farm = disk
        .try_farm(EpochPolicy::Keep)
        .expect("a farm over the store");
    let (stream, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert!(farm.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(accepted(&farm, lease).await, "the result is taken");
    assert_eq!(disk.rows("op/"), 0, "the finished operation is gone");
    assert_eq!(disk.rows("lease/"), 0);
    assert_eq!(exec_rows(&disk), before);

    disk.crash(farm);
    let after = disk.try_farm(EpochPolicy::Keep).expect("a restarted farm");
    let (_, mut inbox) = register(&after);
    submit(&after, request(2)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1, "the restarted farm grants");
    assert_eq!(exec_rows(&disk), before);
}
