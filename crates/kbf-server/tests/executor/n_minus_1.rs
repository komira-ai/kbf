//! A shared executor over a state store an earlier frontend wrote (`docs/design/n-frontends.md`,
//! section 10): the store holds no `exec/` rows, and the executor starts, writes its own
//! liveness and worker rows, and finds nothing else (no worker to adopt, no job to finish) and
//! asks the job service nothing about other frontends' jobs.

use std::sync::Arc;

use kbf_server::executor::rows::ExecRows;
use kbf_server::executor::{Config, Deps, Executor, Ownership};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Cond, ManualClock, Store, Txn};

use crate::rig::{Rig, cancels, fresh_instance};

/// What an earlier frontend left in the store: one row of a family it knows, as bytes no
/// reader of `exec/` could take for its own.
const EARLIER: (&str, &[u8]) = ("member/7", b"the row of an earlier release");

/// Catches a shared executor that cannot start over a store with no `exec/` rows, or that
/// takes another family's row for its own: over a store that holds one row of an older family
/// the executor registers its two workers, writes exactly one liveness row (its group, not
/// draining) and one worker row for each worker (owned by this process, named by its
/// instance), no job row, leaves the older row byte for byte, counts no unknown row, and makes
/// no call to the job service that looks for or cancels another frontend's jobs.
#[tokio::test(start_paused = true)]
async fn a_shared_executor_over_a_store_with_no_exec_rows_writes_its_own_and_finds_nothing_else() {
    let mut rig = Rig::start(&["node1"], 2, |_| {}).await;
    let fake = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let put = Txn::new().put(EARLIER.0, EARLIER.1.to_vec(), Cond::Any);
    fake.txn(put).expect("store").expect("no conflict");
    let store: Arc<dyn Store> = Arc::clone(&fake) as Arc<dyn Store>;
    let before = store.scan_prefix("").expect("scan");
    assert!(
        before.iter().all(|(key, _)| !key.starts_with("exec/")),
        "the store holds no exec/ row yet"
    );

    let cfg = Config {
        ownership: Ownership::Shared,
        instance: fresh_instance(),
        ..rig.cfg.clone()
    };
    let deps = Deps {
        farm: Arc::clone(&rig.farm),
        client: rig.fake.client().expect("a client"),
        ingest: rig.ingest.clone(),
        inputs: rig.inputs.clone(),
        metrics: Arc::clone(&rig.metrics),
        store: Some(Arc::clone(&store)),
    };
    rig.exec = Executor::new(cfg.clone(), deps).expect("a valid config");
    rig.cfg = cfg.clone();
    rig.pump().await;

    let rows = ExecRows::new(Arc::clone(&store));
    let alive = rows.live_instances().expect("liveness rows");
    assert_eq!(alive.len(), 1, "{alive:?}");
    assert_eq!(alive[0].0, cfg.instance);
    assert_eq!(alive[0].1.group, "front-1");
    assert!(!alive[0].1.draining);

    let workers = rows.workers().expect("worker rows");
    let names: Vec<&str> = workers.iter().map(|w| w.worker.as_str()).collect();
    assert_eq!(names, [cfg.worker_name(0), cfg.worker_name(1)]);
    for entry in &workers {
        assert_eq!(entry.row.owner_instance, cfg.instance);
        assert!(rows.jobs(&entry.worker).expect("job rows").is_empty());
    }

    let after = store.scan_prefix("").expect("scan");
    assert_eq!(
        after.len(),
        before.len() + 3,
        "a liveness row and two workers"
    );
    let older = after.iter().find(|(key, _)| key == EARLIER.0);
    assert_eq!(older.map(|(_, row)| row.value.as_slice()), Some(EARLIER.1));
    assert_eq!(rows.unknown_rows(), 0);

    let methods = rig.methods();
    assert!(!methods.contains(&"ListJobs"), "{methods:?}");
    assert!(cancels(&rig.fake.calls()).is_empty());
}
