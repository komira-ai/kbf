//! A writer whose state store has a newer generation: its index writes are refused.

use std::sync::Arc;

use kbf_history::records::RunOutcome;
use kbf_history::{InvRef, Limits};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Fenced, ManualClock, Store};

use crate::support::{Index, Life, Objects, T};

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

/// Asserts that a writer whose store was taken over by a newer generation drops its index
/// rows as a failed write does: no row lands, the refusal is counted under the index stage,
/// and the rows are not sent again in another form.
#[test]
fn a_superseded_writer_drops_its_rows() {
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
    assert!(fence.keys("ui/").is_empty());
    assert!(life.writer.drops().index > 0);
}
