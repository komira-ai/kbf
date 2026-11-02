//! Each rule of the shadow has a test here that goes red when the rule is broken; the
//! doc comment of a test names it.

use kbf_estimator::{GIB, MIB};
use kbf_front::Described;
use kbf_invocation::FootprintUsage;
use kbf_types::{ActionKey, Digest, DigestFunction, FencePolicy, OperationId, Resources, WorkerId};

use super::*;

const DAY: u64 = DAY_MS;
/// The wall time the test farm was built at: day 20,000, five seconds in.
const EPOCH: u64 = 20_000 * DAY + 5_000;

fn bounds() -> Bounds {
    Bounds {
        default_memory: 4 * GIB,
        host_memory: 64 * GIB,
        node_cpu_millis: 8_000,
    }
}

fn linux() -> Platform {
    Platform {
        os_family: "linux",
        isa: "-".to_owned(),
    }
}

fn subject(cfg: Option<&str>) -> Subject {
    let (mut subject, _) = Subject::of(Described {
        arguments: vec!["compile".to_owned()],
        ..Described::default()
    });
    subject.cfg = cfg.map(str::to_owned);
    subject
}

fn key() -> Key {
    key_of("main", &linux(), Basis::Cgroup, &subject(None)).expect("a key")
}

fn cgroup(peak: Option<u64>) -> RunUsage {
    RunUsage {
        wall_micros: 2_000_000,
        basis: UsageBasis::Cgroup,
        memory_peak_bytes: peak,
        cpu_usage_micros: Some(500),
        ..RunUsage::default()
    }
}

fn ok() -> Outcome {
    Outcome::Completed {
        action_result: Digest::new(DigestFunction::Sha256, [1; 32], 1).into(),
    }
}

/// A success of `peak` bytes booked at 4 GiB.
fn success(peak: u64) -> Attempt {
    let (_, run) = run_of(ok(), Some(&cgroup(Some(peak))), 4 * GIB).expect("a run");
    Attempt { key: key(), run }
}

/// An own-limit kill booked at 4 GiB.
fn kill(used: Option<u64>) -> Attempt {
    let usage = cgroup(used);
    let (_, run) =
        run_of(Outcome::Failed(Failure::OutOfMemory), Some(&usage), 4 * GIB).expect("a run");
    Attempt { key: key(), run }
}

fn lease(seq: u64) -> LeaseId {
    LeaseId::new(5, seq)
}

fn start(seq: u64, kind: LeaseKind, gpus: u64) -> StartLease {
    StartLease {
        worker: WorkerId::new("node-a"),
        lease: lease(seq),
        operation: OperationId(seq),
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [2; 32], 2),
        }
        .into(),
        kind,
        resources: Resources {
            gpus,
            ..Resources::new(1_000, 4 * GIB)
        },
        fence: FencePolicy::RunOn,
        device: None,
    }
}

fn shadow() -> Learned {
    Learned::new(LearnedSizes::Shadow, 4 * GIB, EPOCH)
}

/// A shadow that has seen five quiet runs of 100 MiB: its estimate is 1 GiB.
fn seeded() -> Learned {
    let mut learned = shadow();
    for _ in 0..5 {
        learned.observe(&success(100 * MIB), &bounds());
    }
    learned
}

fn estimate(memory: u64, level: Level) -> Result<Estimate, Fallback> {
    Ok(Estimate {
        memory_bytes: memory,
        cpu_millis: 1_000,
        level,
        life_minutes: 1,
    })
}

/// Catches: an attempt put in the bucket the next one over (a table of one attempt for
/// each edge): the 95% edges of the estimate's limit and of the attempt's own, a kill
/// called covered (its peak is only where it was cut off), a saturated success called
/// covered at a larger estimate, a peak over the limit hidden by its being censored, the
/// level of the estimate lost, and a missing estimate called anything but cold.
#[test]
fn an_attempt_falls_into_the_bucket_its_numbers_name() {
    let one_gib = estimate(GIB, Level::L2);
    // limit(1 GiB) = 2 GiB; 95% of it is 1.9 GiB.
    let limit = memory_max(GIB).expect("a limit");
    let edge = limit / 20 * 19;
    let rows: [(&Result<Estimate, Fallback>, Run, ShadowLevel, ShadowBucket); 11] = [
        (
            &Err(Fallback::Cold),
            success(MIB).run,
            ShadowLevel::None,
            ShadowBucket::Cold,
        ),
        (
            &one_gib,
            success(100 * MIB).run,
            ShadowLevel::L2,
            ShadowBucket::Covered,
        ),
        (
            &one_gib,
            success(edge).run,
            ShadowLevel::L2,
            ShadowBucket::Covered,
        ),
        // Within 5% of the limit: it would have run against it.
        (
            &one_gib,
            success(edge + MIB).run,
            ShadowLevel::L2,
            ShadowBucket::Censored,
        ),
        (
            &one_gib,
            success(limit).run,
            ShadowLevel::L2,
            ShadowBucket::Censored,
        ),
        (
            &one_gib,
            success(limit + 1).run,
            ShadowLevel::L2,
            ShadowBucket::WouldOom,
        ),
        // A success that ran at 95% of its own 6.5 GiB limit, over the estimate's.
        (
            &one_gib,
            success(6 * GIB + 400 * MIB).run,
            ShadowLevel::L2,
            ShadowBucket::WouldOom,
        ),
        // The same success against an estimate that would have given it room: only a
        // lower bound, so no claim.
        (
            &estimate(16 * GIB, Level::L1),
            success(6 * GIB + 400 * MIB).run,
            ShadowLevel::L1,
            ShadowBucket::Censored,
        ),
        // A kill is a lower bound too, whatever it was seen to use.
        (
            &one_gib,
            kill(Some(MIB)).run,
            ShadowLevel::L2,
            ShadowBucket::WouldOom,
        ),
        (
            &estimate(16 * GIB, Level::L1),
            kill(None).run,
            ShadowLevel::L1,
            ShadowBucket::Censored,
        ),
        (
            &estimate(16 * GIB, Level::L1),
            kill(Some(20 * GIB)).run,
            ShadowLevel::L1,
            ShadowBucket::Censored,
        ),
    ];
    for (i, (estimate, run, level, bucket)) in rows.iter().enumerate() {
        assert_eq!(super::bucket(estimate, run), (*level, *bucket), "row {i}");
    }
}

/// Catches: a run counted from a daemon that did not say which instrument measured it
/// (its peak could be the daemon's `wait4` figure), a macOS run read from the cgroup
/// block or the other way round, an own-limit kill, a node-pressure kill, a lost run and
/// a plain failure confused, an absent peak made zero, the node's CPU pressure
/// (hundredths of a percent) not turned into the estimator's tenths, and a swap or limit
/// count dropped.
#[test]
fn a_run_is_read_from_the_block_its_basis_names() {
    let mut usage = cgroup(Some(7));
    usage.memory_events_max = Some(3);
    usage.memory_swap_peak_bytes = Some(9);
    usage.node_cpu_pressure_centi = Some(1_234);
    let (basis, run) = run_of(ok(), Some(&usage), 4 * GIB).expect("a run");
    assert_eq!(basis, Basis::Cgroup);
    assert_eq!(
        run,
        Run {
            outcome: RunOutcome::Success,
            wall_us: 2_000_000,
            peak_memory_bytes: Some(7),
            booked_memory_bytes: 4 * GIB,
            raised_memory_bytes: None,
            limit_hits: 3,
            swap_peak_bytes: 9,
            cpu_usage_us: 500,
            node_cpu_pressure_permille: Some(123),
        }
    );

    let mac = RunUsage {
        wall_micros: 1,
        basis: UsageBasis::MacFootprint,
        // A cgroup block the daemon must not have sent beside it is not read.
        memory_peak_bytes: Some(1),
        footprint: Some(FootprintUsage {
            peak_footprint_bytes: Some(11),
            cpu_user_micros: Some(20),
            cpu_system_micros: Some(30),
            wall_micros: Some(5_000_000),
        }),
        ..RunUsage::default()
    };
    let (basis, run) = run_of(ok(), Some(&mac), 4 * GIB).expect("a run");
    assert_eq!(basis, Basis::MacFootprint);
    assert_eq!(
        (run.peak_memory_bytes, run.wall_us, run.cpu_usage_us),
        (Some(11), 5_000_000, 50)
    );
    let bare = RunUsage {
        basis: UsageBasis::MacFootprint,
        wall_micros: 8,
        ..RunUsage::default()
    };
    let (_, run) = run_of(ok(), Some(&bare), 4 * GIB).expect("a run");
    assert_eq!(
        (run.peak_memory_bytes, run.wall_us, run.cpu_usage_us),
        (None, 8, 0)
    );

    let unsaid = RunUsage {
        basis: UsageBasis::Unspecified,
        ..cgroup(Some(7))
    };
    assert_eq!(run_of(ok(), Some(&unsaid), 4 * GIB), None);
    assert_eq!(run_of(ok(), None, 4 * GIB), None);

    let outcome = |o| {
        run_of(o, Some(&cgroup(Some(7))), 1)
            .expect("a run")
            .1
            .outcome
    };
    assert_eq!(
        outcome(Outcome::Failed(Failure::OutOfMemory)),
        RunOutcome::OwnLimitKill
    );
    assert_eq!(
        outcome(Outcome::Failed(Failure::NodeMemoryPressure)),
        RunOutcome::NodePressureKill
    );
    assert_eq!(
        outcome(Outcome::Failed(Failure::Lost(kbf_types::LostWhy::Fenced))),
        RunOutcome::Lost
    );
    assert_eq!(
        outcome(Outcome::Failed(Failure::Timeout)),
        RunOutcome::Failed
    );
    // Absurd pressure saturates instead of wrapping.
    let mut usage = cgroup(Some(7));
    usage.node_cpu_pressure_centi = Some(u32::MAX);
    let (_, run) = run_of(ok(), Some(&usage), 1).expect("a run");
    assert_eq!(run.node_cpu_pressure_permille, Some(u16::MAX));
}

/// Catches: a key made for a request whose `needs` do not pin the OS (the platform
/// class would be a guess), two instances or two architectures sharing a key, a `-` or
/// empty configuration hashed as a configuration, and the OS-to-instrument map wrong.
#[test]
fn a_key_names_instance_platform_basis_shape_and_configuration() {
    let none = Platform {
        os_family: "-",
        isa: "-".to_owned(),
    };
    assert_eq!(
        key_of("main", &none, Basis::Cgroup, &subject(None)),
        Err(Skip::NoOs)
    );
    // An operation restored from a row that kept no subject has no key of its own.
    let unknown = Subject::of(Described::default()).0;
    assert_eq!(
        key_of("main", &linux(), Basis::Cgroup, &unknown),
        Err(Skip::NoSubject)
    );
    let arm = Platform {
        os_family: "linux",
        isa: "arm64".to_owned(),
    };
    let a = key_of("main", &linux(), Basis::Cgroup, &subject(None)).expect("a key");
    let b = key_of("main", &arm, Basis::Cgroup, &subject(None)).expect("a key");
    let c = key_of("other", &linux(), Basis::Cgroup, &subject(None)).expect("a key");
    assert_eq!(a.platform().as_str(), "linux");
    assert_eq!(b.platform().as_str(), "linux/arm64");
    assert!(a != b && a != c);
    for cfg in ["-", ""] {
        assert_eq!(
            key_of("main", &linux(), Basis::Cgroup, &subject(Some(cfg))),
            Ok(a.clone())
        );
    }
    let with = key_of("main", &linux(), Basis::Cgroup, &subject(Some("7a3c"))).expect("a key");
    assert_eq!(with.cfg(), Some(Cfg(cfg_hash("7a3c"))));
    assert_eq!(with.l2(), a);
    assert_ne!(cfg_hash("a"), cfg_hash("b"));
    assert_eq!(basis_of_os("linux"), Some(Basis::Cgroup));
    assert_eq!(basis_of_os("macos"), Some(Basis::MacFootprint));
    assert_eq!(basis_of_os("-"), None);
}

/// Catches: a size the client named not passed as a hint (the estimate could go below
/// what the client asked for), a size the farm defaulted passed as one, and the two axes
/// swapped.
#[test]
fn only_the_axes_the_client_named_are_hints() {
    let evidence = |cpu, memory| Evidence {
        cpu_source: cpu,
        memory_source: memory,
        input_bytes: None,
        input_files: None,
    };
    let both = hint_of(
        &evidence(BookSource::Hint, BookSource::Hint),
        7 * GIB,
        2_500,
    );
    assert_eq!(
        (both.memory_bytes, both.cpu_millis),
        (Some(7 * GIB), Some(2_500))
    );
    let only_memory = hint_of(
        &evidence(BookSource::Default, BookSource::Hint),
        7 * GIB,
        1_000,
    );
    assert_eq!(
        (only_memory.memory_bytes, only_memory.cpu_millis),
        (Some(7 * GIB), None)
    );
    let only_cpu = hint_of(
        &evidence(BookSource::Hint, BookSource::Default),
        4 * GIB,
        2_000,
    );
    assert_eq!(
        (only_cpu.memory_bytes, only_cpu.cpu_millis),
        (None, Some(2_000))
    );
    let huge = hint_of(
        &evidence(BookSource::Hint, BookSource::Default),
        0,
        u64::MAX,
    );
    assert_eq!(huge.cpu_millis, Some(u32::MAX));
}

/// Catches: the estimator taught before it is asked (the second sample would already
/// have an estimate to land against, and `cold` would not be counted for it), attempts
/// counted in the wrong bucket or level, and a tally that does not reach the scrape.
/// From the second sample on the estimator answers with the default booking (it only
/// lowers from five), and from the sixth with its own 1 GiB.
#[test]
fn an_attempt_is_asked_about_before_it_is_taught() {
    let mut learned = shadow();
    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(learned.observe(&success(100 * MIB), &bounds()));
    }
    let cold = Some((ShadowLevel::None, ShadowBucket::Cold));
    let covered = Some((ShadowLevel::L2, ShadowBucket::Covered));
    assert_eq!(seen, [cold, cold, covered, covered, covered, covered]);
    assert_eq!(learned.tallied(ShadowLevel::None, ShadowBucket::Cold), 2);
    assert_eq!(learned.tallied(ShadowLevel::L2, ShadowBucket::Covered), 4);
    assert_eq!(learned.tallied(ShadowLevel::L1, ShadowBucket::Covered), 0);
    let totals = ShadowTotals::default();
    learned.publish(&totals);
    assert_eq!(totals.get(ShadowLevel::None, ShadowBucket::Cold), 2);
    assert_eq!(totals.get(ShadowLevel::L2, ShadowBucket::Covered), 4);
    assert_eq!(learned.calls(), 6);
}

/// Catches: an attempt that is no memory sample (a failure that is not a memory kill, a
/// run too short to measure) put in a bucket, so the tally would claim coverage the
/// estimator never judged.
#[test]
fn an_attempt_the_estimator_does_not_take_is_in_no_bucket() {
    let mut learned = seeded();
    let covered = learned.tallied(ShadowLevel::L2, ShadowBucket::Covered);
    let mut failed = success(100 * MIB);
    failed.run.outcome = RunOutcome::Failed;
    let mut short = success(100 * MIB);
    short.run.wall_us = 10;
    assert_eq!(learned.observe(&failed, &bounds()), None);
    assert_eq!(learned.observe(&short, &bounds()), None);
    assert_eq!(
        learned.tallied(ShadowLevel::L2, ShadowBucket::Covered),
        covered
    );
}

/// Catches: the estimate text without its level, memory, CPU or the default it is
/// compared with, the hint ignored (an estimate below what the client named), a missing
/// estimate not saying why, and a floor not shown.
#[test]
fn the_estimate_line_says_what_would_be_booked_and_what_is() {
    let mut learned = seeded();
    let text = learned.estimate_text(&key(), &Hint::default(), &bounds());
    let shape = format!("key={:016x}", key().shape().0);
    // The runs seeded here have no CPU reading, so the estimate takes the estimator's cold
    // CPU, two cores.
    assert_eq!(
        text,
        format!("{shape} level=l2 memory_mib=1024 cpu_millis=2000 default_mib=4096 floor_mib=0")
    );
    let hint = Hint {
        memory_bytes: Some(8 * GIB),
        cpu_millis: Some(2_000),
    };
    let text = learned.estimate_text(&key(), &hint, &bounds());
    assert_eq!(
        text,
        format!("{shape} level=l2 memory_mib=8192 cpu_millis=2000 default_mib=4096 floor_mib=0")
    );
    let other = key_of("other", &linux(), Basis::Cgroup, &subject(None)).expect("a key");
    let text = learned.estimate_text(&other, &Hint::default(), &bounds());
    assert_eq!(
        text,
        format!("{shape} fallback=Missing default_mib=4096 floor_mib=0")
    );
    // A kill sets a floor, which the line shows.
    let mut hit = kill(Some(5 * GIB));
    hit.run.raised_memory_bytes = Some(16 * GIB);
    learned.observe(&hit, &bounds());
    let text = learned.estimate_text(&key(), &Hint::default(), &bounds());
    assert!(text.ends_with("floor_mib=16384"), "{text}");
}

/// Catches: `off` asking, teaching, keeping or writing anything: a restored row taken
/// up, a lease's booking remembered, an attempt staged, a batch due or a sweep due.
#[test]
fn off_does_nothing_with_anything() {
    let mut on = shadow();
    for _ in 0..3 {
        on.observe(&success(MIB), &bounds());
    }
    let rows = on.rows_of(on.dirty.clone());
    let mut off = Learned::new(LearnedSizes::Off, 4 * GIB, EPOCH);
    assert!(off.off());
    off.restore(&rows);
    off.on_start(&start(1, LeaseKind::Action, 0));
    off.clock(FarmTime::from_millis(10 * DAY));
    assert_eq!(off.due(), Vec::new());
    assert_eq!(off.sweep_due(), None);
    assert_eq!(off.sweep_due(), None);
    assert!(off.summaries().is_empty());
    assert_eq!(off.booking(lease(1)), Err(Skip::NoBooking));
    assert_eq!(off.calls(), 0);
}

/// Catches: `shadow` ignoring the rows the store held (the learning restarts from nothing
/// at every deploy), and a row that does not decode stopping the others.
#[test]
fn shadow_takes_up_what_the_store_held() {
    let mut first = seeded();
    let mut rows = first.rows_of(first.dirty.clone());
    let other = key_of("other", &linux(), Basis::Cgroup, &subject(None)).expect("a key");
    rows.insert(0, (other.clone(), vec![1, 2, 3]));
    let mut second = shadow();
    second.restore(&rows);
    assert!(second.summaries().get(&key()).is_some());
    assert!(second.summaries().get(&other).is_none());
    let text = second.estimate_text(&key(), &Hint::default(), &bounds());
    assert!(text.contains(" level=l2 memory_mib=1024 "), "{text}");
}

/// Catches: leases the farm did not see start (taken over from a predecessor), or that
/// are not action leases without a GPU, shown to the estimator, and the count of why
/// not.
#[test]
fn only_action_leases_the_farm_saw_start_are_shown() {
    let mut learned = shadow();
    learned.on_start(&start(1, LeaseKind::Action, 0));
    learned.on_start(&start(2, LeaseKind::Action, 1));
    learned.on_start(&start(3, LeaseKind::Vm, 0));
    assert_eq!(learned.booking(lease(1)), Ok(4 * GIB));
    assert_eq!(learned.booking(lease(2)), Err(Skip::Kind));
    assert_eq!(learned.booking(lease(3)), Err(Skip::Kind));
    assert_eq!(learned.booking(lease(4)), Err(Skip::NoBooking));
    assert_eq!(
        (
            learned.skipped(Skip::Kind),
            learned.skipped(Skip::NoBooking)
        ),
        (2, 1)
    );
    learned.forget(lease(1));
    assert_eq!(learned.booking(lease(1)), Err(Skip::NoBooking));
    learned.stage(lease(1), None);
    learned.skip(Skip::NoOs);
    assert_eq!(
        (learned.skipped(Skip::NoBasis), learned.skipped(Skip::NoOs)),
        (1, 1)
    );
}

/// Catches: the lease booking table growing without bound (a lease whose result never
/// comes), and the newest forgotten instead of the oldest.
#[test]
fn the_booking_table_is_bounded_and_forgets_the_oldest() {
    let mut learned = shadow();
    for seq in 0..=STARTED_CAP as u64 {
        learned.on_start(&start(seq, LeaseKind::Action, 0));
    }
    assert_eq!(learned.started.len(), STARTED_CAP);
    assert_eq!(learned.booking(lease(0)), Err(Skip::NoBooking));
    assert!(learned.booking(lease(STARTED_CAP as u64)).is_ok());
}

/// Catches: a staged attempt observed though the scheduler did not take its report, one
/// taken but not observed, and a stage that outlives its answer.
#[test]
fn a_report_the_scheduler_did_not_take_teaches_nothing() {
    let mut learned = shadow();
    learned.stage(lease(1), Some(success(MIB)));
    assert_eq!(learned.finish(lease(1), false, &bounds()), Vec::new());
    assert_eq!(learned.calls(), 0);
    learned.stage(lease(2), Some(success(MIB)));
    assert_eq!(learned.finish(lease(2), true, &bounds()), Vec::new());
    assert_eq!(learned.calls(), 1);
    learned.finish(lease(2), true, &bounds());
    assert_eq!(learned.calls(), 1);
}

/// Catches: the rows of a kill's requeue holding another key's summary (every dirty key
/// rides one transaction), the rows of the keys a kill changed left to the write-behind,
/// and the kill's raised booking lost (the floor must be above the booking it died at).
#[test]
fn a_kills_requeue_carries_the_two_summaries_it_changed_and_no_others() {
    let mut learned = shadow();
    let other = key_of("other", &linux(), Basis::Cgroup, &subject(None)).expect("a key");
    learned.observe(
        &Attempt {
            key: other.clone(),
            run: success(MIB).run,
        },
        &bounds(),
    );
    let with_cfg = key_of("main", &linux(), Basis::Cgroup, &subject(Some("7a3c"))).expect("a key");
    let mut hit = kill(Some(5 * GIB));
    hit.key = with_cfg.clone();
    learned.stage(lease(1), Some(hit));
    let rows = learned.requeued(lease(1), 16 * GIB, &bounds());
    let keys: Vec<&Key> = rows.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, [&key(), &with_cfg]);
    let floor = learned.summaries().key_floor(&with_cfg, &bounds());
    assert!(floor >= 16 * GIB, "floor {floor}");
    // The other key is still waiting for the write-behind.
    learned.clock(FarmTime::from_millis(FLUSH_EVERY_MS));
    let due: Vec<Key> = learned.due().into_iter().map(|(k, _)| k).collect();
    assert_eq!(due, [other]);
    // A requeue with nothing staged (another lease) carries nothing.
    assert_eq!(learned.requeued(lease(9), 16 * GIB, &bounds()), Vec::new());
}

/// Catches: dirty keys written at every tick (a write per result), or never; a full
/// batch made to wait; a batch larger than the bound; and keys a refused write dropped
/// instead of written again.
#[test]
fn dirty_keys_are_written_behind_in_batches() {
    let mut learned = shadow();
    learned.observe(&success(MIB), &bounds());
    learned.clock(FarmTime::from_millis(FLUSH_EVERY_MS - 1));
    assert_eq!(learned.due(), Vec::new());
    learned.clock(FarmTime::from_millis(FLUSH_EVERY_MS));
    let rows = learned.due();
    assert_eq!(rows.len(), 1);
    assert_eq!(learned.due(), Vec::new());
    learned.unwritten(&rows);
    learned.clock(FarmTime::from_millis(2 * FLUSH_EVERY_MS));
    assert_eq!(learned.due(), rows);

    // A full batch does not wait for the interval, and a batch is bounded.
    for shape in 0..(BATCH as u64 + 10) {
        let key = Key::new(
            Instance::new("main"),
            PlatformClass::new("linux"),
            Basis::Cgroup,
            Shape(shape + 1),
            None,
        );
        learned.observe(
            &Attempt {
                key,
                run: success(MIB).run,
            },
            &bounds(),
        );
    }
    assert_eq!(learned.due().len(), BATCH);
}

/// Catches: `due_now` waiting for the write-behind interval like `due` (a test or a
/// shutdown left with unwritten summaries), and `due_now` leaving the keys dirty or
/// handing the same rows over twice.
#[test]
fn due_now_hands_over_the_changed_summaries_before_the_interval_has_passed() {
    let mut learned = shadow();
    learned.observe(&success(MIB), &bounds());
    learned.clock(FarmTime::from_millis(FLUSH_EVERY_MS - 1));
    assert_eq!(learned.due(), Vec::new(), "the interval has not passed");
    let rows = learned.due_now();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, key());
    assert_eq!(learned.due_now(), Vec::new(), "nothing changed since");
    learned.clock(FarmTime::from_millis(2 * FLUSH_EVERY_MS));
    assert_eq!(learned.due(), Vec::new(), "nor later");
    assert_eq!(shadow().due_now(), Vec::new(), "nothing was ever changed");
}

/// Catches: a sweep at start (nothing is stale yet), none ever, one at every tick, and a
/// sweep that leaves stale summaries in memory.
#[test]
fn stale_rows_are_swept_an_hour_after_the_last_look() {
    let mut learned = shadow();
    learned.observe(&success(MIB), &bounds());
    assert_eq!(learned.sweep_due(), None);
    learned.clock(FarmTime::from_millis(SWEEP_EVERY_MS - 1));
    assert_eq!(learned.sweep_due(), None);
    learned.clock(FarmTime::from_millis(SWEEP_EVERY_MS));
    let day = learned.sweep_due().expect("due");
    assert_eq!(day, Day(20_000));
    learned.swept(false);
    assert_eq!(learned.sweep_due(), None);
    // 61 days on, the summary is stale and the sweep takes it out of memory.
    learned.clock(FarmTime::from_millis(61 * DAY));
    assert!(learned.sweep_due().is_some());
    assert!(learned.summaries().is_empty());
}

/// Catches: the day taken from a clock (a restart would put the same result on another
/// day), farm time that runs backward, and a day count that wraps.
#[test]
fn the_day_is_the_wall_time_the_farm_was_built_at_plus_farm_time() {
    let mut learned = shadow();
    learned.clock(FarmTime::from_millis(DAY));
    assert_eq!(learned.day(), Day(20_001));
    learned.clock(FarmTime::from_millis(5));
    assert_eq!(learned.day(), Day(20_001));
    learned.observe(&success(MIB), &bounds());
    let seen = learned
        .summaries()
        .get(&key())
        .expect("a summary")
        .last_seen();
    assert_eq!(seen, Day(20_001));
    let far = Learned::new(LearnedSizes::Shadow, GIB, u64::MAX);
    assert_eq!(far.day(), Day(u32::MAX));
}

/// Catches: an own-limit kill that ended the operation (no requeue) left to the
/// write-behind, so that a crash in the next seconds loses the floor it set; and a
/// success returning rows (only a kill has safety state to hurry).
#[test]
fn a_final_kill_hands_back_the_rows_it_changed() {
    let mut learned = shadow();
    let mut hit = kill(Some(5 * GIB));
    hit.run.raised_memory_bytes = Some(16 * GIB);
    learned.stage(lease(1), Some(hit.clone()));
    let rows = learned.finish(lease(1), true, &bounds());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, key());
    // Written: not waiting for the write-behind.
    learned.clock(FarmTime::from_millis(FLUSH_EVERY_MS));
    assert_eq!(learned.due(), Vec::new());
    learned.stage(lease(2), Some(success(MIB)));
    assert_eq!(learned.finish(lease(2), true, &bounds()), Vec::new());
}

/// Catches: a kill with no usage block given a peak it did not measure, or a limit other
/// than the one it died at: it is a sample of at least `memory_max(booked)`.
#[test]
fn a_kill_with_no_usage_died_at_its_limit() {
    let run = kill_run(4 * GIB);
    assert_eq!(run.outcome, RunOutcome::OwnLimitKill);
    assert_eq!(
        (run.peak_memory_bytes, run.booked_memory_bytes),
        (None, 4 * GIB)
    );
    let mut learned = shadow();
    let seen = learned.observe(&Attempt { key: key(), run }, &bounds());
    assert_eq!(seen, Some((ShadowLevel::None, ShadowBucket::Cold)));
    let sample = learned
        .summaries()
        .get(&key())
        .expect("a summary")
        .long_max();
    assert_eq!(sample, memory_max(4 * GIB).expect("a limit"));
}

/// Catches: a sweep that forgets it has more to do (a backlog waits an hour per slice),
/// one that keeps going once it is done, and a new sweep started while one is going.
#[test]
fn a_sweep_with_more_to_do_goes_on_at_the_next_tick() {
    let mut learned = shadow();
    assert_eq!(learned.sweep_due(), None);
    learned.clock(FarmTime::from_millis(SWEEP_EVERY_MS));
    let day = learned.sweep_due().expect("due");
    learned.swept(true);
    learned.clock(FarmTime::from_millis(SWEEP_EVERY_MS + 1_000));
    assert_eq!(learned.sweep_due(), Some(day));
    learned.swept(false);
    assert_eq!(learned.sweep_due(), None);
}

// ----------------------------------------------------------- key floor decay

/// A farm in `on` mode that has seen an own-limit kill of the key: the estimator's floor
/// for it is 16 GiB, and the operation of every attempt below has size key `SIZE_KEY`.
const SIZE_KEY: u64 = 77;

fn killed_once() -> Learned {
    let mut learned = Learned::new(LearnedSizes::On, 4 * GIB, EPOCH);
    learned.register(&key(), SIZE_KEY);
    learned.stage(lease(1), Some(kill(None)));
    learned.requeued(lease(1), 16 * GIB, &bounds());
    assert!(learned.take_lowered().is_empty(), "a kill lowers nothing");
    assert_eq!(l2_floor(&learned), 16 * GIB);
    learned
}

fn l2_floor(learned: &Learned) -> u64 {
    learned
        .summaries()
        .get(&key().l2())
        .map_or(0, kbf_estimator::Summary::oom_floor)
}

/// Shows `learned` one finished success of `peak` bytes that was booked at `booked`, as
/// the farm does for a result it took.
fn run_success(learned: &mut Learned, seq: u64, peak: u64, booked: u64) {
    let (_, run) = run_of(ok(), Some(&cgroup(Some(peak))), booked).expect("a run");
    learned.stage(lease(seq), Some(Attempt { key: key(), run }));
    learned.finish(lease(seq), true, &bounds());
}

fn at_day(learned: &mut Learned, days: u64) {
    learned.clock(FarmTime::from_millis(days * DAY));
}

/// A floor holds while the heavy mode recurs: a run above half the floor every 30 runs,
/// for 400 runs over ten days, never lowers it.
#[test]
fn a_key_floor_holds_while_heavy_runs_recur() {
    let mut learned = killed_once();
    for seq in 2..402 {
        at_day(&mut learned, (seq - 2) / 40);
        let peak = if seq % 30 == 0 { 9 * GIB } else { 500 * MIB };
        run_success(&mut learned, seq, peak, 16 * GIB);
        assert!(learned.take_lowered().is_empty(), "lowered at run {seq}");
    }
    assert_eq!(l2_floor(&learned), 16 * GIB);
}

/// A floor halves once its key has run 256 times and 7 days have passed without a run near
/// the heavy mode, and only then: not at run 255, and once at run 256.
#[test]
fn a_key_floor_halves_after_256_quiet_runs_and_7_days() {
    let mut learned = killed_once();
    at_day(&mut learned, 8);
    for seq in 2..257 {
        run_success(&mut learned, seq, 500 * MIB, 16 * GIB);
        assert!(learned.take_lowered().is_empty(), "lowered at run {seq}");
    }
    run_success(&mut learned, 257, 500 * MIB, 16 * GIB);
    assert_eq!(learned.take_lowered(), vec![(SIZE_KEY, 16 * GIB, 8 * GIB)]);
    assert_eq!(l2_floor(&learned), 8 * GIB);
    // And once: the next quiet run lowers nothing more.
    run_success(&mut learned, 258, 500 * MIB, 16 * GIB);
    assert!(learned.take_lowered().is_empty());
}

/// Neither condition is enough alone: 300 quiet runs within a day, and 100 quiet runs
/// after 8 days, leave the floor.
#[test]
fn a_key_floor_does_not_halve_before_both_256_runs_and_7_days() {
    let mut learned = killed_once();
    for seq in 2..302 {
        run_success(&mut learned, seq, 500 * MIB, 16 * GIB);
    }
    assert!(learned.take_lowered().is_empty(), "300 runs in one day");
    let mut learned = killed_once();
    at_day(&mut learned, 30);
    for seq in 2..102 {
        run_success(&mut learned, seq, 500 * MIB, 16 * GIB);
    }
    assert!(learned.take_lowered().is_empty(), "100 runs in 30 days");
    assert_eq!(l2_floor(&learned), 16 * GIB);
}

/// A floor that decays down to the first rung or below is reported as 0: the key keeps no
/// floor that books less than the default.
#[test]
fn a_decayed_key_floor_is_never_kept_below_the_default() {
    let mut learned = killed_once();
    let mut kept = Vec::new();
    let mut seq = 2;
    for round in 1..=3 {
        at_day(&mut learned, 8 * round);
        for _ in 0..257 {
            run_success(&mut learned, seq, 500 * MIB, 16 * GIB);
            seq += 1;
            kept.extend(learned.take_lowered().into_iter().map(|(_, _, ask)| ask));
        }
    }
    // 16 -> 8 GiB is kept; 4 GiB is the default and 2 GiB is below it: nothing is kept.
    assert_eq!(kept, vec![8 * GIB, 0, 0]);
}

/// After a decay a kill raises the floor again, and the estimator reports no lowering for
/// it.
#[test]
fn a_kill_after_a_key_floor_decayed_raises_it_again() {
    let mut learned = killed_once();
    at_day(&mut learned, 8);
    for seq in 2..258 {
        run_success(&mut learned, seq, 500 * MIB, 16 * GIB);
    }
    assert_eq!(learned.take_lowered(), vec![(SIZE_KEY, 16 * GIB, 8 * GIB)]);
    learned.stage(lease(300), Some(kill(None)));
    learned.requeued(lease(300), 16 * GIB, &bounds());
    assert_eq!(l2_floor(&learned), 16 * GIB);
    assert!(learned.take_lowered().is_empty());
}

/// The decay is reported for the key whichever attempt of it crosses the threshold: the
/// attempts here carry no size key of their own (an attempt whose client sized its memory
/// is never keyed), and the floor of the keyed operation registered under the same
/// estimator key is the one lowered.
#[test]
fn a_decay_caused_by_an_unkeyed_attempt_lowers_the_keyed_floor() {
    let mut learned = killed_once();
    // Another keyed operation under the same key, and one under another key.
    learned.register(&key(), SIZE_KEY + 1);
    let (other, _) = Subject::of(Described {
        arguments: vec!["link".to_owned()],
        ..Described::default()
    });
    let other = key_of("main", &linux(), Basis::Cgroup, &other).expect("a key");
    learned.register(&other, SIZE_KEY + 2);
    at_day(&mut learned, 8);
    for seq in 2..258 {
        run_success(&mut learned, seq, 500 * MIB, 16 * GIB);
    }
    let mut lowered = learned.take_lowered();
    lowered.sort_unstable();
    assert_eq!(
        lowered,
        vec![
            (SIZE_KEY, 16 * GIB, 8 * GIB),
            (SIZE_KEY + 1, 16 * GIB, 8 * GIB)
        ]
    );
}

/// Only `on` keeps the registry: with `shadow` a decay reports nothing.
#[test]
fn only_on_reports_a_decay() {
    let mut learned = Learned::new(LearnedSizes::Shadow, 4 * GIB, EPOCH);
    learned.register(&key(), SIZE_KEY);
    learned.stage(lease(1), Some(kill(None)));
    learned.requeued(lease(1), 16 * GIB, &bounds());
    at_day(&mut learned, 8);
    for seq in 2..258 {
        run_success(&mut learned, seq, 500 * MIB, 16 * GIB);
    }
    assert_eq!(l2_floor(&learned), 8 * GIB, "the estimator still decays");
    assert!(learned.take_lowered().is_empty());
}

/// The registry of size keys is bounded: past the cap the estimator key registered first
/// is forgotten with its size keys, and the newest stay.
#[test]
fn the_registry_of_size_keys_is_bounded_and_forgets_the_oldest() {
    let mut learned = Learned::new(LearnedSizes::On, 4 * GIB, EPOCH);
    let shaped = |n: u64| {
        Key::new(
            Instance::new("main"),
            PlatformClass::new("linux"),
            Basis::Cgroup,
            Shape(n),
            None,
        )
    };
    for n in 0..=STARTED_CAP as u64 {
        learned.register(&shaped(n), n);
    }
    assert!(learned.named(&shaped(0)).is_empty(), "the oldest is gone");
    assert_eq!(learned.summary_key(0), None);
    let newest = STARTED_CAP as u64;
    assert_eq!(learned.named(&shaped(newest)), vec![newest]);
    assert_eq!(learned.summary_key(newest), Some(shaped(newest)));
    assert_eq!(learned.named(&shaped(1)), vec![1]);
}

/// Pins that the live estimator learns from the lease's `ResourceUsage` block and from no
/// timestamp of the execution metadata: a run that carries no usage (an executor run, whose
/// phase times are a task's claim) is never a sample, whatever its outcome.
#[test]
fn a_run_without_a_usage_block_is_never_a_sample() {
    assert!(run_of(ok(), None, GIB).is_none());
    assert!(run_of(Outcome::Failed(Failure::OutOfMemory), None, GIB).is_none());
}
