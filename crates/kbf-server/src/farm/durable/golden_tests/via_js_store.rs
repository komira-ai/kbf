//! The scenarios of the golden files, written through `js-store` with this server's codec and
//! rows (`crate::farm_js`), and a store this module's own code wrote, read through it.
//!
//! Catches: a codec or an ext that writes a row differently from the store layer that runs
//! today (the rows are compared with the same two files, byte for byte, expiry included), and
//! a store written by this server's release that `js-store` reads as something else than the
//! store layer does.

use js_store::{Ending as JsEnding, Rows};
use kbf_types::sched::usage::LeaseCost;

use super::*;
use crate::farm_js::codec::KbfRowCodec;
use crate::farm_js::ext::KbfExt;
use crate::farm_js::load::load;

/// The `Ending` of a result `finished` of the operation of action `tag`, kept for a second.
fn js_ending(finished: &Finished, tag: u8) -> JsEnding {
    let action = ActionKey::try_from(&request(tag).key).expect("an action");
    JsEnding::new(finished.to_row(), Duration::from_secs(1)).with_parcel(named::encode(&action))
}

/// A handle of `js-store` on `store`, as the first frontend of the scenarios.
fn rows_over(store: &Arc<Fake>) -> Rows {
    let handle: Arc<dyn Store> = store.clone();
    let codec = Arc::new(KbfRowCodec);
    let policy = js_store::EpochPolicy::Keep;
    let (rows, _) = Rows::open(
        handle,
        policy,
        0x0102_0304_0506_0708,
        codec,
        Arc::new(KbfExt),
    )
    .expect("open");
    rows
}

/// [`opened`], through `js-store`.
fn js_opened() -> (Arc<Fake>, Rows) {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    kbf_state::share(&*store).expect("the shared marker");
    let rows = rows_over(&store);
    rows.reserve_operation(OperationId(4))
        .expect("operation ids");
    rows.reserve_seqs(0).expect("lease sequences");
    let node = WorkerId::new("node-a");
    rows.arrive(&node, FarmTime::from_millis(0))
        .expect("a Hello");
    let caps =
        kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    let up = js_store::WorkerUp {
        worker: node,
        instance: DaemonInstance::new("daemon-1"),
        predecessor_stopped: true,
        capacity: Resources::new(2_000, 1 << 30),
        caps,
    };
    rows.session(&up).expect("a session");
    (store, rows)
}

/// [`granted_request`], through `js-store`.
fn js_granted(rows: &Rows, operation: u64, seq: u64, request: &Request, size: Option<u64>) {
    let parcel = subject::encode(&subject());
    let submitted = rows.submit(OperationId(operation), request, &parcel, size);
    submitted.expect("submit");
    let grant = LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new("node-a"),
        cost: LeaseCost::default(),
    };
    rows.granted(&grant).expect("grant");
}

/// A write to the store of the rows `txn` holds.
fn apply(store: &Fake, txn: Txn) {
    store.txn(txn).expect("a store").expect("a write");
}

/// [`written`], through `js-store`.
fn js_written() -> Arc<Fake> {
    let (store, rows) = js_opened();
    let drain = js_store::StoredCordon::Draining {
        deadline_unix_ms: 1_234_567,
    };
    let cordon = rows.cordoned(&WorkerId::new("node-b"), Some(drain));
    cordon.expect("a drain");
    let cordon = js_store::StoredCordon::Cordoned;
    rows.cordoned(&WorkerId::new("node-c"), Some(cordon))
        .expect("a cordon");

    js_granted(&rows, 4, 0, &request(1), Some(7));
    let floors = FloorChange {
        raised: Some(Floor {
            key: request(1).key,
            ask: 16 << 30,
            stamp: 2,
        }),
        key_raised: Some(KeyFloor {
            key: 7,
            ask: 16 << 30,
            stamp: 2,
        }),
        ..FloorChange::default()
    };
    let name = KbfExt.summary_name(&size_key());
    let fate = fate();
    let queued = rows.requeued(&requeue(4, 0), Some(2), Some(&fate), &floors, Some(&name));
    queued.expect("a kill");

    let result = ActionResult {
        exit_code: 0,
        stdout_raw: b"out".to_vec(),
        ..ActionResult::default()
    };
    let partial = ActionResult {
        stderr_raw: b"cut".to_vec(),
        ..ActionResult::default()
    };
    let finished = [
        Finished::Failed(status(3, "ended")),
        Finished::Ran(Box::new(result)),
        Finished::TimedOut {
            status: status(4, "timed out"),
            partial: Box::new(partial),
        },
    ];
    for (i, finished) in (0..).zip(finished) {
        let (operation, seq, tag) = (5 + i, 1 + i, 2 + u8::try_from(i).expect("small"));
        js_granted(&rows, operation, seq, &request(tag), None);
        let ending = js_ending(&finished, tag);
        let done = rows.finished(OperationId(operation), LeaseId::new(5, seq), &ending);
        done.expect("a result");
    }

    js_granted(&rows, 8, 4, &request(9), Some(7));

    let estimates = [(size_key(), size_summary())];
    apply(&store, KbfExt.put_estimates(Txn::new(), &estimates));
    apply(&store, Txn::new().put(ALIVE, Vec::new(), Cond::Any));
    // Last, so that it moves no expiry: `js-store` has no seam for a wall time yet, and
    // nothing wires it into the server. The row is put as the embedder will put it, so the
    // bytes of the file are checked on both sides.
    let wall = Txn::new().put(
        opwall_key(OperationId(8)),
        opwall::encode(WALL),
        Cond::Absent,
    );
    apply(&store, wall);
    store
}

/// [`written_variants`], through `js-store`.
fn js_written_variants() -> Arc<Fake> {
    let (store, rows) = js_opened();
    let cordon = rows.cordoned(
        &WorkerId::new("node-a"),
        Some(js_store::StoredCordon::Cordoned),
    );
    cordon.expect("a cordon");
    let mut count = 0_u64;
    let mut next = || {
        let n = count;
        count += 1;
        (4 + n, n, 20 + u8::try_from(n).expect("small"))
    };

    let kinds = every_lease_kind().into_iter().cycle();
    for (qos, kind) in every_qos().into_iter().zip(kinds) {
        let (operation, seq, tag) = next();
        let held = Request {
            qos,
            kind,
            ..request(tag)
        };
        js_granted(&rows, operation, seq, &held, None);
    }

    let fate = fate();
    for failure in every_failure() {
        let (operation, seq, tag) = next();
        js_granted(&rows, operation, seq, &request(tag), None);
        let answer = Answer {
            outcome: Outcome::Failed(failure),
            memory_runs: fate.memory_runs.clone(),
            farm_runs: fate.farm_runs.clone(),
            ..answer(operation, seq)
        };
        let ending = js_ending(&finished_of(&answer, None), tag);
        let done = rows.finished(answer.operation, answer.lease, &ending);
        done.expect("a failure");
    }

    let (operation, _, tag) = next();
    let parcel = subject::encode(&subject());
    let submitted = rows.submit(OperationId(operation), &request(tag), &parcel, None);
    submitted.expect("submit");
    let refusal = Refusal {
        operation: OperationId(operation),
        waiters: Vec::new(),
        reason: "no node can run it".to_owned(),
    };
    let ending = js_ending(&refused_as(&refusal), tag);
    rows.refused(refusal.operation, &ending).expect("a refusal");
    apply(&store, Txn::new().put(ALIVE, Vec::new(), Cond::Any));
    store
}

/// Asserts that the scenario of `scheduler-rows.txt`, written through `js-store`, leaves the
/// rows of the file.
#[test]
fn the_rows_js_store_writes_are_the_rows_of_the_file() {
    assert_rows(&js_written(), GOLDEN);
}

/// Asserts the same for the values the first scenario leaves out (`scheduler-variants.txt`).
#[test]
fn the_rows_js_store_writes_for_every_variant_are_the_rows_of_its_file() {
    assert_rows(&js_written_variants(), VARIANTS);
}

/// Asserts that a store written by the store layer loads through `js-store` and this server's
/// checks as the store layer loads it: the same operations, requests, leases, loss counts,
/// fates, subjects, size keys, floors, key floors, the summaries they name, cordons and
/// counters; and that its lease rows, which carry no cost, are recovered with none.
#[test]
fn a_store_the_scheduler_wrote_loads_the_same_through_js_store() {
    let store = written();
    let handle: Arc<dyn Store> = store.clone();
    let legacy = Durable::open(handle, EpochPolicy::Keep, 1).expect("open").0;
    let want = legacy.load().expect("the store layer's load");
    let got = load(&rows_over(&store)).expect("js-store's load");
    let (stored, summaries) = (got.stored, got.summaries);

    assert_eq!(stored.next_operation, want.next_operation);
    assert_eq!(stored.next_seq, want.next_seq);
    assert_eq!(stored.floors, want.floors);
    assert_eq!(stored.key_floors, want.key_floors);
    assert_eq!(summaries, want.key_names);
    assert_eq!(
        format!("{:?}", stored.cordons),
        format!("{:?}", want.cordons)
    );
    assert_eq!(stored.operations.len(), want.operations.len());
    let mut held = 0;
    for (got, want) in stored.operations.into_iter().zip(want.operations) {
        assert_eq!(got.operation, want.operation);
        assert_eq!(got.request, want.request);
        assert_eq!(got.lease, want.lease);
        assert_eq!(got.losses, want.losses);
        assert_eq!(got.fate, want.fate);
        assert_eq!(got.size_key, want.size_key);
        assert_eq!(got.parcel, want.subject.as_ref().map(subject::encode));
        assert_eq!(got.cost_kept, got.lease.is_none());
        if let Some(lease) = got.recovered(WaiterId(1)).lease {
            assert_eq!(lease.cost, None);
            held += 1;
        }
    }
    assert_eq!(held, 1, "operation 8 holds its lease");
}
