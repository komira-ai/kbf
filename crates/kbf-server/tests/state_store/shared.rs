//! Several frontends over one state store: the ids they number with, and the epoch they
//! serve under. Each test runs over the fake, a SQLite file and a Postgres schema (the
//! `restarts!` list of the parent module).

use std::collections::BTreeSet;
use std::sync::{Arc, Barrier};

use super::*;

/// The epoch row of the store, as a number.
fn stored_epoch(disk: &Disk) -> u64 {
    let row = disk
        .store
        .get("meta/epoch")
        .expect("read")
        .expect("an epoch");
    u64::from_be_bytes(row.value.as_slice().try_into().expect("eight bytes"))
}

/// The leases `farm` holds, each with the operation it runs.
fn held(farm: &TestFarm) -> Vec<(LeaseId, OperationId)> {
    let views = farm.lease_views(None);
    views
        .leases
        .iter()
        .map(|view| (view.lease, view.operation))
        .collect()
}

/// Submits the requests `tags` name to `farm`, which has a node with room for them, and
/// returns the leases it holds then.
fn submit_all(farm: &TestFarm, tags: &[u8]) -> Vec<(LeaseId, OperationId)> {
    for &tag in tags {
        submit(farm, request(tag)).expect("submitted");
    }
    held(farm)
}

/// Whether `farm` accepts the result `worker` reports for `lease`.
async fn finished(farm: &TestFarm, worker: &WorkerId, lease: LeaseId) -> bool {
    let ack = farm.report(worker, invalid(lease)).await;
    ack.is_some_and(|ack| ack.accepted)
}

/// Finishes every lease `farm` holds, which `worker` ran.
async fn finish_all(farm: &TestFarm, worker: &WorkerId) {
    for (lease, _) in held(farm) {
        assert!(finished(farm, worker, lease).await, "{lease:?}");
    }
}

/// Asserts that several frontends sharing one store never number two operations or two
/// leases alike: not while they run side by side, not after their operations finish and
/// the rows that guarded their ids are deleted, and not in the frontends a restart starts,
/// whose own counters know nothing of what the ones before them numbered. Every id the
/// later frontends take is past every id taken before.
pub(super) async fn frontends_over_one_store_never_hand_out_the_same_id(backing: Backing) {
    let mut disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let (node_a, node_b) = (WorkerId::new("node-a"), WorkerId::new("node-b"));
    let (_, _inbox_a) = register_as(&a, &node_a);
    let (_, _inbox_b) = register_as(&b, &node_b);
    let mut first = submit_all(&a, &[1, 2, 3]);
    first.extend(submit_all(&b, &[11, 12, 13]));
    let mut leases: BTreeSet<LeaseId> = first.iter().map(|(lease, _)| *lease).collect();
    let mut operations: BTreeSet<OperationId> = first.iter().map(|(_, op)| *op).collect();
    assert_eq!(first.len(), 6);
    assert_eq!(leases.len(), 6, "{first:?}");
    assert_eq!(operations.len(), 6, "{first:?}");

    finish_all(&a, &node_a).await;
    finish_all(&b, &node_b).await;
    assert_eq!((disk.rows("op/"), disk.rows("lease/")), (0, 0));

    // The frontends run on, and the rows that guarded the ids are gone.
    let mut middle = submit_all(&a, &[4]);
    middle.extend(submit_all(&b, &[14]));
    let middle_leases: BTreeSet<LeaseId> = middle.iter().map(|(lease, _)| *lease).collect();
    let middle_operations: BTreeSet<OperationId> = middle.iter().map(|(_, op)| *op).collect();
    assert_eq!(middle_leases.len(), 2, "{middle:?}");
    assert_eq!(middle_operations.len(), 2, "{middle:?}");
    assert!(
        middle_leases.is_disjoint(&leases),
        "{middle:?} after {first:?}"
    );
    assert!(
        middle_operations.is_disjoint(&operations),
        "{middle:?} after {first:?}"
    );
    leases.extend(middle_leases);
    operations.extend(middle_operations);
    finish_all(&a, &node_a).await;
    finish_all(&b, &node_b).await;
    assert_eq!((disk.rows("op/"), disk.rows("lease/")), (0, 0));
    drop(b);
    disk.crash(a);

    let farms = [
        disk.farm(EpochPolicy::Keep),
        disk.farm(EpochPolicy::Keep),
        disk.farm(EpochPolicy::Keep),
    ];
    let nodes = ["node-c", "node-d", "node-e"];
    let mut inboxes = Vec::new();
    let mut later = Vec::new();
    for ((farm, node), tag) in farms.iter().zip(nodes).zip(21..) {
        inboxes.push(register_as(farm, &WorkerId::new(node)));
        later.extend(submit_all(farm, &[tag, tag + 10]));
    }
    let later_leases: BTreeSet<LeaseId> = later.iter().map(|(lease, _)| *lease).collect();
    let later_operations: BTreeSet<OperationId> = later.iter().map(|(_, op)| *op).collect();
    assert_eq!(later_leases.len(), 6, "{later:?}");
    assert_eq!(later_operations.len(), 6, "{later:?}");
    let (last_lease, last_operation) = (leases.last(), operations.last());
    assert!(
        later_leases.first() > last_lease,
        "{later:?} after {first:?}"
    );
    assert!(
        later_operations.first() > last_operation,
        "{later:?} after {first:?}"
    );
}

/// Asserts that starting a second frontend over a store, and restarting either, does not
/// change the epoch the store holds or the one the frontends serve: daemons that reconnect
/// keep the runs of their leases.
pub(super) async fn a_second_frontend_started_over_the_store_keeps_its_epoch(backing: Backing) {
    let mut disk = Disk::over(backing);
    let first = disk.farm(EpochPolicy::Keep);
    let epoch = first.lease_epoch();
    let second = disk.farm(EpochPolicy::Keep);
    assert_eq!(second.lease_epoch(), epoch);
    assert_eq!(stored_epoch(&disk), epoch);
    drop(second);
    disk.crash(first);

    let restarted = disk.farm(EpochPolicy::Keep);

    assert_eq!(restarted.lease_epoch(), epoch);
    assert_eq!(restarted.term(), epoch);
    assert_eq!(stored_epoch(&disk), epoch);
}

/// Asserts that the operator's bump raises the epoch the store holds by one, once, and
/// leaves the epoch the running frontends serve as it was; a frontend started after it
/// serves the raised epoch and starting it raises nothing more.
pub(super) async fn the_operator_bump_raises_the_epoch_once(backing: Backing) {
    let disk = Disk::over(backing);
    let (a, b) = (disk.farm(EpochPolicy::Keep), disk.farm(EpochPolicy::Keep));
    let epoch = a.lease_epoch();

    let bumped = a.bump_epoch().expect("a bump").expect("a state store");

    assert_eq!((bumped.previous, bumped.epoch), (epoch, epoch + 1));
    assert_eq!(stored_epoch(&disk), epoch + 1);
    assert_eq!((a.lease_epoch(), b.lease_epoch()), (epoch, epoch));
    let started = disk.farm(EpochPolicy::Keep);
    assert_eq!(started.lease_epoch(), epoch + 1);
    assert_eq!(stored_epoch(&disk), epoch + 1);
}

/// Asserts that frontends that open an empty store at the same moment end with one epoch
/// between them, which is the one the store holds: the one that loses the race to make it
/// takes the winner's.
pub(super) async fn frontends_opening_an_empty_store_together_keep_one_epoch(backing: Backing) {
    let disk = Disk::over(backing);
    let store: Arc<dyn Store> = disk.store.clone();
    let barrier = Arc::new(Barrier::new(3));
    let mut opens = Vec::new();
    for _ in 0..3 {
        let (store, barrier) = (Arc::clone(&store), Arc::clone(&barrier));
        opens.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            let state = StateStore {
                store,
                epoch: EpochPolicy::Keep,
                members: Default::default(),
                history: None,
            };
            farm_over(state).lease_epoch()
        }));
    }
    let mut epochs = BTreeSet::new();
    for open in opens {
        epochs.insert(open.await.expect("a farm"));
    }

    assert_eq!(epochs.len(), 1, "{epochs:?}");
    assert_eq!(epochs.first().copied(), Some(stored_epoch(&disk)));
}
