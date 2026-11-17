//! Shared ownership (`--executor-ownership shared`) through the driver: any number of
//! frontends of one group over one state store serve beside each other, and none of them
//! cancels a job of another or of its own when it ends.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::JobState;
use kbf_server::executor::rows::ExecRows;
use kbf_server::executor::{ConfigError, Deps, Executor, Running};

use crate::pool::{earlier, job};
use crate::rig::{Rig, Shared, cancels, settle};

/// Lets `seconds` pass in steps of 5 s on every clock (tokio's, the job service's and the
/// state store's), a second at a time with the drivers' tasks let run in between, the farm
/// ticking after each step.
async fn pass(rig: &Rig, shared: &Shared, seconds: u64) {
    for _ in 0..seconds.div_ceil(5) {
        for _ in 0..5 {
            rig.advance(Duration::from_secs(1)).await;
            shared.advance(Duration::from_secs(1));
            settle().await;
        }
        rig.farm.tick().await;
        settle().await;
    }
}

/// Seconds enough for two checks for a rival and the watch of an earlier process's jobs, each
/// longer than the liveness row lives.
fn long(rig: &Rig) -> u64 {
    rig.cfg.adopt_watch().as_secs() * 2 + 60
}

/// Whether `running` has not stopped by itself.
async fn goes_on(running: &mut Running) -> bool {
    tokio::time::timeout(Duration::ZERO, running.failed())
        .await
        .is_err()
}

/// The workers the farm knows, in node-id order.
fn nodes(rig: &Rig) -> Vec<String> {
    let view = rig.farm.nodes();
    view.nodes.into_iter().map(|node| node.node_id).collect()
}

/// The worker rows of the store, by name.
fn worker_rows(rows: &ExecRows) -> Vec<String> {
    let entries = rows.workers().expect("worker rows");
    entries.into_iter().map(|entry| entry.worker).collect()
}

/// Catches two frontends of one group that cannot serve beside each other: each steps for far
/// longer than a liveness row lives and neither stops (no rival, no twin), the farm has both
/// sets of workers under distinct ids `js:<group>.<8 characters of the instance>-<n>`, every
/// action becomes a job of the group under one of the two processes, each job has its row, the
/// jobs end and their callers are told, and neither process cancelled anything.
#[tokio::test(start_paused = true)]
async fn two_frontends_of_one_group_serve_beside_each_other() {
    let rig = Rig::start(&["node1", "node2"], 2, |_| {}).await;
    let shared = Shared::new();
    let mut a = Running::spawn(rig.shared_executor(&shared));
    let mut b = Running::spawn(rig.shared_executor(&shared));
    pass(&rig, &shared, long(&rig)).await;
    assert!(goes_on(&mut a).await, "the first frontend stopped");
    assert!(goes_on(&mut b).await, "the second frontend stopped");

    let ids = nodes(&rig);
    assert_eq!(ids.len(), 4, "{ids:?}");
    let owners: BTreeSet<&str> = ids
        .iter()
        .map(|id| {
            let rest = id.strip_prefix("js:front-1.").expect("a group worker id");
            let (short, n) = rest.split_once('-').expect("a worker number");
            assert_eq!(short.len(), 8, "{id}");
            assert!(n == "0" || n == "1", "{id}");
            short
        })
        .collect();
    assert_eq!(owners.len(), 2, "{ids:?}");
    assert_eq!(worker_rows(&shared.rows()).len(), 4);

    let callers: Vec<_> = (1..=4).map(|tag| rig.submit(tag)).collect();
    pass(&rig, &shared, 20).await;
    let posted: Vec<_> = (1..=4).flat_map(|tag| rig.jobs_of(tag)).collect();
    assert_eq!(posted.len(), 4, "{posted:?}");
    let instances: BTreeSet<_> = posted
        .iter()
        .map(|p| {
            p.spec
                .labels
                .get("kbf-instance")
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    assert!(
        !instances.is_empty() && instances.len() <= 2,
        "{instances:?}"
    );
    for p in &posted {
        assert_eq!(
            p.spec.labels.get("kbf-frontend").map(String::as_str),
            Some("front-1")
        );
    }
    let rows = shared.rows();
    let with_rows: usize = worker_rows(&rows)
        .iter()
        .map(|worker| rows.jobs(worker).expect("job rows").len())
        .sum();
    assert_eq!(with_rows, 4);

    for p in &posted {
        rig.host_finishes(p.id(), "node1", 0);
    }
    pass(&rig, &shared, 30).await;
    for caller in &callers {
        assert!(
            caller.finished().is_some(),
            "action {} not done",
            caller.tag
        );
    }
    assert!(goes_on(&mut a).await && goes_on(&mut b).await);
    a.stop().await;
    b.stop().await;
    assert!(cancels(&rig.fake.calls()).is_empty());
    assert!(shared.keys().is_empty(), "{:?}", shared.keys());
}

/// Catches a hard stop that cancels the jobs it started or leaves its rows as if it still ran:
/// with a job open, stopping the process sends no `CancelJob`, the job stays live and keeps
/// its row, the worker row is kept with no owner, and the liveness row is gone.
#[tokio::test(start_paused = true)]
async fn a_hard_stop_releases_and_cancels_nothing() {
    let rig = Rig::start(&["node1"], 1, |_| {}).await;
    let shared = Shared::new();
    let running = Running::spawn(rig.shared_executor(&shared));
    let _caller = rig.submit(1);
    pass(&rig, &shared, 20).await;
    let posted = rig.jobs_of(1);
    assert_eq!(posted.len(), 1, "{posted:?}");
    let id = posted[0].id().to_owned();
    let rows = shared.rows();
    let workers = worker_rows(&rows);
    assert_eq!(workers.len(), 1, "{workers:?}");
    assert_eq!(rows.jobs(&workers[0]).expect("job rows").len(), 1);
    assert_eq!(rows.live_instances().expect("liveness").len(), 1);

    running.stop().await;

    assert!(cancels(&rig.fake.calls()).is_empty());
    let after = job(&rig, &id);
    assert!(!after.cancel_requested, "the job was asked to stop");
    assert_eq!(after.state(), JobState::Queued);
    let kept = rows.jobs(&workers[0]).expect("job rows");
    assert_eq!(kept.len(), 1, "the row of the job went");
    assert_eq!(kept[0].job, id);
    let entries = rows.workers().expect("worker rows");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].worker, workers[0]);
    assert_eq!(entries[0].row.owner_instance, "");
    assert!(rows.live_instances().expect("liveness").is_empty());
}

/// Catches a graceful end that leaves rows for no one: with no job open, stopping deletes the
/// worker rows and the liveness row, and nothing is left under `exec/`.
#[tokio::test(start_paused = true)]
async fn a_graceful_end_with_no_job_open_deletes_every_row() {
    let rig = Rig::start(&["node1"], 2, |_| {}).await;
    let shared = Shared::new();
    let running = Running::spawn(rig.shared_executor(&shared));
    pass(&rig, &shared, 20).await;
    assert_eq!(shared.keys().len(), 3, "{:?}", shared.keys());
    assert_eq!(nodes(&rig).len(), 2);

    running.stop().await;

    assert!(shared.keys().is_empty(), "{:?}", shared.keys());
    assert!(cancels(&rig.fake.calls()).is_empty());
}

/// Catches a restart that cancels what an earlier process of its group left or stops because
/// of it: live jobs of another process of the same group are on the job service when a shared
/// executor starts, and after longer than the watch of earlier jobs it has cancelled none of
/// them, has not stopped, and has its workers.
#[tokio::test(start_paused = true)]
async fn a_restart_cancels_nothing() {
    let rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    let shared = Shared::new();
    let left = [
        earlier(&rig, "front-1", "elsewhere"),
        earlier(&rig, "front-1", "elsewhere"),
    ];
    let mut running = Running::spawn(rig.shared_executor(&shared));
    pass(&rig, &shared, long(&rig)).await;
    assert!(goes_on(&mut running).await, "the executor stopped");
    assert_eq!(nodes(&rig).len(), 1);
    for id in &left {
        assert!(!job(&rig, id).cancel_requested, "{id} was cancelled");
    }
    running.stop().await;
    assert!(cancels(&rig.fake.calls()).is_empty());
    for id in &left {
        assert_eq!(job(&rig, id).state(), JobState::Queued);
    }
}

/// Catches shared ownership with nowhere to keep its rows being accepted: building an
/// executor with `--executor-ownership shared` and no state store is refused as
/// `ConfigError::SharedStore`, and nothing is asked of the job service.
#[tokio::test(start_paused = true)]
async fn shared_with_no_store_is_refused() {
    let rig = Rig::start(&["node1"], 1, |_| {}).await;
    let deps = Deps {
        farm: rig.farm.clone(),
        client: rig.fake.client().expect("a client"),
        ingest: rig.ingest.clone(),
        inputs: rig.inputs.clone(),
        metrics: Arc::default(),
        store: None,
    };
    let refused = Executor::new(rig.shared_config(), deps);
    assert_eq!(refused.err(), Some(ConfigError::SharedStore));
    assert!(rig.fake.calls().is_empty());
}

/// Catches a shared executor that looks for rivals: with a job of another frontend live on the
/// job service, which a single-owner executor of the rig names as a rival, the shared one's
/// `rival` is `None` and it asks the job service nothing (no `ListJobs`, no call at all).
#[tokio::test(start_paused = true)]
async fn rival_of_a_shared_executor_asks_the_job_service_nothing() {
    let rig = Rig::start(&["node1"], 1, |_| {}).await;
    let shared = Shared::new();
    earlier(&rig, "front-2", "elsewhere");
    let exec = rig.shared_executor(&shared);
    assert_eq!(exec.rival().await, None);
    assert!(rig.methods().is_empty(), "{:?}", rig.methods());
    let single = rig.restarted_executor();
    assert!(single.rival().await.is_some());
    assert_eq!(rig.methods(), ["ListJobs"]);
}
