//! `--usage-profiles record` on the js core: what the farm learns from the heartbeats and
//! results it takes, through [`Farm`]'s public methods. A run is a lease: a node that runs
//! it reports its minutes on heartbeats, the farm accepts its `Result`, and the hub reads the
//! result on the node's next beats (the grace is counted in beats of the node, two).
//!
//! No test reads a clock or waits: the beats are numbered by hand and the wall minute of a
//! tick is an argument.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_caps::NodeCaps;
use kbf_front::{Described, Dispatch as _, Submission};
use kbf_proto::worker::server_message::Message;
use kbf_proto::worker::{self, MemoryKill};
use kbf_sched::MemoryLadder;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::sched::usage::{CurveSource, LeaseMinute, NodeLoad};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseId, LeaseKind, Qos, Resources, work_key};

use super::hot::{Joined, backend, farm_on, join, killed, linux, node, ok, rerun_start};
use crate::farm::{EpochPolicy, Farm, Reported};
use crate::farm_js::usage::UsageStats;
use crate::state_store::StateStore;
use crate::{SchedulerCore, UsageProfiles};

const GIB: u64 = 1 << 30;

fn fake() -> Arc<Fake> {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    kbf_state::share(&*store).expect("the shared marker");
    store
}

/// A store whose writes of usage rows, and reads of them, fail while it is told to.
struct Gate {
    inner: Arc<Fake>,
    writes_fail: AtomicBool,
    reads_fail: AtomicBool,
    /// Every write fails, the farm's own rows too.
    all_fail: AtomicBool,
}

impl Gate {
    fn over(inner: Arc<Fake>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            writes_fail: AtomicBool::new(false),
            reads_fail: AtomicBool::new(false),
            all_fail: AtomicBool::new(false),
        })
    }

    fn refused() -> StoreError {
        StoreError::Failed("the usage rows are refused by the test".to_owned())
    }
}

impl Store for Gate {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let usage = batch.iter().flat_map(Txn::ops).any(|op| {
            op.key_cond()
                .is_some_and(|(key, _)| key.starts_with("usage/"))
        });
        if self.all_fail.load(Ordering::SeqCst) {
            return Err(Self::refused());
        }
        if usage && self.writes_fail.load(Ordering::SeqCst) {
            return Err(Self::refused());
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        if prefix.starts_with("usage/") && self.reads_fail.load(Ordering::SeqCst) {
            return Err(Self::refused());
        }
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// The action `tag` runs `compile`, asking for 4 GiB on Linux: every tag is another action
/// of the one shape.
fn submission(tag: u8) -> Submission {
    let key = ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
    };
    let request = kbf_sched::Request {
        key: key.into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 4 * GIB),
        hermetic: true,
        // So that an action asked for again runs again, not from the cache.
        do_not_cache: true,
        needs: kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("a platform"),
        whole_worker: false,
    };
    Submission {
        request,
        call: None,
        described: Described {
            arguments: vec!["compile".to_owned()],
            ..Described::default()
        },
        evidence: Default::default(),
        max_wall: None,
    }
}

fn action_of(tag: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
    }
}

/// A farm on the js core over `store`, learning as `mode` says, and one Linux node of eight
/// cores and 64 GiB on it. The farm's usage side is read through [`Farm::usage_stats`].
struct Rig {
    farm: Farm,
    joined: Joined,
    name: &'static str,
    /// The number of the next heartbeat, the load's own sequence too.
    seq: u64,
}

fn state(store: &Arc<dyn Store>) -> StateStore {
    StateStore {
        store: Arc::clone(store),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }
}

fn rig_over(store: &Arc<dyn Store>, mode: UsageProfiles) -> Rig {
    rig_capped(store, mode, None)
}

/// [`rig_over`], keeping at most `cap` records a refusing store has not taken, if given.
fn rig_capped(store: &Arc<dyn Store>, mode: UsageProfiles, cap: Option<usize>) -> Rig {
    let farm = farm_on(SchedulerCore::Js, &backend())
        .with_memory_ladder(MemoryLadder::from_gib(&[4, 16, 32]).expect("a ladder"));
    let farm = match cap {
        Some(cap) => farm.with_held_cap(cap),
        None => farm,
    };
    let farm = farm
        .with_usage_profiles(mode)
        .with_state(Some(state(store)))
        .expect("a farm over the store");
    let mut reported = linux();
    reported.capacity = Resources::new(8_000, 64 * GIB);
    let joined = join(&farm, "node-u", reported);
    Rig {
        farm,
        joined,
        name: "node-u",
        seq: 0,
    }
}

fn rig(mode: UsageProfiles) -> (Rig, Arc<Fake>) {
    let store = fake();
    let shared: Arc<dyn Store> = store.clone();
    (rig_over(&shared, mode), store)
}

fn minutes_of(lease: LeaseId, minutes: u32) -> Vec<LeaseMinute> {
    (0..minutes)
        .map(|minute| LeaseMinute {
            lease,
            minute,
            cpu_mean_millis: 1_000,
            cpu_peak_window_millis: 1_001,
            ws_max_bytes: 5_000,
        })
        .collect()
}

fn load_of(seq: u64, minutes: Vec<LeaseMinute>) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 1_000_000,
        cpu_usage_usec: seq * 500_000,
        memory_working_set_bytes: 1 << 20,
        minutes,
        ..NodeLoad::default()
    }
}

fn lease_of(wire: &worker::LeaseId) -> LeaseId {
    LeaseId::new(wire.term, wire.seq)
}

impl Rig {
    /// One heartbeat of the node, running `running`, carrying `minutes`; whether it was
    /// acknowledged.
    async fn beat(&mut self, running: Vec<LeaseId>, minutes: Vec<LeaseMinute>) -> bool {
        self.seq += 1;
        let load = load_of(self.seq, minutes);
        self.farm
            .heartbeat_with_load(
                &node(self.name),
                self.joined.stream,
                self.seq,
                running,
                Some(load),
            )
            .await
    }

    /// The beats that carry no load: the first carries the result accepted so far to the hub,
    /// and the hub hands the run to the profile after the grace beats that follow it.
    async fn grace(&mut self) {
        for _ in 0..=js_usage::GRACE_BEATS {
            self.seq += 1;
            let acknowledged = self
                .farm
                .heartbeat_with_load(
                    &node(self.name),
                    self.joined.stream,
                    self.seq,
                    Vec::new(),
                    None,
                )
                .await;
            assert!(acknowledged);
        }
    }

    /// Submits the action `tag` and returns what the node was sent to run it. Anything the
    /// node was sent before (the rerun of a killed run) is dropped.
    fn start(&mut self, tag: u8) -> worker::Start {
        let _earlier = self.joined.sent();
        let _ticket = self.farm.submit(submission(tag)).expect("queued");
        self.joined
            .sent()
            .into_iter()
            .find_map(|message| match message {
                Message::Start(start) => Some(start),
                _ => None,
            })
            .expect("a Start")
    }

    /// Runs the action `tag` for three minutes and reports its result, with `report` made
    /// from the lease; returns the lease. The hub has not read the result yet.
    async fn run_with(
        &mut self,
        tag: u8,
        report: impl FnOnce(&worker::LeaseId) -> worker::Result,
    ) -> LeaseId {
        let start = self.start(tag);
        let wire = start.lease_id.expect("a lease");
        let lease = lease_of(&wire);
        assert!(self.beat(vec![lease], minutes_of(lease, 3)).await);
        let ack = self
            .farm
            .report(&node(self.name), report(&wire))
            .await
            .expect("an ack");
        assert!(ack.accepted, "{ack:?}");
        lease
    }

    /// A run that ends on its own with exit code 0, read by the hub after its grace.
    async fn clean(&mut self, tag: u8) -> LeaseId {
        let lease = self.run_with(tag, |wire| ok(wire, None)).await;
        self.grace().await;
        lease
    }

    fn stats(&self) -> UsageStats {
        self.farm.usage_stats().expect("a farm that records")
    }
}

/// What a farm placed over three clean runs and a fourth action, as the node was told, and
/// what the usage side held at the end.
async fn script(mode: UsageProfiles) -> (Vec<(u64, u64)>, Option<UsageStats>) {
    let (mut rig, _store) = rig(mode);
    let mut booked = Vec::new();
    for tag in 1..=3 {
        let start = rig.start(tag);
        booked.push((start.millicpus, start.memory_bytes));
        let wire = start.lease_id.expect("a lease");
        let lease = lease_of(&wire);
        assert!(rig.beat(vec![lease], minutes_of(lease, 3)).await);
        let ack = rig
            .farm
            .report(&node("node-u"), ok(&wire, None))
            .await
            .expect("an ack");
        assert!(ack.accepted);
        rig.grace().await;
    }
    let start = rig.start(4);
    booked.push((start.millicpus, start.memory_bytes));
    (booked, rig.farm.usage_stats())
}

/// Catches `record` learning nothing from the heartbeats and results of three clean runs of
/// one shape (a profile, three runs learned, no run left held), and catches it changing where
/// work goes: the fourth action is booked as under `off`, the same as the first three.
#[tokio::test(start_paused = true)]
async fn record_learns_a_profile_from_three_clean_runs_and_places_nothing_differently() {
    let (off, off_stats) = script(UsageProfiles::Off).await;
    let (record, stats) = script(UsageProfiles::Record).await;
    assert_eq!(off_stats, None);
    assert_eq!(record, off);
    let stats = stats.expect("record keeps stats");
    assert_eq!(stats.keys, 1);
    assert_eq!(stats.counters.runs_learned, 3);
    assert_eq!(stats.declined, 0);
    assert_eq!(stats.held_runs, 0, "the fourth action has not reported");
    assert_eq!(stats.waiting, 0);
}

/// Pins what `off` has always done (it arrives green): no usage side, and not one row under
/// `usage/` in the state store after the same runs that `record` learns from.
#[tokio::test(start_paused = true)]
async fn off_records_nothing_and_writes_no_rows() {
    let (mut rig, store) = rig(UsageProfiles::Off);
    for tag in 1..=3 {
        rig.clean(tag).await;
    }
    assert_eq!(rig.farm.usage_stats(), None);
    let rows = store.scan_prefix("usage/").expect("a scan");
    assert!(rows.is_empty(), "{} rows", rows.len());
}

/// Catches a run that did not end on its own teaching the profile: an own-limit kill and a
/// kill for node pressure are reported (each leaves minutes on file), then three clean runs;
/// the profile has three runs, not five.
#[tokio::test(start_paused = true)]
async fn killed_run_is_not_learned() {
    let (mut rig, _store) = rig(UsageProfiles::Record);
    rig.run_with(1, |wire| killed(wire, MemoryKill::OwnLimit))
        .await;
    rig.run_with(2, |wire| killed(wire, MemoryKill::NodePressure))
        .await;
    rig.grace().await;
    assert_eq!(rig.stats().counters.runs_learned, 0);
    assert_eq!(rig.stats().keys, 0);
    for tag in 3..=5 {
        rig.clean(tag).await;
    }
    let stats = rig.stats();
    assert_eq!(stats.counters.runs_learned, 3);
    assert_eq!(stats.keys, 1);
    assert_eq!(stats.held_runs, 0);
}

fn mac() -> Reported {
    let caps = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
        .expect("caps")
        .with_drivers(["container"]);
    Reported::serving(Resources::new(4_000, 8 * GIB), caps)
}

/// Catches minutes filed for a lease by a node the farm does not hold it for: a macOS node,
/// which cannot run the Linux action, reports three minutes of the lease the Linux node holds.
/// They are counted as unheld and no run follows.
#[tokio::test(start_paused = true)]
async fn a_beat_from_the_wrong_host_files_no_minutes() {
    let (mut rig, _store) = rig(UsageProfiles::Record);
    let other = join(&rig.farm, "node-m", mac());
    let wire = rig.start(1).lease_id.expect("a lease");
    let lease = lease_of(&wire);
    let beaten = rig
        .farm
        .heartbeat_with_load(
            &node("node-m"),
            other.stream,
            1,
            Vec::new(),
            Some(load_of(1, minutes_of(lease, 3))),
        )
        .await;
    assert!(beaten, "the beat is served");
    assert_eq!(rig.stats().counters.minutes_unheld, 3);
    let ack = rig
        .farm
        .report(&node("node-u"), ok(&wire, None))
        .await
        .expect("an ack");
    assert!(ack.accepted);
    rig.grace().await;
    let stats = rig.stats();
    assert_eq!(stats.counters.runs_learned, 0, "no minutes of its own");
    assert_eq!(stats.counters.runs_without_series, 1);
}

/// Catches a load the usage side cannot take (a lease minute past what a minute index holds)
/// stopping the beat or going unseen: the beat is acknowledged and one load is counted
/// rejected.
#[tokio::test(start_paused = true)]
async fn a_bad_load_is_counted_and_the_beat_is_served() {
    let (mut rig, _store) = rig(UsageProfiles::Record);
    let lease = lease_of(&rig.start(1).lease_id.expect("a lease"));
    let mut minutes = minutes_of(lease, 1);
    minutes[0].minute = 70_000;
    assert!(rig.beat(vec![lease], minutes).await);
    assert_eq!(rig.stats().counters.loads_rejected, 1);
    assert!(rig.beat(vec![lease], minutes_of(lease, 1)).await);
    assert_eq!(rig.stats().counters.loads_rejected, 1);
}

/// Catches profiles that do not survive a restart: a farm over the same store has the key at
/// its start, and the curve of an action of the shape, once the action is submitted (the
/// farm names its profile then), from the three runs.
#[tokio::test(start_paused = true)]
async fn a_restart_reads_the_profiles_back() {
    let store = fake();
    let shared: Arc<dyn Store> = store.clone();
    let mut first = rig_over(&shared, UsageProfiles::Record);
    for tag in 1..=3 {
        first.clean(tag).await;
    }
    assert_eq!(first.stats().keys, 1);
    drop(first);

    let second = rig_over(&shared, UsageProfiles::Record);
    let stats = second.stats();
    assert_eq!(stats.keys, 1);
    assert_eq!(stats.counters.runs_learned, 0);
    let _ticket = second.farm.submit(submission(9)).expect("queued");
    let usage = super::js(&second.farm).usage.as_ref().expect("usage");
    let curve = usage
        .sink()
        .curves()
        .curve(&work_key("main", action_of(9).action))
        .expect("a curve for an action of the shape");
    assert_eq!(curve.runs(), 3);
}

/// Catches a store that refuses the profile rows stopping the farm or going uncounted: the
/// result is acknowledged, the failure is one fault and the run is not learned; once the
/// store takes writes the next run is.
#[tokio::test(start_paused = true)]
async fn a_store_failure_is_counted_and_the_farm_goes_on() {
    let gate = Gate::over(fake());
    let shared: Arc<dyn Store> = gate.clone();
    let mut rig = rig_over(&shared, UsageProfiles::Record);
    gate.writes_fail.store(true, Ordering::SeqCst);
    rig.clean(1).await;
    let stats = rig.stats();
    assert_eq!(stats.counters.faults, 1);
    assert_eq!(stats.counters.runs_learned, 0);
    gate.writes_fail.store(false, Ordering::SeqCst);
    rig.clean(2).await;
    let stats = rig.stats();
    assert_eq!(stats.counters.faults, 1);
    assert_eq!(stats.counters.runs_learned, 1);
    assert_eq!(stats.keys, 1);
}

/// Catches profiles that cannot be read at the start stopping the farm: it runs without
/// recording, and runs the action it is given.
#[tokio::test(start_paused = true)]
async fn profiles_that_cannot_be_read_at_the_start_leave_the_farm_running() {
    let gate = Gate::over(fake());
    gate.reads_fail.store(true, Ordering::SeqCst);
    let shared: Arc<dyn Store> = gate;
    let mut rig = rig_over(&shared, UsageProfiles::Record);
    assert_eq!(rig.farm.usage_stats(), None);
    rig.clean(1).await;
}

/// How a run ends before its key is forgotten: the result is accepted and no beat has read it.
async fn reported_not_read(rig: &mut Rig, tag: u8, unclean: bool) {
    if unclean {
        rig.run_with(tag, |wire| killed(wire, MemoryKill::OwnLimit))
            .await;
    } else {
        rig.run_with(tag, |wire| ok(wire, None)).await;
    }
    let stats = rig.stats();
    assert_eq!(
        stats.held_runs, 1,
        "the run is bound once its result is accepted"
    );
    assert_eq!(stats.waiting, 1);
}

fn nothing_held(rig: &Rig) {
    let stats = rig.stats();
    assert_eq!(stats.held_runs, 0);
    assert_eq!(stats.waiting, 0);
}

/// Catches a run whose key outlives it, by each way it can end: a clean result and an unclean
/// one read by the node's next beat, a result no beat reads for a day (the node went away; a
/// tick at an explicit wall minute lets it go), and a handover (`fence`). Each leaves nothing
/// bound, so the table cannot fill and stop learning.
#[tokio::test(start_paused = true)]
async fn every_end_of_a_run_forgets_its_key() {
    let (mut clean, _s1) = rig(UsageProfiles::Record);
    reported_not_read(&mut clean, 1, false).await;
    clean.grace().await;
    nothing_held(&clean);

    let (mut unclean, _s2) = rig(UsageProfiles::Record);
    reported_not_read(&mut unclean, 1, true).await;
    unclean.grace().await;
    nothing_held(&unclean);

    let (mut gone, _s3) = rig(UsageProfiles::Record);
    reported_not_read(&mut gone, 1, false).await;
    let usage = super::js(&gone.farm).usage.as_ref().expect("usage");
    usage.tick_blocking(1 << 40);
    nothing_held(&gone);

    let (mut handed, _s4) = rig(UsageProfiles::Record);
    reported_not_read(&mut handed, 1, false).await;
    handed.farm.fence();
    nothing_held(&handed);
}

/// Catches the profile name of an action being removed while a later run of the action still
/// needs it: it is there when the third result is accepted, and still there after the beats
/// that read it, which learned the run. (It is bounded by the index's own cap, not removed.)
#[tokio::test(start_paused = true)]
async fn the_shape_entry_stays_after_the_result_is_read() {
    let (mut rig, _store) = rig(UsageProfiles::Record);
    rig.clean(1).await;
    rig.clean(2).await;
    rig.run_with(3, |wire| ok(wire, None)).await;
    let usage = Arc::clone(super::js(&rig.farm).usage.as_ref().expect("usage"));
    assert!(usage.shapes().get(&action_of(3)).is_some());
    assert_eq!(rig.stats().counters.runs_learned, 2);
    rig.grace().await;
    assert!(usage.shapes().get(&action_of(3)).is_some());
    assert_eq!(rig.stats().counters.runs_learned, 3);
}

/// Catches the rerun of a killed run going unlearned: the kill is read by a beat, which let go
/// of the action's profile name, and the rerun's clean result (the size after the ladder) had
/// no key. The rerun is learned, and the kill is not.
#[tokio::test(start_paused = true)]
async fn a_killed_run_and_its_clean_rerun_teach_the_rerun() {
    let (mut rig, _store) = rig(UsageProfiles::Record);
    rig.run_with(1, |wire| killed(wire, MemoryKill::OwnLimit))
        .await;
    let wire = rerun_start(&rig.farm, &mut rig.joined).await;
    let lease = lease_of(&wire);
    assert!(rig.beat(vec![lease], minutes_of(lease, 3)).await);
    let ack = rig
        .farm
        .report(&node(rig.name), ok(&wire, None))
        .await
        .expect("an ack");
    assert!(ack.accepted, "{ack:?}");
    rig.grace().await;
    let stats = rig.stats();
    assert_eq!(stats.counters.runs_without_key, 0);
    assert_eq!(stats.counters.runs_learned, 1);
    assert_eq!(stats.held_runs, 0);
}

/// Catches two runs of one action in flight sharing one profile name that the first to be read
/// removes: the action runs, its result is accepted and not yet read, the action is asked for
/// again and runs; both are learned.
#[tokio::test(start_paused = true)]
async fn two_runs_of_one_action_in_flight_are_both_learned() {
    let (mut rig, _store) = rig(UsageProfiles::Record);
    rig.run_with(1, |wire| ok(wire, None)).await;
    rig.run_with(1, |wire| ok(wire, None)).await;
    rig.grace().await;
    let stats = rig.stats();
    assert_eq!(stats.counters.runs_without_key, 0);
    assert_eq!(stats.counters.runs_learned, 2);
}

/// Catches a result the core took but did not finish writing (the store refuses the record)
/// or dropped over the cap going unseen: it is counted by which of the two it was, and its
/// run is not followed.
#[tokio::test(start_paused = true)]
async fn a_result_the_store_has_not_taken_is_counted_not_followed() {
    for (cap, held, dropped) in [(None, 1, 0), (Some(0), 0, 1)] {
        let gate = Gate::over(fake());
        let shared: Arc<dyn Store> = gate.clone();
        let mut rig = rig_capped(&shared, UsageProfiles::Record, cap);
        let wire = rig.start(1).lease_id.expect("a lease");
        let lease = lease_of(&wire);
        assert!(rig.beat(vec![lease], minutes_of(lease, 3)).await);
        gate.all_fail.store(true, Ordering::SeqCst);
        let ack = rig.farm.report(&node(rig.name), ok(&wire, None)).await;
        assert_eq!(ack, None);
        let stats = rig.stats();
        assert_eq!(stats.results_held, held, "cap {cap:?}");
        assert_eq!(stats.results_dropped, dropped, "cap {cap:?}");
        assert_eq!(stats.held_runs, 0);
        assert_eq!(stats.waiting, 0);
    }
}

/// Catches the usage families missing from a scrape of a farm that records, or present on one
/// that does not: `off` renders none, `record` renders the counts of what it learned.
#[tokio::test(start_paused = true)]
async fn the_usage_families_are_on_a_scrape_only_when_the_farm_records() {
    let scrape = |farm: &Farm| {
        let mut out = String::new();
        crate::metrics::render_farm(&mut out, farm);
        out
    };
    let (mut off, _s1) = rig(UsageProfiles::Off);
    off.clean(1).await;
    assert!(!scrape(&off.farm).contains("kbf_usage_runs"));
    let (mut record, _s2) = rig(UsageProfiles::Record);
    for tag in 1..=3 {
        record.clean(tag).await;
    }
    let text = scrape(&record.farm);
    assert!(text.contains("kbf_usage_runs_learned_total 3\n"), "{text}");
    assert!(text.contains("kbf_usage_profile_keys 1\n"), "{text}");
}

/// Catches the farm's tick and its shutdown not reaching the usage side: a tick runs the
/// usage tick (once, for the first call), the final flush is taken by the store, and a farm
/// that records nothing ticks and finishes as it always did.
#[tokio::test(start_paused = true)]
async fn the_farms_tick_and_shutdown_reach_the_usage_side() {
    let (record, _s1) = rig(UsageProfiles::Record);
    record.farm.tick().await;
    record.farm.tick().await;
    assert_eq!(record.stats().ticks, 1);
    assert!(record.farm.finish_usage().await);
    let (off, _s2) = rig(UsageProfiles::Off);
    off.farm.tick().await;
    assert!(off.farm.finish_usage().await);
}
