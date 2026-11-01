//! `--learned-sizes`: which submissions are keyed, read back from the `op/` rows the
//! farm writes, and what a restart makes of a keyed row.

use std::sync::Arc;

use kbf_front::{Described, Dispatch, Submission};
use kbf_invocation::{BookSource, Evidence};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources};

use super::{EpochPolicy, Farm};
use crate::LearnedSizes;
use crate::state_store::StateStore;

fn store() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn farm_over(store: &Arc<Fake>, mode: LearnedSizes) -> Farm {
    over(store.clone(), mode)
}

fn over(store: Arc<dyn Store>, mode: LearnedSizes) -> Farm {
    over_with(store, mode, None)
}

/// [`over`], keeping at most `floors` memory floors.
fn over_with(store: Arc<dyn Store>, mode: LearnedSizes, floors: Option<usize>) -> Farm {
    let farm = Farm::new_legacy(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_learned_sizes(mode, 4 << 30);
    let farm = match floors {
        Some(bound) => farm.with_memory_floors(bound),
        None => farm,
    };
    farm.with_state(Some(StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a farm over the store")
}

/// What a submission is like. The defaults are the keyed case: an action of unnamed
/// memory on Linux, instance `main`, program `cc`.
#[derive(Clone)]
struct Job {
    tag: u8,
    instance: &'static str,
    platform: Vec<(&'static str, &'static str)>,
    program: &'static str,
    bare: bool,
    kind: LeaseKind,
    gpus: u64,
    memory: BookSource,
}

impl Job {
    fn new(tag: u8) -> Self {
        Self {
            tag,
            instance: "main",
            platform: vec![("OSFamily", "linux")],
            program: "cc",
            bare: false,
            kind: LeaseKind::Action,
            gpus: 0,
            memory: BookSource::Default,
        }
    }

    fn submit(&self, farm: &Farm) {
        let request = kbf_sched::Request {
            key: ActionKey {
                instance: self.instance.to_owned(),
                action: Digest::new(DigestFunction::Sha256, [self.tag; 32], 1),
            }
            .into(),
            qos: Qos::Ci,
            kind: self.kind,
            resources: Resources::new(1_000, 4 << 30).with_gpus(self.gpus),
            hermetic: true,
            do_not_cache: false,
            needs: kbf_caps::Request::from_platform(self.platform.iter().copied()).unwrap(),
            whole_worker: false,
        };
        farm.submit(Submission {
            request,
            call: None,
            described: Described {
                arguments: if self.bare {
                    Vec::new()
                } else {
                    vec![self.program.to_owned()]
                },
                ..Described::default()
            },
            evidence: Evidence {
                memory_source: self.memory,
                ..Evidence::default()
            },
            max_wall: None,
        })
        .expect("submitted");
    }
}

/// The size key of each operation of the store, in operation order: its `opkey/` row, or
/// `None` for an operation with none. The request rows are format 1 whatever the key.
fn keys(store: &Arc<Fake>) -> Vec<Option<u64>> {
    let opkeys: std::collections::BTreeMap<String, Vec<u8>> = store
        .scan_prefix("opkey/")
        .expect("scan")
        .into_iter()
        .map(|(k, row)| (k["opkey/".len()..].to_owned(), row.value))
        .collect();
    store
        .scan_prefix("op/")
        .expect("scan")
        .into_iter()
        .map(|(k, row)| {
            assert!(
                kbf_log::decode(kbf_log::Format::V1, &row.value).is_ok(),
                "a request row is format 1"
            );
            opkeys
                .get(&k["op/".len()..])
                .map(|v| u64::from_be_bytes(v[1..9].try_into().expect("an opkey row")))
        })
        .collect()
}

/// The size keys the scheduler holds its operations with.
fn size_keys(farm: &Farm) -> Vec<Option<u64>> {
    let state = farm.lock();
    state
        .sched
        .unfinished()
        .map(|op| state.sched.size_key(op))
        .collect()
}

fn keyed_with(jobs: &[Job]) -> Vec<Option<u64>> {
    let store = store();
    let farm = farm_over(&store, LearnedSizes::On);
    for job in jobs {
        job.submit(&farm);
    }
    keys(&store)
}

/// Only `on` keys a submission: a plain action of unnamed memory is keyed with it, and
/// with `off` or `shadow` no row is written and nothing is named.
#[test]
fn only_on_keys_a_submission() {
    for mode in [LearnedSizes::Off, LearnedSizes::Shadow] {
        let store = store();
        let farm = farm_over(&store, mode);
        Job::new(1).submit(&farm);
        assert_eq!(keys(&store), vec![None], "{mode:?}");
        assert_eq!(size_keys(&farm), vec![None], "{mode:?}");
    }
    let store = store();
    let farm = farm_over(&store, LearnedSizes::On);
    Job::new(1).submit(&farm);
    let [Some(key)] = keys(&store)[..] else {
        panic!("on keys a plain action");
    };
    assert_eq!(
        size_keys(&farm),
        vec![Some(key)],
        "the scheduler holds it too"
    );
}

/// Catches a key given to a submission the floor must not apply to: one whose memory the
/// client named (a hint equal to the default included: it is `Hint`, not `Default`),
/// whose platform names no OS, that is not an `action`, or
/// that books GPUs.
#[test]
fn only_an_unnamed_pinned_plain_action_is_keyed() {
    let named = Job {
        memory: BookSource::Hint,
        ..Job::new(1)
    };
    let unpinned = Job {
        platform: vec![],
        ..Job::new(3)
    };
    let machine = Job {
        kind: LeaseKind::WholeMachine,
        ..Job::new(4)
    };
    let gpu = Job {
        gpus: 1,
        ..Job::new(5)
    };
    assert_eq!(keyed_with(&[named, unpinned, machine, gpu]), vec![None; 4]);
}

/// Catches a key that is the shape alone: a tenant's kill would floor another tenant's
/// same-shaped action (on purpose, by sending the same argv), and a Linux kill would
/// floor a Mac's. Two instances, one shape; two platforms, one shape; and the same
/// instance and platform with another digest share one key.
#[test]
fn the_key_names_the_instance_the_platform_and_the_shape() {
    let other_instance = Job {
        instance: "other",
        ..Job::new(2)
    };
    let mac = Job {
        platform: vec![("OSFamily", "macos")],
        ..Job::new(3)
    };
    let arm = Job {
        platform: vec![("OSFamily", "linux"), ("arch", "arm64")],
        ..Job::new(4)
    };
    let other_shape = Job {
        program: "ld",
        ..Job::new(5)
    };
    let keys = keyed_with(&[
        Job::new(1),
        other_instance,
        mac,
        arm,
        other_shape,
        Job::new(6),
    ]);
    let keys: Vec<u64> = keys.into_iter().map(|k| k.expect("keyed")).collect();
    assert_eq!(keys[0], keys[5], "another digest of the same action");
    for i in 1..5 {
        assert_ne!(keys[0], keys[i], "key {i} equals the first");
    }
}

/// Catches a restart that keeps a size key it should not: `off` (and `shadow`) ignore
/// the key of a row an `on` run queued, so the operation books as before; `on` keeps it.
#[test]
fn a_restart_without_on_ignores_the_size_key_of_a_row() {
    let store = store();
    let first = farm_over(&store, LearnedSizes::On);
    Job::new(1).submit(&first);
    assert!(keys(&store)[0].is_some());
    drop(first);
    for mode in [LearnedSizes::Off, LearnedSizes::Shadow] {
        let farm = farm_over(&store, mode);
        let loaded = size_keys(&farm);
        assert_eq!(loaded, vec![None], "{mode:?}");
    }
    let farm = farm_over(&store, LearnedSizes::On);
    assert!(size_keys(&farm)[0].is_some());
}

/// Seeds a row no build reads (a version no one wrote): `off` and `shadow` neither load
/// nor fail on it, and `on` refuses the start rather than guess.
#[test]
fn a_bad_size_key_row_stops_a_start_only_when_on() {
    use kbf_state::{Cond, Txn};
    let store = store();
    drop(farm_over(&store, LearnedSizes::On));
    store
        .txn(
            Txn::new()
                .put("opkey/0000000000000001", vec![9], Cond::Any)
                .put("keyfloor/0000000000000001", vec![9], Cond::Any),
        )
        .expect("write")
        .expect("applied");
    for mode in [LearnedSizes::Off, LearnedSizes::Shadow] {
        drop(farm_over(&store, mode));
    }
    let on = Farm::new_legacy(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_learned_sizes(LearnedSizes::On, 4 << 30);
    let store: Arc<dyn Store> = store;
    let refused = on.with_state(Some(StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }));
    assert!(refused.is_err(), "on started over a bad row");
}

/// A worker `name` with room for a 16 GiB lease registered with `farm`.
fn register(farm: &Farm, name: &str) -> kbf_types::WorkerId {
    register_sized(farm, name, 64)
}

/// A worker `name` of `gib` GiB registered with `farm`.
fn register_sized(farm: &Farm, name: &str, gib: u64) -> kbf_types::WorkerId {
    let worker = kbf_types::WorkerId::new(name);
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = tokio::sync::mpsc::unbounded_channel();
    // The receiver lives as long as the test: a dropped one closes the stream.
    std::mem::forget(inbox);
    farm.register(
        &worker,
        kbf_sched::DaemonInstance::new(name),
        kbf_sched::DaemonInstance::new(""),
        crate::farm::phase::Reported::serving(kbf_types::Resources::new(4_000, gib << 30), caps),
        outbound,
        kbf_proto::worker::ServerMessage::default(),
    );
    worker
}

/// The whole path: a keyed action killed at its own limit through the farm's report
/// input writes a `keyfloor/` row with the requeue; a farm started over the same store
/// with `on` loads it, and a new digest of the same shape books the floor on its first
/// placement; started with `off` it loads none.
#[test]
fn a_kill_through_the_farm_leaves_a_floor_the_next_start_books() {
    use kbf_types::{Failure, Outcome};
    let store = store();
    let farm = farm_over(&store, LearnedSizes::On);
    let node = register(&farm, "node-a");
    Job::new(1).submit(&farm);
    let key = keys(&store)[0].expect("keyed");
    let (operation, lease) = {
        let state = farm.lock();
        let operation = state.sched.unfinished().next().expect("queued");
        (operation, state.sched.leases_on(&node)[0])
    };
    let report = super::report::Report {
        lease,
        outcome: Outcome::Failed(Failure::OutOfMemory),
        measured: None,
        exit_code: None,
        run: None,
    };
    let now = farm.now();
    farm.lock().take_report(now, &node, operation, report);
    let rows = store.scan_prefix("keyfloor/").expect("scan");
    assert_eq!(rows.len(), 1, "one key floor row");
    assert_eq!(farm.lock().sched.key_floor(key), Some(16 << 30));
    drop(farm);

    let restarted = farm_over(&store, LearnedSizes::On);
    assert_eq!(restarted.lock().sched.key_floor(key), Some(16 << 30));
    let other = register(&restarted, "node-b");
    Job::new(2).submit(&restarted);
    // Two leases on the new node, both at 16 GiB: the killed action's retry (the store
    // holds it queued) and the new digest, which started at the floor and not at 4 GiB.
    assert_eq!(
        restarted
            .lock()
            .sched
            .booked(&other)
            .map(|b| b.memory_bytes),
        Some(32 << 30),
        "a new digest of the shape starts at the floor"
    );
    drop(restarted);

    let off = farm_over(&store, LearnedSizes::Off);
    assert_eq!(off.lock().sched.key_floor(key), None);
}

/// The estimator's key for `Job::new(_)`, and the run of a quiet success booked at 16 GiB.
fn estimator_key_and_run() -> (kbf_estimator::Key, kbf_estimator::Run) {
    use crate::action_usage::Subject;
    use crate::learned::{key_of, run_of};
    use crate::machine::Platform;
    use kbf_estimator::{Basis, GIB};
    use kbf_front::Described;
    use kbf_types::Outcome;

    let (subject, _) = Subject::of(Described {
        arguments: vec!["cc".to_owned()],
        ..Described::default()
    });
    let platform = Platform {
        os_family: "linux",
        isa: "-".to_owned(),
    };
    let key = key_of("main", &platform, Basis::Cgroup, &subject).expect("a key");
    let ok = Outcome::Completed {
        action_result: Digest::new(DigestFunction::Sha256, [1; 32], 1).into(),
    };
    let usage = kbf_invocation::RunUsage {
        wall_micros: 2_000_000,
        basis: kbf_invocation::UsageBasis::Cgroup,
        memory_peak_bytes: Some(500 << 20),
        cpu_usage_micros: Some(500),
        ..Default::default()
    };
    let (_, run) = run_of(ok, Some(&usage), 16 * GIB).expect("a run");
    (key, run)
}

/// A farm in `on` mode over `store` whose one keyed action has been killed at its own
/// limit: its key floor is 16 GiB. Returns the farm, its worker, the size key, the
/// estimator's key and the run of a quiet success to show it.
struct Killed {
    farm: Farm,
    node: kbf_types::WorkerId,
    size_key: u64,
    key: kbf_estimator::Key,
    run: kbf_estimator::Run,
    seq: u64,
}

fn killed_farm(store: Arc<dyn Store>, rows: &Arc<Fake>) -> Killed {
    killed_farm_on(store, rows, &[("node-a", 64)], None)
}

/// [`killed_farm`] over workers of the given sizes (name, GiB): the first by name takes
/// the job, and the kill happens there.
fn killed_farm_on(
    store: Arc<dyn Store>,
    rows: &Arc<Fake>,
    nodes: &[(&str, u64)],
    floors: Option<usize>,
) -> Killed {
    use kbf_estimator::GIB;

    let farm = over_with(store, LearnedSizes::On, floors);
    let workers: Vec<_> = nodes
        .iter()
        .map(|(name, gib)| register_sized(&farm, name, *gib))
        .collect();
    let node = workers[0].clone();
    Job::new(1).submit(&farm);
    let size_key = keys(rows)[0].expect("keyed");
    let (operation, lease) = {
        let state = farm.lock();
        let operation = state.sched.unfinished().next().expect("queued");
        (operation, state.sched.leases_on(&node)[0])
    };
    let report = super::report::Report {
        lease,
        outcome: kbf_types::Outcome::Failed(kbf_types::Failure::OutOfMemory),
        measured: None,
        exit_code: None,
        run: None,
    };
    let now = farm.now();
    farm.lock().take_report(now, &node, operation, report);
    assert_eq!(farm.lock().sched.key_floor(size_key), Some(16 * GIB));

    let (key, run) = estimator_key_and_run();
    Killed {
        farm,
        node,
        size_key,
        key,
        run,
        seq: 1_000,
    }
}

impl Killed {
    /// 257 quiet runs of the key, shown at farm time `days` days in.
    fn quiet(&mut self, days: u64) {
        self.quiet_runs(days, 257);
    }

    /// `runs` quiet runs of the key, shown at farm time `days` days in.
    fn quiet_runs(&mut self, days: u64, runs: u64) {
        use crate::learned::Attempt;
        use kbf_types::{FarmTime, LeaseId};
        let mut state = self.farm.lock();
        state
            .side
            .learned
            .clock(FarmTime::from_millis(days * 86_400_000));
        for _ in 0..runs {
            self.seq += 1;
            let lease = LeaseId::new(9, self.seq);
            let attempt = Attempt {
                key: self.key.clone(),
                run: self.run,
            };
            state.side.learned.stage(lease, Some(attempt));
            state.finish_learning(kbf_types::OperationId(u64::MAX), lease, &self.node, true);
        }
    }
}

/// The ask the one `keyfloor/` row holds.
fn row_ask(store: &Arc<Fake>) -> Option<u64> {
    let rows = store.scan_prefix("keyfloor/").expect("scan");
    rows.first()
        .map(|(_, row)| u64::from_be_bytes(row.value[9..17].try_into().expect("ask")))
}

/// The estimator's decay of a key floor reaches the scheduler and the store: after 256
/// runs of the key and 7 days quiet the floor halves in the scheduler and its `keyfloor/`
/// row, a restart books the lower floor, and a floor that falls to the first rung or below
/// is forgotten (row deleted) so a new digest books the default.
#[test]
fn a_decayed_key_floor_is_lowered_in_the_scheduler_and_the_store() {
    use kbf_estimator::GIB;
    let store = store();
    let mut killed = killed_farm(store.clone(), &store);
    let size_key = killed.size_key;
    killed.quiet(8);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(8 * GIB));
    assert_eq!(row_ask(&store), Some(8 * GIB), "the row is lowered too");
    // The summaries are written behind; a restart takes up what was written.
    killed.farm.lock().flush_learned();
    drop(killed);

    let restarted = farm_over(&store, LearnedSizes::On);
    assert_eq!(restarted.lock().sched.key_floor(size_key), Some(8 * GIB));
    let other = register(&restarted, "node-b");
    Job::new(2).submit(&restarted);
    // The killed action's retry (queued in the store, at the rung it was raised to) and
    // the new digest, at the lower floor.
    assert_eq!(
        restarted
            .lock()
            .sched
            .booked(&other)
            .map(|b| b.memory_bytes),
        Some((16 + 8) << 30),
    );
    let (key, run) = estimator_key_and_run();
    let mut again = Killed {
        farm: restarted,
        node: other,
        size_key,
        key,
        run,
        seq: 2_000,
    };
    again.quiet(16);
    assert_eq!(
        again.farm.lock().sched.key_floor(size_key),
        None,
        "a floor at the first rung is forgotten"
    );
    assert_eq!(row_ask(&store), None, "and its row is deleted");
}

/// A store over the fake that refuses the writes of `keyfloor/` rows while told to.
struct Refusing {
    inner: Arc<Fake>,
    refuse: std::sync::atomic::AtomicBool,
    /// The operation (its id) whose requeue is refused, whatever `refuse` says; none for
    /// `u64::MAX`.
    stuck: std::sync::atomic::AtomicU64,
}

impl Refusing {
    fn new(inner: &Arc<Fake>) -> Arc<Self> {
        Arc::new(Self {
            inner: inner.clone(),
            refuse: std::sync::atomic::AtomicBool::new(false),
            stuck: std::sync::atomic::AtomicU64::new(u64::MAX),
        })
    }

    /// Refuses the requeues of operation `operation` (they write its `fate/` row), until [`Self::release`].
    fn stick(&self, operation: u64) {
        self.stuck
            .store(operation, std::sync::atomic::Ordering::SeqCst);
    }

    fn release(&self) {
        self.stuck
            .store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Store for Refusing {
    fn apply_batch(
        &self,
        batch: Vec<kbf_state::Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, kbf_state::Conflict>>, kbf_state::StoreError> {
        let rows = batch.iter().flat_map(kbf_state::Txn::ops).any(|op| {
            op.key_cond()
                .is_some_and(|(key, _)| key.starts_with("keyfloor/"))
        });
        let stuck = self.stuck.load(std::sync::atomic::Ordering::SeqCst);
        let queues = |op: &kbf_state::Op| {
            op.key_cond()
                .is_some_and(|(key, _)| key == format!("fate/{stuck:016x}"))
        };
        if stuck != u64::MAX && batch.iter().flat_map(kbf_state::Txn::ops).any(queues) {
            return Err(kbf_state::StoreError::Failed("refused".to_owned()));
        }
        if rows && self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(kbf_state::StoreError::Failed("refused".to_owned()));
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<kbf_state::Versioned>, kbf_state::StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, kbf_state::Versioned)>, kbf_state::StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(
        &self,
        scan: kbf_state::ScanRange<'_>,
    ) -> Result<kbf_state::ScanPage, kbf_state::StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(
        &self,
        after: kbf_state::Revision,
        limit: usize,
    ) -> Result<kbf_state::Changes, kbf_state::StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, kbf_state::StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> kbf_state::CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> kbf_state::StoreTime {
        self.inner.now()
    }
}

/// A row the store refuses is not lost: the scheduler floor is lowered at once, the farm
/// does not fail, and a later tick writes the row.
#[test]
fn a_decay_row_the_store_refuses_is_written_at_a_later_tick() {
    use kbf_estimator::GIB;
    let rows = store();
    let refusing = Refusing::new(&rows);
    let mut killed = killed_farm(refusing.clone(), &rows);
    let size_key = killed.size_key;
    refusing
        .refuse
        .store(true, std::sync::atomic::Ordering::SeqCst);
    killed.quiet(8);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(8 * GIB));
    assert_eq!(row_ask(&rows), Some(16 * GIB), "the store kept the old row");
    refusing
        .refuse
        .store(false, std::sync::atomic::Ordering::SeqCst);
    killed.farm.lock().flush_learned();
    assert_eq!(row_ask(&rows), Some(8 * GIB), "written at the next tick");
}

/// The write-behind of the size summaries, asked for before the interval has passed,
/// writes the changed summaries to the store at once; a tick before that writes none, and
/// a second ask with nothing changed writes nothing.
#[test]
fn write_behind_now_writes_the_changed_summaries_at_once() {
    let rows = store();
    let mut killed = killed_farm(rows.clone(), &rows);
    let held = |rows: &Arc<Fake>| -> Vec<(String, Vec<u8>)> {
        rows.scan_prefix("est/")
            .expect("scan")
            .into_iter()
            .map(|(k, row)| (k, row.value))
            .collect()
    };
    killed.quiet_runs(0, 1);
    let before = held(&rows);
    killed.farm.lock().flush_learned();
    assert_eq!(held(&rows), before, "the interval has not passed");
    killed.farm.write_behind_now();
    let written = held(&rows);
    assert_ne!(written, before, "the changed summary is written");
    killed.farm.write_behind_now();
    assert_eq!(held(&rows), written, "nothing changed since");
}

/// With a newer writer holding the store nothing is written, but the scheduler's floor is
/// lowered all the same.
#[test]
fn a_fenced_farm_lowers_the_floor_and_writes_no_row() {
    use kbf_estimator::GIB;
    let rows = store();
    let mut killed = killed_farm(rows.clone(), &rows);
    let size_key = killed.size_key;
    killed.farm.lock().fenced = true;
    killed.quiet(8);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(8 * GIB));
    assert_eq!(row_ask(&rows), Some(16 * GIB));
    killed.farm.lock().flush_learned();
    assert_eq!(
        row_ask(&rows),
        Some(16 * GIB),
        "a later tick writes nothing either"
    );
}

/// A key the scheduler holds no floor for (another keyed operation under the same
/// estimator key, never killed) has nothing lowered and nothing written.
#[test]
fn a_decay_for_a_key_without_a_floor_writes_nothing() {
    let rows = store();
    let mut killed = killed_farm(rows.clone(), &rows);
    killed.farm.lock().side.learned.register(&killed.key, 999);
    killed.quiet(8);
    assert_eq!(killed.farm.lock().sched.key_floor(999), None);
    let names: Vec<String> = rows
        .scan_prefix("keyfloor/")
        .expect("scan")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        vec![format!("keyfloor/{:016x}", killed.size_key)],
        "the one row is the killed key's"
    );
}

/// A floor the scheduler raised after the estimator last saw the key (a kill it never
/// saw) is not lowered to the estimator's older value.
#[test]
fn a_decay_does_not_lower_a_floor_a_newer_kill_raised() {
    use kbf_estimator::GIB;
    let rows = store();
    let mut killed = killed_farm(rows.clone(), &rows);
    let size_key = killed.size_key;
    killed
        .farm
        .lock()
        .sched
        .restore_key_floors(vec![kbf_sched::KeyFloor {
            key: size_key,
            ask: 32 * GIB,
            stamp: 99,
        }]);
    killed.quiet(8);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(32 * GIB));
    assert_eq!(row_ask(&rows), Some(16 * GIB));
}

/// What a quiet cgroup-measured success of `peak` bytes carried, as the farm reads it.
fn measured(peak: u64) -> crate::action_usage::Measured {
    use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
    use kbf_proto::worker::{CgroupUsage, ResourceUsage, UsageBasis};
    let usage = ResourceUsage {
        wall_micros: 2_000_000,
        cgroup: Some(CgroupUsage {
            memory_peak_bytes: Some(peak),
            cpu_usage_micros: Some(500),
            ..CgroupUsage::default()
        }),
        basis: UsageBasis::Cgroup.into(),
        ..ResourceUsage::default()
    };
    let result = kbf_proto::worker::Result {
        action_result: Some(ActionResult {
            execution_metadata: Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![usage.to_any()],
                ..ExecutedActionMetadata::default()
            }),
            ..ActionResult::default()
        }),
        ..kbf_proto::worker::Result::default()
    };
    crate::action_usage::Measured::of(&result).expect("a usage")
}

/// The farm takes the success of `lease` (operation `operation`) measured at `peak`.
fn succeed(
    farm: &Farm,
    node: &kbf_types::WorkerId,
    operation: kbf_types::OperationId,
    lease: kbf_types::LeaseId,
    peak: u64,
) {
    let report = super::report::Report {
        lease,
        outcome: kbf_types::Outcome::Completed {
            action_result: Digest::new(DigestFunction::Sha256, [1; 32], 1).into(),
        },
        measured: Some(measured(peak)),
        exit_code: Some(0),
        run: None,
    };
    let now = farm.now();
    farm.lock().take_report(now, node, operation, report);
}

/// The operation and lease of the newest operation the node holds.
fn newest(farm: &Farm, node: &kbf_types::WorkerId) -> (kbf_types::OperationId, kbf_types::LeaseId) {
    let mut state = farm.lock();
    // A requeued operation is placed at the next tick.
    state.feed_quiet(farm.now(), kbf_sched::Event::Tick);
    let operation = state.sched.unfinished().max().expect("an operation");
    let lease = state
        .sched
        .leases_on(node)
        .into_iter()
        .find(|lease| {
            matches!(
                state.sched.state(operation),
                Some(kbf_sched::OpState::Leased { lease: held, .. }) if held == lease
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "its lease: {operation:?} {:?} {:?}",
                state.sched.state(operation),
                state.sched.leases_on(node)
            )
        });
    (operation, lease)
}

/// The kill-raised floor and the estimator's floor agree when the kill happens on a
/// worker smaller than the largest one that could run the action, so the decay is not
/// held back by a scheduler floor the estimator's older, lower value cannot match.
#[test]
fn a_floor_killed_on_a_smaller_worker_decays_like_any_other() {
    use kbf_estimator::GIB;
    let rows = store();
    let mut killed = killed_farm_on(rows.clone(), &rows, &[("a-small", 16), ("b-big", 64)], None);
    let size_key = killed.size_key;
    let summary = killed
        .farm
        .lock()
        .side
        .learned
        .summary_key(size_key)
        .expect("named");
    let estimator_floor = |killed: &Killed| {
        let state = killed.farm.lock();
        state
            .side
            .learned
            .summaries()
            .get(&summary)
            .map(kbf_estimator::Summary::oom_floor)
    };
    assert_eq!(
        estimator_floor(&killed),
        Some(16 * GIB),
        "not clamped to 16 GiB less 256 MiB"
    );
    killed.quiet(8);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(8 * GIB));
    assert_eq!(row_ask(&rows), Some(8 * GIB));
}

/// A lowered row waiting for the store is dropped when a kill raises the floor again: the
/// requeue writes the higher floor, and the late lower row must not overwrite it. What is
/// pending is lost at a restart, which leaves the higher row: the safe side.
#[test]
fn a_pending_lowered_row_does_not_overwrite_a_floor_raised_since() {
    use kbf_estimator::GIB;
    let rows = store();
    let refusing = Refusing::new(&rows);
    let mut killed = killed_farm(refusing.clone(), &rows);
    let size_key = killed.size_key;
    refusing
        .refuse
        .store(true, std::sync::atomic::Ordering::SeqCst);
    killed.quiet(8);
    assert_eq!(killed.farm.lock().unwritten_floors.len(), 1, "pending");

    // A new digest books the lowered floor and is killed while the store still refuses:
    // the floor rises to 16 GiB, and the requeue's row waits for the store too.
    Job::new(2).submit(&killed.farm);
    let (operation, lease) = newest(&killed.farm, &killed.node);
    let report = super::report::Report {
        lease,
        outcome: kbf_types::Outcome::Failed(kbf_types::Failure::OutOfMemory),
        measured: None,
        exit_code: None,
        run: None,
    };
    let now = killed.farm.now();
    killed
        .farm
        .lock()
        .take_report(now, &killed.node, operation, report);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(16 * GIB));
    assert!(killed.farm.lock().unwritten_floors.is_empty(), "dropped");
    refusing
        .refuse
        .store(false, std::sync::atomic::Ordering::SeqCst);
    // The worst order: the requeue's higher row first, then the tick that would write a
    // stale lowered one.
    killed.farm.lock().flush_step();
    assert_eq!(row_ask(&rows), Some(16 * GIB));
    killed.farm.lock().flush_learned();
    assert_eq!(row_ask(&rows), Some(16 * GIB), "a later tick leaves it");
}

/// The row names the summary its floor decays with, so after a restart in which every
/// operation of the key has finished, attempts of the key (any, hinted ones included)
/// still lower the floor.
#[test]
fn a_floor_whose_operations_have_all_finished_still_decays_after_a_restart() {
    use kbf_estimator::GIB;
    let rows = store();
    let killed = killed_farm(rows.clone(), &rows);
    let size_key = killed.size_key;
    // The killed action's retry runs to the end: no operation is left in the store.
    let (operation, lease) = newest(&killed.farm, &killed.node);
    succeed(&killed.farm, &killed.node, operation, lease, 500 << 20);
    killed.farm.lock().flush_learned();
    let (node, key, run) = (killed.node.clone(), killed.key.clone(), killed.run);
    drop(killed);
    assert!(rows.scan_prefix("op/").expect("scan").is_empty());
    assert!(rows.scan_prefix("opkey/").expect("scan").is_empty());

    let restarted = farm_over(&rows, LearnedSizes::On);
    assert_eq!(restarted.lock().side.learned.named(&key), vec![size_key]);
    let mut again = Killed {
        farm: restarted,
        node,
        size_key,
        key,
        run,
        seq: 3_000,
    };
    again.quiet(8);
    assert_eq!(again.farm.lock().sched.key_floor(size_key), Some(8 * GIB));
    assert_eq!(row_ask(&rows), Some(8 * GIB));
}

/// A keyed operation still in the store registers its estimator key at restart, with no
/// floor row to name it.
#[test]
fn a_restart_registers_the_keys_of_the_operations_it_restores() {
    let rows = store();
    let farm = farm_over(&rows, LearnedSizes::On);
    Job::new(1).submit(&farm);
    let size_key = keys(&rows)[0].expect("keyed");
    drop(farm);
    assert!(rows.scan_prefix("keyfloor/").expect("scan").is_empty());
    let restarted = farm_over(&rows, LearnedSizes::On);
    let (key, _) = estimator_key_and_run();
    assert_eq!(restarted.lock().side.learned.named(&key), vec![size_key]);
    // Without `on` nothing is registered.
    drop(restarted);
    let off = farm_over(&rows, LearnedSizes::Shadow);
    assert!(off.lock().side.learned.named(&key).is_empty());
}

/// An attempt the client sized (never keyed) is the one that crosses the threshold, as the
/// farm runs it end to end: the keyed floor of the same shape is lowered.
#[test]
fn a_client_sized_attempt_that_crosses_the_threshold_lowers_the_keyed_floor() {
    use kbf_estimator::GIB;
    let rows = store();
    let mut killed = killed_farm(rows.clone(), &rows);
    let size_key = killed.size_key;
    // 255 quiet runs leave the key one short of the threshold.
    killed.quiet_runs(8, 255);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(16 * GIB));
    let hinted = Job {
        memory: BookSource::Hint,
        ..Job::new(3)
    };
    hinted.submit(&killed.farm);
    let (operation, lease) = newest(&killed.farm, &killed.node);
    assert_eq!(keys(&rows)[1], None, "the client sized it: not keyed");
    succeed(&killed.farm, &killed.node, operation, lease, 500 << 20);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(8 * GIB));
}

/// The farm kills the newest operation the node holds at its own memory limit.
fn kill_newest(farm: &Farm, node: &kbf_types::WorkerId) {
    let (operation, lease) = newest(farm, node);
    let report = super::report::Report {
        lease,
        outcome: kbf_types::Outcome::Failed(kbf_types::Failure::OutOfMemory),
        measured: None,
        exit_code: None,
        run: None,
    };
    let now = farm.now();
    farm.lock().take_report(now, node, operation, report);
}

fn key_floor_rows(rows: &Arc<Fake>) -> Vec<String> {
    rows.scan_prefix("keyfloor/")
        .expect("scan")
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Requeues are retried independently: an older one the store refused, retried after a
/// newer one landed, writes the floor as the scheduler holds it now and not the one it was
/// taken with. (The per-digest floors of the same requeues keep the pattern they had.)
#[test]
fn a_requeue_retried_after_a_newer_one_writes_the_floor_as_it_stands() {
    use kbf_estimator::GIB;
    let rows = store();
    let refusing = Refusing::new(&rows);
    // The first kill's requeue is refused until released, whatever lands in between.
    refusing.stick(0);
    let killed = killed_farm(refusing.clone(), &rows);
    let size_key = killed.size_key;
    assert_eq!(row_ask(&rows), None, "the first kill's row was refused");
    Job::new(2).submit(&killed.farm);
    kill_newest(&killed.farm, &killed.node);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(32 * GIB));
    assert_eq!(row_ask(&rows), Some(32 * GIB), "the newer requeue landed");
    refusing.release();
    killed.farm.lock().flush_step();
    assert_eq!(row_ask(&rows), Some(32 * GIB), "the older retry leaves it");
}

/// A key forgotten by one requeue and raised again by a later one: the older requeue,
/// retried last, must not delete the live key's row nor bring back the forgotten one.
#[test]
fn an_eviction_retried_after_a_re_raise_does_not_delete_the_live_row() {
    use kbf_estimator::GIB;
    let rows = store();
    let refusing = Refusing::new(&rows);
    let killed = killed_farm_on(refusing.clone(), &rows, &[("node-a", 64)], Some(1));
    let size_key = killed.size_key;
    // Another shape's kill forgets this key (one floor is kept): its requeue is refused
    // until released, whatever lands in between.
    refusing.stick(1);
    let other = Job {
        program: "ld",
        ..Job::new(2)
    };
    other.submit(&killed.farm);
    kill_newest(&killed.farm, &killed.node);
    assert_eq!(
        killed.farm.lock().sched.key_floor(size_key),
        None,
        "evicted"
    );
    // A new digest of the first shape raises its floor again, forgetting the other.
    Job::new(3).submit(&killed.farm);
    kill_newest(&killed.farm, &killed.node);
    assert_eq!(killed.farm.lock().sched.key_floor(size_key), Some(16 * GIB));
    assert_eq!(
        key_floor_rows(&rows),
        vec![format!("keyfloor/{size_key:016x}")]
    );
    refusing.release();
    killed.farm.lock().flush_step();
    assert_eq!(
        key_floor_rows(&rows),
        vec![format!("keyfloor/{size_key:016x}")],
        "the older requeue's retry neither deletes the live row nor writes the forgotten one"
    );
}

/// A lowered row pending for the store belongs to a key another kill has since made the
/// scheduler forget: it is not written, and the forgotten key's row stays gone.
#[test]
fn a_pending_lowered_row_of_a_forgotten_key_is_not_written() {
    let rows = store();
    let refusing = Refusing::new(&rows);
    let mut killed = killed_farm_on(refusing.clone(), &rows, &[("node-a", 64)], Some(1));
    let size_key = killed.size_key;
    refusing
        .refuse
        .store(true, std::sync::atomic::Ordering::SeqCst);
    killed.quiet(8);
    assert_eq!(killed.farm.lock().unwritten_floors.len(), 1, "pending");
    let other = Job {
        program: "ld",
        ..Job::new(2)
    };
    other.submit(&killed.farm);
    kill_newest(&killed.farm, &killed.node);
    assert_eq!(
        killed.farm.lock().sched.key_floor(size_key),
        None,
        "forgotten"
    );
    refusing
        .refuse
        .store(false, std::sync::atomic::Ordering::SeqCst);
    killed.farm.lock().flush_step();
    killed.farm.lock().flush_learned();
    assert!(
        !key_floor_rows(&rows).contains(&format!("keyfloor/{size_key:016x}")),
        "{:?}",
        key_floor_rows(&rows)
    );
    assert_eq!(key_floor_rows(&rows).len(), 1, "only the other key's row");
}

/// With `on`, a keyed operation's attempts are judged against the largest worker that
/// could run it, as its key floor is; an operation with no key (the client sized it)
/// keeps the bound of the worker it ran on.
#[test]
fn a_keyed_operation_is_bounded_by_the_largest_worker_and_an_unkeyed_one_by_its_own() {
    let rows = store();
    let farm = farm_over(&rows, LearnedSizes::On);
    let small = register_sized(&farm, "a-small", 16);
    register_sized(&farm, "b-big", 64);
    Job::new(1).submit(&farm);
    Job {
        memory: BookSource::Hint,
        ..Job::new(2)
    }
    .submit(&farm);
    let state = farm.lock();
    let mut ops: Vec<_> = state.sched.unfinished().collect();
    ops.sort_unstable();
    assert_eq!(state.bounds_of(ops[0], &small).host_memory, 64 << 30);
    assert_eq!(state.bounds_of(ops[1], &small).host_memory, 16 << 30);
}

/// A stored operation names its estimator key at restart only when what names it is there:
/// a subject, a platform that pins the OS, and a key that is not the empty subject's.
#[test]
fn a_stored_operation_is_registered_only_when_it_can_be_named() {
    use super::durable::StoredOperation;
    use crate::action_usage::Subject;
    use kbf_front::Described;

    let rows = store();
    let farm = farm_over(&rows, LearnedSizes::On);
    let (subject, _) = Subject::of(Described {
        arguments: vec!["cc".to_owned()],
        ..Described::default()
    });
    let (empty, _) = Subject::of(Described::default());
    let linux = kbf_caps::Request::from_platform([("OSFamily", "linux")]).unwrap();
    let action = ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [1; 32], 1),
    };
    let stored = |needs: kbf_caps::Request, subject: Option<Subject>| StoredOperation {
        operation: kbf_types::OperationId(1),
        action: action.clone(),
        request: kbf_sched::Request {
            key: (&action).into(),
            qos: Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(1_000, 4 << 30),
            hermetic: true,
            do_not_cache: false,
            needs,
            whole_worker: false,
        },
        lease: None,
        losses: 0,
        fate: None,
        subject,
        size_key: Some(5),
        max_wall: None,
    };
    let (key, _) = estimator_key_and_run();
    {
        let mut state = farm.lock();
        state.register_stored(&stored(linux.clone(), None), 5);
        state.register_stored(
            &stored(kbf_caps::Request::default(), Some(subject.clone())),
            5,
        );
        state.register_stored(&stored(linux.clone(), Some(empty)), 5);
        assert!(
            state.side.learned.named(&key).is_empty(),
            "none can be named"
        );
        state.register_stored(&stored(linux, Some(subject)), 5);
    }
    assert_eq!(farm.lock().side.learned.named(&key), vec![5]);
}

/// A report the farm never saw start, and a success that carries no usage, teach the
/// estimator nothing and are counted as not shown; neither stops the report.
#[test]
fn a_report_without_a_booking_or_usage_is_counted_not_shown() {
    use crate::learned::Skip;
    let rows = store();
    let killed = killed_farm(rows.clone(), &rows);
    // A lease the farm never saw start.
    let stranger = kbf_types::LeaseId::new(9, 4_242);
    let report = super::report::Report {
        lease: stranger,
        outcome: kbf_types::Outcome::Completed {
            action_result: Digest::new(DigestFunction::Sha256, [1; 32], 1).into(),
        },
        measured: None,
        exit_code: Some(0),
        run: None,
    };
    let now = killed.farm.now();
    killed
        .farm
        .lock()
        .take_report(now, &killed.node, kbf_types::OperationId(0), report);
    assert_eq!(killed.farm.lock().side.learned.skipped(Skip::NoBooking), 1);

    // A success of the retry that measured nothing.
    let (operation, lease) = newest(&killed.farm, &killed.node);
    let report = super::report::Report {
        lease,
        outcome: kbf_types::Outcome::Completed {
            action_result: Digest::new(DigestFunction::Sha256, [1; 32], 1).into(),
        },
        measured: None,
        exit_code: Some(0),
        run: None,
    };
    killed
        .farm
        .lock()
        .take_report(now, &killed.node, operation, report);
    assert_eq!(killed.farm.lock().side.learned.skipped(Skip::NoBasis), 1);
}
