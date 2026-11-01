//! The server's shadow (`kbf_server::learned`) and the replay learn from the same
//! ordered records and answer alike. The server hears of an attempt as its result
//! commits and builds the estimator's `Run` from the lease's usage block; the replay
//! builds it from the exported row. This test feeds both the same attempts, in the order
//! their results committed (by end time), and holds their estimates and floors equal
//! after every one.

use kbf_estimator::{
    Basis, Bounds, Day, Estimate, Fallback, GIB, Hint, Key, MIB, Summaries, memory_max,
};
use kbf_estimator_replay::replay::sim::attempt_run;
use kbf_estimator_replay::replay::trace::key_of;
use kbf_estimator_replay::row::{Row, Usage};
use kbf_invocation::{RunUsage, UsageBasis};
use kbf_server::action_usage::{ShapeKey, Subject};
use kbf_server::learned::{Attempt, Learned, LearnedSizes, run_of};
use kbf_server::machine::Platform;
use kbf_types::{Failure, FarmTime, Outcome};

const DAY_MS: u64 = 86_400_000;

/// One attempt of an action, as both sides are told of it.
#[derive(Clone, Debug)]
struct Rec {
    instance: &'static str,
    cfg: &'static str,
    shape: u64,
    /// What the attempt needed, and what it was booked at.
    need: u64,
    booked: u64,
    kill: bool,
    raised: Option<u64>,
    run_ms: u64,
    ended_ms: u64,
}

impl Rec {
    fn queued_ms(&self) -> u64 {
        self.ended_ms - self.run_ms
    }

    /// The row the exporter writes for it.
    fn row(&self) -> Row {
        Row {
            run_id: "0".repeat(24),
            instance: self.instance.to_owned(),
            platform: "linux".to_owned(),
            node: "n1".to_owned(),
            shape: format!("{:016x}", self.shape),
            cfg: self.cfg.to_owned(),
            digest: "-".to_owned(),
            outcome: if self.kill { "memory_killed" } else { "ok" }.to_owned(),
            memory_kill: if self.kill { "own_limit" } else { "none" }.to_owned(),
            booked_memory_bytes: Some(self.booked),
            raised_memory_bytes: self.raised,
            memory_used_bytes: None,
            cgroup: Some(Usage::default()),
            queued_us: self.queued_ms() * 1_000,
            started_us: None,
            ended_us: Some(self.ended_ms * 1_000),
            node_capacity: None,
            basis: Some("cgroup".to_owned()),
            operation: None,
            booked_millicpus: None,
            node_cpu_pressure_centi: None,
            invocation_id: None,
            correlated_invocations_id: None,
            phase_times: None,
            fetch_us: None,
            execution_us: None,
            pack_us: None,
            lease: None,
        }
    }

    /// The usage block the daemon reported for it.
    fn usage(&self) -> RunUsage {
        RunUsage {
            wall_micros: self.run_ms * 1_000,
            basis: UsageBasis::Cgroup,
            memory_peak_bytes: Some(self.need.min(memory_max(self.booked).unwrap_or(0))),
            cpu_usage_micros: Some(0),
            ..RunUsage::default()
        }
    }
}

fn bounds() -> Bounds {
    Bounds {
        default_memory: 4 * GIB,
        host_memory: 64 * GIB,
        node_cpu_millis: 8_000,
    }
}

/// A fixed, mixed history: three actions (one in two configurations) over five days,
/// quiet runs of growing size, a run that ran at its limit, two kills within a few runs
/// (the second makes the key raise-only), a heavy mode, and short runs.
fn history() -> Vec<Rec> {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let mut recs = Vec::new();
    for i in 0..160_u64 {
        let (instance, cfg, shape) = match i % 5 {
            0 | 1 => ("main", "c1", 0xa1),
            2 => ("main", "c2", 0xa1),
            3 => ("main", "-", 0xb2),
            _ => ("other", "c1", 0xa1),
        };
        let need = (100 + next(900) + i) * MIB;
        let run_ms = 1_000 + next(60_000);
        // Longer runs end later than the shorter ones queued after them.
        let ended_ms = i * 3_000 * 60 + next(4_000_000) + DAY_MS * (i / 40);
        let mut rec = Rec {
            instance,
            cfg,
            shape,
            need,
            booked: 4 * GIB,
            kill: false,
            raised: None,
            run_ms,
            ended_ms: ended_ms.max(run_ms),
        };
        match i {
            // Ran at its limit: only a lower bound.
            30 | 31 => rec.need = 6 * GIB + 400 * MIB,
            // Two kills of one key, a few runs apart.
            50 | 60 => {
                rec.need = 9 * GIB;
                rec.kill = true;
                rec.raised = Some(16 * GIB);
            }
            // A heavy mode that fits the raised booking.
            70..=72 => {
                rec.need = 12 * GIB;
                rec.booked = 16 * GIB;
            }
            // Too short to be a sample.
            80 | 81 => rec.run_ms = 100,
            _ => {}
        }
        recs.push(rec);
    }
    recs
}

fn by_end(recs: &[Rec]) -> Vec<Rec> {
    let mut sorted = recs.to_vec();
    sorted.sort_by_key(|r| (r.ended_ms, r.shape, r.cfg));
    sorted
}

/// The order the attempts were queued in: what a learner that listened at submit would
/// have heard them in.
fn by_queue(recs: &[Rec]) -> Vec<Rec> {
    let mut sorted = recs.to_vec();
    sorted.sort_by_key(|r| (r.queued_ms(), r.shape, r.cfg));
    sorted
}

/// What the server's shadow is told of `rec`: the key from the request's identity and the
/// run from the usage block.
fn server_attempt(rec: &Rec) -> Attempt {
    let platform = Platform {
        os_family: "linux",
        isa: "-".to_owned(),
    };
    let subject = Subject {
        key: ShapeKey::from_bytes(rec.shape.to_be_bytes()),
        cfg: (rec.cfg != "-").then(|| rec.cfg.to_owned()),
        target: None,
        test: false,
        label: Default::default(),
    };
    let key = kbf_server::learned::key_of(rec.instance, &platform, Basis::Cgroup, &subject)
        .expect("the request pins the OS");
    let outcome = if rec.kill {
        Outcome::Failed(Failure::OutOfMemory)
    } else {
        Outcome::Completed {
            action_result: kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 1)
                .into(),
        }
    };
    let (basis, mut run) = run_of(outcome, Some(&rec.usage()), rec.booked).expect("a run");
    assert_eq!(basis, Basis::Cgroup);
    run.raised_memory_bytes = rec.raised;
    Attempt { key, run }
}

type Answer = (Result<Estimate, Fallback>, u64);

/// What each side answers for `key` once it has heard `heard` records.
struct Sides {
    server: Learned,
    replay: Summaries,
    today: Day,
}

impl Sides {
    fn new() -> Self {
        Self {
            server: Learned::new(LearnedSizes::Shadow, 4 * GIB, 0),
            replay: Summaries::default(),
            today: Day(0),
        }
    }

    /// Both sides hear of `rec` as its result commits.
    fn hear(&mut self, server: &Rec, replay: &Rec) {
        self.server.clock(FarmTime::from_millis(server.ended_ms));
        self.server.observe(&server_attempt(server), &bounds());
        self.today = Day(u32::try_from(replay.ended_ms / DAY_MS).expect("a day"));
        let key = key_of(&replay.row()).expect("a shape");
        let run = attempt_run(
            replay.need,
            0,
            replay.run_ms * 1_000,
            replay.booked,
            replay.kill,
            replay.raised,
        );
        self.replay.observe(&key, &run, self.today, &bounds());
    }

    /// The stored form of `key`'s summary on each side: the order samples came in shows.
    fn rows(&self, key: &Key) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let bytes = |s: &Summaries| s.get(key).map(kbf_estimator::Summary::encode);
        (bytes(self.server.summaries()), bytes(&self.replay))
    }

    fn asked(&self, key: &Key, today: Day) -> (Answer, Answer) {
        let ask = |s: &Summaries| {
            (
                s.estimate(key, today, &bounds(), &Hint::default()),
                s.key_floor(key, &bounds()),
            )
        };
        (ask(self.server.summaries()), ask(&self.replay))
    }
}

fn keys(recs: &[Rec]) -> Vec<Key> {
    let mut keys: Vec<Key> = recs.iter().filter_map(|r| key_of(&r.row())).collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Catches: the server's adapter and the replay's reading one record two ways: a
/// different key (instance, platform, basis, shape or configuration hash), a different
/// run (outcome, peak, wall time, booking, raise), or a different clock, so that the
/// estimator answers differently for the same history. The second half shows the history
/// is sensitive to order: the same records heard in queue order leave a different stored
/// summary. (That the farm itself hears results in commit order is held by
/// `learned_sizes.rs` in kbf-server, through a daemon.)
#[test]
fn the_server_and_the_replay_answer_alike_for_the_same_ordered_records() {
    let recs = history();
    let committed = by_end(&recs);
    let keys = keys(&recs);
    let mut sides = Sides::new();
    let mut answered = 0;
    for rec in &committed {
        sides.hear(rec, rec);
        for key in &keys {
            let (server, replay) = sides.asked(key, sides.today);
            assert_eq!(
                server, replay,
                "after the run ending at {} ms, {key:?}",
                rec.ended_ms
            );
            answered += usize::from(server.0.is_ok());
            let (a, b) = sides.rows(key);
            assert_eq!(a, b, "stored form after {} ms", rec.ended_ms);
        }
    }
    // The history is rich enough to have exercised the estimator: estimates were given,
    // and the floors of the kills were set.
    assert!(answered > 100, "{answered} answers");
    for key in &keys {
        let (server, _) = sides.asked(key, sides.today);
        if key.cfg().is_none() || key.instance().as_str() == "main" {
            assert!(
                server.0.is_ok() || server.0 == Err(Fallback::OverCap),
                "{key:?}"
            );
        }
    }
    let floors: Vec<u64> = keys
        .iter()
        .map(|k| sides.asked(k, sides.today).0.1)
        .collect();
    assert!(floors.iter().any(|f| *f >= 16 * GIB), "{floors:?}");

    // The same records in queue order are heard by the server as a different history: the
    // test is able to see the difference.
    let mut early = Sides::new();
    let mut differs = false;
    for (queued, ended) in by_queue(&recs).iter().zip(&committed) {
        early.hear(queued, ended);
        differs |= keys.iter().any(|key| {
            let (server, replay) = early.asked(key, early.today);
            let (a, b) = early.rows(key);
            server != replay || a != b
        });
    }
    assert!(
        differs,
        "queue order and commit order gave the same answers"
    );
}

/// Catches: the two sides' keys differing for one action (an instance, platform class or
/// configuration hash built another way), two tenants or two architectures sharing a key,
/// and a request that does not pin the OS being given one.
#[test]
fn the_keys_of_the_two_sides_agree_and_separate_what_they_must() {
    let rec = &history()[0];
    let server = server_attempt(rec).key;
    assert_eq!(Some(server.clone()), key_of(&rec.row()));
    // An empty configuration is no configuration on both sides.
    let empty = Rec {
        cfg: "",
        ..rec.clone()
    };
    let none = Rec {
        cfg: "-",
        ..rec.clone()
    };
    assert_eq!(key_of(&empty.row()), key_of(&none.row()));
    assert_eq!(server_attempt(&empty).key, server_attempt(&none).key);
    let other = Rec {
        instance: "elsewhere",
        ..rec.clone()
    };
    assert_ne!(server_attempt(&other).key, server);
    let subject = Subject {
        key: ShapeKey::from_bytes(rec.shape.to_be_bytes()),
        cfg: Some(rec.cfg.to_owned()),
        target: None,
        test: false,
        label: Default::default(),
    };
    let arm = Platform {
        os_family: "linux",
        isa: "arm64".to_owned(),
    };
    let on_arm = kbf_server::learned::key_of(rec.instance, &arm, Basis::Cgroup, &subject);
    assert_ne!(on_arm, Ok(server));
    let unpinned = Platform {
        os_family: "-",
        isa: "-".to_owned(),
    };
    assert_eq!(
        kbf_server::learned::key_of(rec.instance, &unpinned, Basis::Cgroup, &subject),
        Err(kbf_server::learned::Skip::NoOs)
    );
}
