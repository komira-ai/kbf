//! The one-server cell over seed sweeps (crates/kbf-sim-cell; docs/design/ha.md
//! section 15). Each run checks properties 1 and 5 to 7 after every step and panics
//! with the kind, seed and property on a violation; `replay` reruns one seed.

use kbf_sim_cell::{Kind, Scenario, Sweep, run};

const SEEDS: u64 = 64;

fn sweep(kind: Kind) -> Sweep {
    let swept = kbf_sim_cell::sweep(0..SEEDS, |seed| Scenario::new(kind, seed));
    swept.assert_clean();
    for (seed, report) in &swept.reports {
        // Not vacuous: the server answered, and uploads were acknowledged.
        assert!(report.present > 0, "{kind:?} seed {seed}: {report:?}");
        assert!(!report.acked.is_empty(), "{kind:?} seed {seed}: {report:?}");
        if kind.restarts() {
            assert!(
                report.starts > 1,
                "{kind:?} seed {seed} never restarted: {report:?}"
            );
        }
    }
    swept
}

/// Catches any defect that breaks a property with only the network's faults.
#[test]
fn calm_seeds_keep_every_property() {
    sweep(Kind::Calm);
}

/// Catches a restart that loses or repeats an applied entry (property 1), refuses
/// its own log (7, the defect "a lost Bootstrap log id"), or answers a blob present
/// that the store lacks (5).
#[test]
fn restart_seeds_keep_properties_1_5_6_7() {
    sweep(Kind::Restart);
}

/// Catches an entry applied before it is durable, or a sync that reports durable
/// what is not (property 1: the defects "apply before commit" and "ack before
/// fsync"), across power cuts now and part-way through a commit.
#[test]
fn crash_seeds_keep_properties_1_5_6_7() {
    sweep(Kind::Crash);
}

/// Catches an entry applied although its sync failed (property 1, read while the
/// stopped log still serves reads).
#[test]
fn io_error_seeds_keep_properties_1_5_6_7() {
    sweep(Kind::IoError);
}

/// Catches a cell that reads a clock, a random source or a hashed map's order: one
/// seed replays to one trace, and another seed gives another.
#[test]
fn a_seed_replays_to_the_same_trace() {
    let a = run(&Scenario::new(Kind::Crash, 3)).trace;
    assert_eq!(a, run(&Scenario::new(Kind::Crash, 3)).trace);
    assert_ne!(a, run(&Scenario::new(Kind::Crash, 4)).trace);
}

/// Catches a kind the replay line names but `replay` cannot parse back (it would
/// replay a calm run instead).
#[test]
fn every_kind_parses_from_its_name() {
    for kind in Kind::ALL {
        assert_eq!(Kind::parse(&format!("{kind:?}")), Some(kind));
    }
    assert_eq!(Kind::parse("Stormy"), None);
}

/// Reruns one seed: `KBF_SIM_KIND=Crash KBF_SIM_SEED=7 cargo test -p kbf-sim-cell
/// --test cell -- --ignored replay`; a failure's replay line adds `KBF_SIM_CRASH` when
/// its scenario pinned one crash.
#[test]
#[ignore = "run by hand to replay one seed"]
fn replay() {
    let var = |name| std::env::var(name).ok();
    let kind = var("KBF_SIM_KIND").unwrap_or_else(|| "Calm".to_owned());
    let seed = var("KBF_SIM_SEED").unwrap_or_else(|| "0".to_owned());
    let crash = var("KBF_SIM_CRASH");
    let scenario = Scenario::from_replay(&kind, &seed, crash.as_deref()).expect("a replay line");
    println!("{:?}", run(&scenario));
}
