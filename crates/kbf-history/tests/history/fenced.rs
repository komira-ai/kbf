//! A writer whose state store has a newer generation: the rows of its own that it may
//! still write, and the shared rows that only the holder writes.

use std::sync::Arc;

use kbf_history::records::RunOutcome;
use kbf_history::{InvRef, Limits};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Fenced, ManualClock, Store};

use crate::rows_support::test_run;
use crate::support::{Index, Life, Objects, T, all, whole};

/// A store with a holder of generation 1 on it, and a way to supersede it.
struct Fence {
    base: Arc<dyn Store>,
    index: Arc<Index>,
}

impl Fence {
    fn new() -> Fence {
        let base: Arc<dyn Store> = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        let holder = Fenced::take(Arc::clone(&base), None).expect("generation 1");
        Fence {
            base,
            index: Index::over(Arc::new(holder)),
        }
    }

    fn supersede(&self) {
        Fenced::take(Arc::clone(&self.base), Some(1)).expect("generation 2");
    }

    fn keys(&self, prefix: &str) -> Vec<String> {
        (self.base.scan_prefix(prefix).expect("scan").into_iter())
            .map(|(key, _)| key)
            .collect()
    }
}

// Catches: a superseded writer that loses the run of a call it still serves. Under the
// generation holder it sends one transaction per batch as it always did (the first
// build's rows, shared ones included, are one transaction). Once another process holds
// the generation, the batch is refused whole, then sent as the rows of its own writer
// (the chunk's row, its stream row, its header and its list entry) and the shared rows
// (the shape row), and only the first lands: the build is readable by the next process,
// its rows being the writer's own, and no shared row is written behind the fence. The
// refusal of the shared row is counted as a dropped index row, as before.
#[test]
fn a_superseded_writer_writes_its_own_rows_and_not_the_shared_ones() {
    let fence = Fence::new();
    let objects = Objects::new();
    let mut life = Life::start_exempting(&objects, &fence.index, 7, Limits::default());
    life.step(T); // reserve a stream block
    let before = fence.base.commit_stats().txns;
    let first = InvRef::named("first");
    test_run(&life, &first, T, RunOutcome::Failed, 1, 1);
    life.close(&first);
    life.drain(T + 1, 20);
    let sent = fence.base.commit_stats().txns - before;
    assert_eq!(sent, 1, "the chunk's rows and the shape row: no split");
    assert_eq!(fence.keys("ui/shape/").len(), 1);

    fence.supersede();
    let second = InvRef::named("second");
    test_run(&life, &second, T + 2, RunOutcome::Failed, 2, 2);
    life.close(&second);
    // One step is enough: the whole batch's refusal wrote nothing and spends none of the
    // step's one transaction, so the step goes on to send the rows that may land (the
    // last step before a stop has no next one).
    life.step(T + 3);
    for prefix in [
        format!("ui/ichunk/{}/", second.key),
        format!("ui/invid/{}/", second.key),
    ] {
        assert_eq!(fence.keys(&prefix).len(), 1, "{prefix}");
    }
    assert_eq!(fence.keys("ui/inv/main/").len(), 2, "its list entry landed");
    for shared in ["ui/shape/", "ui/status/", "ui/statusx/"] {
        let rows = fence.keys(shared);
        assert!(
            rows.len() <= 1,
            "a shared row was written behind the fence: {rows:?}"
        );
    }
    assert!(
        life.writer.pending_rows() > 0,
        "the shared rows wait for a step"
    );
    life.step(T + 4);
    assert!(life.writer.drops().index > 0, "its refusal is counted");
    let reader = life.reader();
    let got = whole(&reader, &second.key, &all).unwrap().unwrap();
    assert_eq!(got.runs.len(), 1, "the build is readable");
}

// Catches: a split that is made for a writer with nothing to send around the fence, or
// one that loops: a writer whose index has no exempt handle (a store with no generation
// of its own to skip) that is refused as fenced drops its rows as a failed write does,
// counted, and does not send them again in a second form.
#[test]
fn a_superseded_writer_with_nothing_exempt_drops_its_rows() {
    let fence = Fence::new();
    let objects = Objects::new();
    let mut life = Life::start(&objects, &fence.index, 7, Limits::default());
    fence.supersede();
    let inv = InvRef::named("lost");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    for _ in 0..4 {
        life.step(T + 1);
    }
    assert!(fence.keys("ui/ichunk/").is_empty());
    assert!(life.writer.drops().index > 0);
}
