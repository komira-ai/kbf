//! The runtimes one daemon runs leases through: [`Runtimes`], a set in which each lease
//! kind has at most one runtime. A Start goes to the runtime that serves its kind; the
//! node report lists every runtime's driver under `drivers`.
//!
//! [`Runtime`] returns `impl Future`, so runtimes of different types cannot share one
//! list as they are. The set holds each behind [`AnyRuntime`], which boxes the futures
//! `run` and `kill` return: one allocation per lease and per kill.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{InputCacheCounts, ResourceUsage};
use kbf_types::{LeaseId, LeaseKind};

use crate::runtime::{NodeLoadError, NodeReading, Runtime, RuntimeError, Work};

/// A future a runtime behind [`AnyRuntime`] returns.
type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`Runtime`] with its futures boxed, so that runtimes of different types share a set.
pub(crate) trait AnyRuntime: Send + Sync {
    fn driver(&self) -> &'static str;
    fn serves(&self, kind: &str) -> bool;
    fn run(&self, work: Work) -> Boxed<'_, Result<ActionResult, RuntimeError>>;
    fn failed_usage(&self, lease_id: LeaseId) -> Option<ResourceUsage>;
    fn node_load(&self) -> Boxed<'_, Result<Option<NodeReading>, NodeLoadError>>;
    fn input_cache(&self) -> Option<InputCacheCounts>;
    fn welcomed_placement_load(&self, on: bool) -> Boxed<'_, ()>;
    fn take_peak_window_millicores(&self, lease_id: LeaseId) -> Option<u64>;
    fn kill(&self, lease_id: LeaseId) -> Boxed<'_, ()>;
}

impl<R: Runtime> AnyRuntime for R {
    fn driver(&self) -> &'static str {
        Runtime::driver(self)
    }

    fn serves(&self, kind: &str) -> bool {
        Runtime::serves(self, kind)
    }

    fn run(&self, work: Work) -> Boxed<'_, Result<ActionResult, RuntimeError>> {
        Box::pin(Runtime::run(self, work))
    }

    fn failed_usage(&self, lease_id: LeaseId) -> Option<ResourceUsage> {
        Runtime::failed_usage(self, lease_id)
    }

    fn node_load(&self) -> Boxed<'_, Result<Option<NodeReading>, NodeLoadError>> {
        Box::pin(Runtime::node_load(self))
    }

    fn input_cache(&self) -> Option<InputCacheCounts> {
        Runtime::input_cache(self)
    }

    fn welcomed_placement_load(&self, on: bool) -> Boxed<'_, ()> {
        Box::pin(Runtime::welcomed_placement_load(self, on))
    }

    fn take_peak_window_millicores(&self, lease_id: LeaseId) -> Option<u64> {
        Runtime::take_peak_window_millicores(self, lease_id)
    }

    fn kill(&self, lease_id: LeaseId) -> Boxed<'_, ()> {
        Box::pin(Runtime::kill(self, lease_id))
    }
}

/// The runtimes of one daemon, in the order they were added. Never empty, and no two
/// serve the same lease kind of [`LeaseKind::ALL`]: [`Runtimes::and`] refuses the second.
pub struct Runtimes {
    all: Vec<Arc<dyn AnyRuntime>>,
}

/// Two runtimes of one daemon serve the same lease kind, so a Start of that kind could
/// go to either.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "drivers {first:?} and {second:?} both serve lease kind {kind}: each kind must have one \
     driver on a node"
)]
pub struct KindServedTwice {
    pub kind: LeaseKind,
    /// The driver already in the set.
    pub first: &'static str,
    /// The driver refused.
    pub second: &'static str,
}

impl Runtimes {
    /// The set of `first` alone.
    #[must_use]
    pub fn new<R: Runtime>(first: Arc<R>) -> Self {
        Self { all: vec![first] }
    }

    /// This set with `next` added after the others.
    ///
    /// # Errors
    /// A kind of [`LeaseKind::ALL`] that `next` serves is served by a runtime already in
    /// the set.
    pub fn and<R: Runtime>(mut self, next: Arc<R>) -> Result<Self, KindServedTwice> {
        for kind in LeaseKind::ALL {
            if !Runtime::serves(&*next, kind.name()) {
                continue;
            }
            if let Some(first) = self.serving(kind.name()) {
                return Err(KindServedTwice {
                    kind,
                    first: first.driver(),
                    second: Runtime::driver(&*next),
                });
            }
        }
        self.all.push(next);
        Ok(self)
    }

    /// Every runtime's driver, in the order the runtimes were added, each once.
    #[must_use]
    pub fn drivers(&self) -> Vec<&'static str> {
        let mut drivers: Vec<&'static str> = Vec::new();
        for driver in self.all.iter().map(|r| r.driver()) {
            if !drivers.contains(&driver) {
                drivers.push(driver);
            }
        }
        drivers
    }

    /// The first reading any runtime gives, in the order they were added; `Ok(None)` when
    /// none takes readings. A runtime that fails ends the search with its error.
    ///
    /// Only that runtime's leases are in the reading. A lease of a later runtime that also
    /// takes readings is not named in it. The readings are not merged here: the container
    /// driver is the only runtime that reads leases today.
    pub(crate) async fn node_load(&self) -> Result<Option<NodeReading>, NodeLoadError> {
        for runtime in &self.all {
            if let Some(reading) = runtime.node_load().await? {
                return Ok(Some(reading));
            }
        }
        Ok(None)
    }

    /// The first input cache counts any runtime gives, in the order they were added; `None`
    /// when no runtime has an input cache.
    pub(crate) fn input_cache(&self) -> Option<InputCacheCounts> {
        self.all.iter().find_map(|runtime| runtime.input_cache())
    }

    /// Tells every runtime, in the order they were added, what a Welcome said of placement
    /// by measured load.
    pub(crate) async fn welcomed_placement_load(&self, on: bool) {
        for runtime in &self.all {
            runtime.welcomed_placement_load(on).await;
        }
    }

    /// Every runtime, in the order they were added.
    pub(crate) fn all(&self) -> &[Arc<dyn AnyRuntime>] {
        &self.all
    }

    /// The runtime that serves lease kind `kind`, if any; for a kind outside
    /// [`LeaseKind::ALL`], which two may serve, the first added.
    pub(crate) fn serving(&self, kind: &str) -> Option<&Arc<dyn AnyRuntime>> {
        self.all.iter().find(|r| r.serves(kind))
    }
}

impl<R: Runtime> From<Arc<R>> for Runtimes {
    fn from(runtime: Arc<R>) -> Self {
        Self::new(runtime)
    }
}

impl std::fmt::Debug for Runtimes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Runtimes").field(&self.drivers()).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use kbf_proto::worker::NodeLoad;

    use super::*;
    use crate::FakeRuntime;
    use crate::runtime::LeaseReading;

    fn fake(kind: LeaseKind, driver: &'static str) -> Arc<FakeRuntime> {
        Arc::new(FakeRuntime::new(Duration::ZERO).serving(kind, driver))
    }

    /// Catches: a set that takes a second runtime for a kind one already serves (a
    /// Start of that kind would go to whichever comes first, silently), and an error
    /// that names the wrong kind or drivers.
    #[test]
    fn a_kind_served_twice_is_refused() {
        let set = Runtimes::new(fake(LeaseKind::Action, "a"));
        let err = set
            .and(fake(LeaseKind::Action, "b"))
            .expect_err("two runtimes serve action");
        assert_eq!(
            err,
            KindServedTwice {
                kind: LeaseKind::Action,
                first: "a",
                second: "b",
            }
        );
        let text = err.to_string();
        assert!(text.contains("\"a\" and \"b\""), "{text}");
        assert!(text.contains("lease kind action:"), "{text}");
    }

    /// Catches: a clash check that looks at only some kinds of [`LeaseKind::ALL`] (only
    /// `action`, or the first few), which would take two `whole_machine` or `vm` runtimes
    /// and send every Start of that kind to the first; and an error that names the
    /// set's first runtime rather than the one that serves the kind.
    #[test]
    fn every_kind_served_twice_is_refused() {
        for kind in LeaseKind::ALL {
            let other = LeaseKind::ALL
                .into_iter()
                .find(|k| *k != kind)
                .expect("ALL has more than one kind");
            let err = Runtimes::new(fake(other, "a"))
                .and(fake(kind, "b"))
                .expect("distinct kinds")
                .and(fake(kind, "c"))
                .expect_err("two runtimes serve one kind");
            assert_eq!(
                err,
                KindServedTwice {
                    kind,
                    first: "b",
                    second: "c",
                },
                "kind {}",
                kind.name()
            );
        }
    }

    /// Catches: a lookup that answers the first runtime whatever the kind, and drivers
    /// that list only the first runtime's, or one driver twice.
    #[test]
    fn each_kind_finds_its_runtime() {
        let set = Runtimes::new(fake(LeaseKind::Action, "a"))
            .and(fake(LeaseKind::WholeMachine, "b"))
            .expect("distinct kinds")
            .and(fake(LeaseKind::Vm, "a"))
            .expect("distinct kinds");
        let driver = |kind: LeaseKind| set.serving(kind.name()).map(|r| r.driver());
        assert_eq!(driver(LeaseKind::Action), Some("a"));
        assert_eq!(driver(LeaseKind::WholeMachine), Some("b"));
        assert_eq!(driver(LeaseKind::Vm), Some("a"));
        assert!(set.serving("gpu").is_none(), "a kind no runtime serves");
        assert_eq!(set.drivers(), ["a", "b"]);
        assert_eq!(format!("{set:?}"), r#"Runtimes(["a", "b"])"#);
    }

    /// A runtime that serves `kind` and answers every `node_load` with `answer`, counting
    /// the readings it was asked for.
    struct Reads {
        kind: LeaseKind,
        answer: Result<Option<NodeReading>, NodeLoadError>,
        asked: AtomicUsize,
    }

    impl Reads {
        fn new(kind: LeaseKind, answer: Result<Option<NodeReading>, NodeLoadError>) -> Arc<Self> {
            Arc::new(Self {
                kind,
                answer,
                asked: AtomicUsize::new(0),
            })
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    impl Runtime for Reads {
        fn driver(&self) -> &'static str {
            "reads"
        }

        fn serves(&self, kind: &str) -> bool {
            kind == self.kind.name()
        }

        async fn run(&self, _work: Work) -> Result<ActionResult, RuntimeError> {
            Ok(ActionResult::default())
        }

        async fn node_load(&self) -> Result<Option<NodeReading>, NodeLoadError> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer.clone()
        }

        async fn kill(&self, _lease_id: LeaseId) {}
    }

    /// A reading that names the lease `seq` of term 1, started at `started_usec`.
    fn reading(seq: u64, started_usec: u64) -> NodeReading {
        NodeReading {
            load: NodeLoad {
                seq,
                ..NodeLoad::default()
            },
            leases: vec![LeaseReading {
                lease: LeaseId { term: 1, seq },
                started_usec,
                usage: None,
            }],
        }
    }

    /// Asserts that the node's reading is the first runtime's whole reading: the leases of
    /// a second runtime that also takes readings are not added or swapped in, and the
    /// second is not asked once the first has answered (a reading or an error). A first
    /// runtime that takes no reading leaves the second's to be the node's, and a set where
    /// none takes one has none.
    #[tokio::test]
    async fn node_load_is_the_first_readers() {
        let answers = |first, second| async move {
            let (a, b) = (
                Reads::new(LeaseKind::Action, first),
                Reads::new(LeaseKind::WholeMachine, second),
            );
            let set = Runtimes::new(Arc::clone(&a))
                .and(Arc::clone(&b))
                .expect("distinct kinds");
            (set.node_load().await, a.asked(), b.asked())
        };
        let error = NodeLoadError {
            kind: "cpu.stat",
            why: "unreadable".to_owned(),
        };

        let (got, first, second) =
            answers(Ok(Some(reading(1, 10))), Ok(Some(reading(2, 20)))).await;
        assert_eq!(got, Ok(Some(reading(1, 10))));
        assert_eq!((first, second), (1, 0));

        let (got, first, second) = answers(Err(error.clone()), Ok(Some(reading(2, 20)))).await;
        assert_eq!(got, Err(error));
        assert_eq!((first, second), (1, 0));

        let (got, first, second) = answers(Ok(None), Ok(Some(reading(2, 20)))).await;
        assert_eq!(got, Ok(Some(reading(2, 20))));
        assert_eq!((first, second), (1, 1));

        let (got, first, second) = answers(Ok(None), Ok(None)).await;
        assert_eq!(got, Ok(None));
        assert_eq!((first, second), (1, 1));
    }

    /// Asserts that a bare node load becomes a reading that names no leases.
    #[test]
    fn a_node_load_alone_is_a_reading_with_no_leases() {
        let load = NodeLoad {
            seq: 4,
            ..NodeLoad::default()
        };
        let reading = NodeReading::from(load.clone());
        assert_eq!(reading.load, load);
        assert!(reading.leases.is_empty());
    }
}
