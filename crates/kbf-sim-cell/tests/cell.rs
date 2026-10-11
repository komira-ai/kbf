//! The one-server cell over seed sweeps (crates/kbf-sim-cell; docs/design/ha.md
//! section 15). Each run checks properties 1 to 7 after every step and panics with
//! the kind, seed and property on a violation; `replay` reruns one seed.

use kbf_sim_cell::{Kind, Scenario, run};

const SEEDS: u64 = 64;

fn sweep(kind: Kind) {
    for seed in 0..SEEDS {
        let report = run(&Scenario::new(kind, seed));
        // Not vacuous: the server answered, and results were accepted.
        assert!(report.present > 0, "{kind:?} seed {seed}: {report:?}");
        assert!(report.accepted > 0, "{kind:?} seed {seed}: {report:?}");
        if !matches!(kind, Kind::Calm | Kind::Partition) {
            assert!(
                report.starts > 1,
                "{kind:?} seed {seed} never restarted: {report:?}"
            );
        }
    }
}

/// Catches any defect that breaks a property with only the network's faults, all
/// seven enforced.
#[test]
fn calm_seeds_keep_every_property() {
    sweep(Kind::Calm);
}

/// Catches a restart that loses or repeats an applied entry (property 1), refuses
/// its own log (7, the mutant "a lost Bootstrap log id"), or answers a blob present
/// that the store lacks (5).
#[test]
fn restart_seeds_keep_properties_1_2_5_6_7() {
    sweep(Kind::Restart);
}

/// Catches an entry applied before it is durable, or a sync that reports durable
/// what is not (property 1: the mutants "apply before commit" and "ack before
/// fsync"), across power cuts now and part-way through a commit.
#[test]
fn crash_seeds_keep_properties_1_2_5_6_7() {
    sweep(Kind::Crash);
}

/// Catches an entry applied although its sync failed (property 1, read while the
/// stopped log still serves reads).
#[test]
fn io_error_seeds_keep_properties_1_2_5_6_7() {
    sweep(Kind::IoError);
}

/// Catches a daemon that does not fence once it has lost contact (property 2: its
/// run goes on beside the run the scheduler grants again after G), and a lease given
/// up while its daemon was still heard (3).
#[test]
fn partition_seeds_keep_properties_1_2_3_5_6_7() {
    sweep(Kind::Partition);
}

/// Catches a cell that reads a clock, a random source or a hashed map's order: one
/// seed replays to one trace, and another seed gives another.
#[test]
fn a_seed_replays_to_the_same_trace() {
    let a = run(&Scenario::new(Kind::Crash, 3)).trace;
    assert_eq!(a, run(&Scenario::new(Kind::Crash, 3)).trace);
    assert_ne!(a, run(&Scenario::new(Kind::Crash, 4)).trace);
}

/// Reruns one seed: `KBF_SIM_KIND=Crash KBF_SIM_SEED=7 cargo test -p kbf-sim-cell
/// --test cell -- --ignored replay`.
#[test]
#[ignore = "run by hand to replay one seed"]
fn replay() {
    let kind = match std::env::var("KBF_SIM_KIND").as_deref() {
        Ok("Restart") => Kind::Restart,
        Ok("Crash") => Kind::Crash,
        Ok("IoError") => Kind::IoError,
        Ok("Partition") => Kind::Partition,
        _ => Kind::Calm,
    };
    let seed = std::env::var("KBF_SIM_SEED").map_or(0, |s| s.parse().expect("a seed"));
    println!("{:?}", run(&Scenario::new(kind, seed)));
}
