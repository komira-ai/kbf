//! An executor over a state store an earlier frontend wrote (`docs/design/n-frontends.md`,
//! section 10): the store holds no `exec/` rows, and the executor starts, writes its own
//! liveness row, and finds nothing else (no operation to take up) and asks the job service
//! nothing.

use std::time::Duration;

use kbf_server::executor::Running;
use kbf_state::{Cond, Store, Txn};

use crate::rig::{HOST, PROMPT, Rig, Shared};

/// What an earlier frontend left in the store: one row of a family it knows, as bytes no
/// reader of `exec/` could take for its own.
const EARLIER: (&str, &[u8]) = ("member/7", b"the row of an earlier release");

/// Catches an executor that cannot start over a store with no `exec/` rows, or that takes
/// another family's row for its own: over a store that holds one row of an older family the
/// executor writes exactly one liveness row, no operation row, leaves the older row byte for
/// byte, finds no operation to take up, and makes no call to the job service.
#[tokio::test]
async fn an_executor_over_a_store_with_no_exec_rows_writes_its_own_and_finds_nothing_else() {
    let shared = Shared::new();
    let put = Txn::new().put(EARLIER.0, EARLIER.1.to_vec(), Cond::Any);
    shared.store.txn(put).expect("store").expect("no conflict");
    let store = shared.handle();
    let before = store.scan_prefix("").expect("scan");
    assert!(
        before.iter().all(|(key, _)| !key.starts_with("exec/")),
        "the store holds no exec/ row yet"
    );

    let rig = Rig::over(&[HOST], Some(shared.handle()), |cfg| {
        cfg.live = Duration::from_millis(400);
    })
    .await;
    let running = Running::spawn(&rig.exec);
    let live = format!("exec/live/{}", rig.cfg.instance);
    let wait = async {
        while shared.keys("exec/live/").is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(PROMPT, wait)
        .await
        .expect("a liveness row");
    assert_eq!(shared.keys("exec/"), [live]);

    let after = store.scan_prefix("").expect("scan");
    assert_eq!(after.len(), before.len() + 1, "only a liveness row");
    let older = after.iter().find(|(key, _)| key == EARLIER.0);
    assert_eq!(older.map(|(_, row)| row.value.as_slice()), Some(EARLIER.1));
    let rows = shared.rows("another-process");
    assert!(rows.orphans().expect("a scan").is_empty());
    assert_eq!(rows.unknown_rows(), 0);
    assert!(rig.methods().is_empty(), "{:?}", rig.methods());
    running.stop().await;
}
