//! A crash at each step of the write order, a lost race, and a long interleaving: the row
//! never names a file that is not there. The "crash" is a panic at a [`Step`]: nothing
//! after it runs, cleanup included. It proves the order of the steps, not that `fsync`
//! reaches the disk.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use super::*;
use crate::object_store::Step;

const STEPS: [Step; 5] = [
    Step::WriteTemp,
    Step::SyncTemp,
    Step::Rename,
    Step::SyncDir,
    Step::CommitRow,
];

/// What a crash at `step` leaves besides the live files, before any sweep: temp files
/// and files under their versioned name that no row names.
fn leftovers(step: Step) -> (usize, usize) {
    match step {
        Step::WriteTemp | Step::ReadFile => (0, 0),
        Step::SyncTemp | Step::Rename => (1, 0),
        Step::SyncDir | Step::CommitRow => (0, 1),
    }
}

/// The temp files and the versioned files in `dir`.
fn counts(dir: &Path) -> (usize, usize) {
    let all = listing(dir);
    let temps = all.iter().filter(|n| n.starts_with(".tmp-")).count();
    (temps, all.len() - temps)
}

fn crash_during(reg: &Registry, step: Step, call: impl FnOnce()) {
    reg.set_failpoint(move |at| {
        assert!(at != step, "crash at {at:?}");
    });
    let outcome = catch_unwind(AssertUnwindSafe(call));
    assert!(
        outcome.is_err(),
        "the failpoint at {step:?} was never reached"
    );
    reg.set_failpoint(|_| {});
}

// Catches a write order in which the row can name a file that is not there: a row
// committed before the file (crash at any step leaves a dangling row), and an old
// version removed before the new row commits (crash at CommitRow leaves no secret).
// Also: a leftover must exist where the order says it does, and the restart sweeps it.
#[test]
fn a_crash_at_any_step_of_a_first_write_leaves_no_row_and_no_file_after_the_sweep() {
    for step in STEPS {
        let store = as_store(&fake());
        let dir = scratch("crash-first").join("s");
        let reg = open(&store, &dir);
        crash_during(&reg, step, || {
            let _ = reg.put_connection(&name("cold"), &put(SENTINEL));
        });
        assert_eq!(counts(&dir), leftovers(step), "{step:?}");
        assert!(
            store.scan_prefix("objstore/").unwrap().is_empty(),
            "{step:?}: no row"
        );
        assert!(reg.connection(&name("cold")).unwrap().is_none(), "{step:?}");
        let restarted = open_now(Arc::clone(&store), &dir, Counting::from(500)).unwrap();
        assert!(listing(&dir).is_empty(), "{step:?}: swept");
        assert_eq!(restarted.generation().unwrap(), 0);
        // And the write can be done again.
        restarted
            .put_connection(&name("cold"), &put(SENTINEL))
            .unwrap();
        assert_eq!(restarted.secret(&name("cold")).unwrap().expose(), SENTINEL);
    }
}

#[test]
fn a_crash_at_any_step_of_a_rotation_keeps_the_live_version_readable() {
    for step in STEPS {
        let store = as_store(&fake());
        let dir = scratch("crash-rotate").join("s");
        let reg = open(&store, &dir);
        reg.put_connection(&name("cold"), &put("live-secret"))
            .unwrap();
        crash_during(&reg, step, || {
            let _ = reg.put_connection(&name("cold"), &put("next-secret"));
        });
        // Before any restart: the row still names version 1 and its file is there.
        assert_eq!(
            reg.secret(&name("cold")).unwrap().expose(),
            "live-secret",
            "{step:?}"
        );
        let (temps, named) = leftovers(step);
        assert_eq!(counts(&dir), (temps, 1 + named), "{step:?}");
        assert_eq!(reg.generation().unwrap(), 1, "{step:?}");
        let restarted = open_now(Arc::clone(&store), &dir, Counting::from(500)).unwrap();
        assert_eq!(
            listing(&dir),
            ["cold.0000000000000001.json"],
            "{step:?}: swept"
        );
        assert_eq!(
            restarted.secret(&name("cold")).unwrap().expose(),
            "live-secret"
        );
        let audit = restarted
            .audit(std::num::NonZeroUsize::new(9).unwrap())
            .unwrap();
        assert_eq!(audit.len(), 1, "{step:?}: a crashed write has no audit row");
        // The rotation can be done again on the restarted registry.
        restarted
            .put_connection(&name("cold"), &put("next-secret"))
            .unwrap();
        assert_eq!(
            restarted.secret(&name("cold")).unwrap().expose(),
            "next-secret"
        );
        assert_eq!(listing(&dir).len(), 1);
    }
}

// Catches a commit without the compare-and-set on the generation: a writer that read the
// registry before another one wrote must lose, not overwrite, and its file must go.
#[test]
fn a_writer_that_lost_a_race_writes_nothing_and_cleans_up() {
    let store = as_store(&fake());
    let dir = scratch("race").join("s");
    let loser = open(&store, &dir);
    let winner = Arc::new(open_now(Arc::clone(&store), &dir, Counting::from(100)).unwrap());
    let racer = Arc::clone(&winner);
    loser.set_failpoint(move |at| {
        if at == Step::CommitRow {
            racer
                .put_connection(&name("cold"), &put("winner-secret"))
                .unwrap();
        }
    });
    let err = loser
        .put_connection(&name("cold"), &put("loser-secret"))
        .unwrap_err();
    assert!(matches!(err, RegistryError::Conflict), "{err}");
    assert_eq!(
        err.to_string(),
        "the registry changed during the call; retry"
    );
    assert_eq!(
        winner.secret(&name("cold")).unwrap().expose(),
        "winner-secret"
    );
    assert_eq!(listing(&dir).len(), 1, "the loser's file is gone");
    assert_eq!(winner.generation().unwrap(), 1);
    assert_eq!(
        winner
            .audit(std::num::NonZeroUsize::new(9).unwrap())
            .unwrap()
            .len(),
        1
    );
}

// Catches an unchanged write that skips the compare-and-set (it still appends an audit
// row, so it must lose to a concurrent writer too, or two rows would share a sequence).
#[test]
fn an_unchanged_write_also_loses_a_race() {
    let store = as_store(&fake());
    let dir = scratch("race-same").join("s");
    let first = open(&store, &dir);
    first.put_connection(&name("cold"), &put("s")).unwrap();
    let other = Arc::new(open_now(Arc::clone(&store), &dir, Counting::from(100)).unwrap());
    let racer = Arc::clone(&other);
    first.set_failpoint(move |at| {
        if at == Step::CommitRow {
            racer.put_connection(&name("cold"), &put("s")).unwrap();
        }
    });
    let err = first.put_connection(&name("cold"), &put("s")).unwrap_err();
    assert!(matches!(err, RegistryError::Conflict), "{err}");
    let audit = other
        .audit(std::num::NonZeroUsize::new(9).unwrap())
        .unwrap();
    let seqs: Vec<u64> = audit.iter().map(|a| a.seq).collect();
    assert_eq!(seqs, [2, 1]);
}

// Catches a lost race that removes the file a change KEPT: a change of the region alone
// writes no file, so when it loses, the live version (still named by the row the winner
// wrote) must stay.
#[test]
fn a_change_that_keeps_the_secret_and_loses_a_race_leaves_the_live_file() {
    let store = as_store(&fake());
    let dir = scratch("race-keep").join("s");
    let loser = open(&store, &dir);
    loser.put_connection(&name("cold"), &put("kept")).unwrap();
    let winner = Arc::new(open_now(Arc::clone(&store), &dir, Counting::from(100)).unwrap());
    let racer = Arc::clone(&winner);
    loser.set_failpoint(move |at| {
        if at == Step::CommitRow {
            let mut other = put("kept");
            other.region = Some("eu-2".to_owned());
            racer.put_connection(&name("cold"), &other).unwrap();
        }
    });
    let mut mine = put("kept");
    mine.region = Some("eu-1".to_owned());
    let err = loser.put_connection(&name("cold"), &mine).unwrap_err();
    assert!(matches!(err, RegistryError::Conflict), "{err}");
    assert_eq!(listing(&dir), ["cold.0000000000000001.json"]);
    assert_eq!(winner.secret(&name("cold")).unwrap().expose(), "kept");
    assert_eq!(
        winner.connection(&name("cold")).unwrap().unwrap().region,
        "eu-2"
    );
}

/// A small deterministic generator (xorshift64*), so a failing seed replays.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }
}

// Catches, over many interleavings of writes, deletes, crashes and restarts, any state
// in which a row names a missing file, a secret differs from the last committed one, or
// a restart leaves a file no row names.
#[test]
fn over_random_interleavings_a_row_always_names_an_existing_file() {
    use std::collections::BTreeMap;
    let names = ["a", "b", "c"];
    for seed in 1..=12u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let store = as_store(&fake());
        let dir = scratch("prop").join("s");
        let mut reg = open(&store, &dir);
        let mut model: BTreeMap<&str, String> = BTreeMap::new();
        let mut last_generation = 0;
        for round in 0..60u64 {
            let n = names[rng.below(3) as usize];
            let secret = format!("secret-{}", rng.below(3));
            match rng.below(5) {
                0 => {
                    if reg.delete_connection(&name(n)).is_ok() {
                        model.remove(n);
                    }
                }
                1 => {
                    // A crash at a step the call never reaches (the same content again
                    // passes no file step) is just a write that finished.
                    let step = STEPS[rng.below(5) as usize];
                    reg.set_failpoint(move |at| assert!(at != step, "crash at {at:?}"));
                    let done = catch_unwind(AssertUnwindSafe(|| {
                        reg.put_connection(&name(n), &put(&secret))
                    }));
                    if matches!(done, Ok(Ok(_))) {
                        model.insert(n, secret);
                    }
                    reg = open_now(
                        Arc::clone(&store),
                        &dir,
                        Counting::from(1_000 * (round + 1)),
                    )
                    .unwrap();
                    assert_eq!(
                        listing(&dir).len(),
                        model.len(),
                        "seed {seed} round {round}: swept"
                    );
                }
                _ => {
                    let mut body = put(&secret);
                    body.region = Some(format!("r{}", rng.below(2)));
                    reg.put_connection(&name(n), &body).unwrap();
                    model.insert(n, secret);
                }
            }
            for (held, secret) in &model {
                let live = reg.secret(&name(held)).unwrap_or_else(|e| {
                    panic!("seed {seed} round {round}: {held} names a missing file: {e}")
                });
                assert_eq!(live.expose(), secret, "seed {seed} round {round}");
            }
            for info in reg.connections().unwrap() {
                assert!(
                    model.contains_key(info.name.as_str()),
                    "seed {seed} round {round}"
                );
            }
            let generation = reg.generation().unwrap();
            assert!(
                generation >= last_generation,
                "seed {seed}: the generation never falls"
            );
            last_generation = generation;
        }
    }
}
