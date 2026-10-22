//! S12: how often the next run exceeds the limit of the estimate, by distribution and
//! by number of samples seen. `EPSILON` is the declared bound, in tenths of a percent;
//! the same table is in the crate docs.

use super::*;

const TRIALS: usize = 10_000;
const COUNTS: [usize; 5] = [3, 4, 5, 10, 32];

/// Peaks are lognormal around 1 GiB; a mixture has a 3% tail four times larger.
#[derive(Clone, Copy)]
struct Dist {
    sigma: f64,
    tail: bool,
}

const DISTS: [Dist; 8] = [
    Dist {
        sigma: 0.1,
        tail: false,
    },
    Dist {
        sigma: 0.3,
        tail: false,
    },
    Dist {
        sigma: 0.6,
        tail: false,
    },
    Dist {
        sigma: 1.0,
        tail: false,
    },
    Dist {
        sigma: 0.1,
        tail: true,
    },
    Dist {
        sigma: 0.3,
        tail: true,
    },
    Dist {
        sigma: 0.6,
        tail: true,
    },
    Dist {
        sigma: 1.0,
        tail: true,
    },
];

fn draw(rng: &mut Rng, dist: Dist) -> u64 {
    let mut peak = GIB as f64 * (dist.sigma * rng.normal()).exp();
    if dist.tail && rng.unit() < 0.03 {
        peak *= 4.0;
    }
    (peak as u64).max(MIB)
}

/// The fraction (in tenths of a percent, rounded up) of trials in which the run after
/// `seen` samples needs more than the limit of the booking the crate gives it.
/// Declared bound on the kill rate in tenths of a percent, per distribution (rows, as
/// `DISTS`) and sample count (columns, as `COUNTS`).
const EPSILON: [[u64; 5]; 8] = [
    [1, 1, 1, 1, 1],
    [1, 1, 2, 2, 1],
    [2, 2, 18, 8, 3],
    [35, 34, 63, 38, 36],
    [1, 1, 30, 27, 12],
    [2, 2, 21, 17, 9],
    [10, 8, 32, 18, 9],
    [43, 41, 68, 45, 43],
];

fn kill_rate(dist: Dist, seen: usize, seed: u64) -> u64 {
    let b = Bounds {
        host_memory: 1 << 40,
        ..Bounds::default()
    };
    let mut rng = Rng(seed);
    let mut kills = 0usize;
    for _ in 0..TRIALS {
        let (mut s, k) = (Summaries::default(), key(Basis::Cgroup, None));
        for _ in 0..seen {
            let run = Run {
                booked_memory_bytes: 1 << 38,
                ..success(draw(&mut rng, dist))
            };
            s.observe(&k, &run, DAY, &b);
        }
        let booked = s
            .estimate(&k, DAY, &b, &Hint::default())
            .map_or(b.default_memory, |est| est.memory_bytes);
        if draw(&mut rng, dist) > memory_max(booked).unwrap() {
            kills += 1;
        }
    }
    (kills * 1000).div_ceil(TRIALS) as u64
}

/// S12: the kill rate of the first run after `n` samples stays within the declared
/// bound for lognormal peaks (sigma 0.1 to 1.0) and for the same with a 3% tail four
/// times larger, 10,000 trials per cell.
#[test]
fn the_kill_rate_stays_within_the_declared_bound() {
    for (row, dist) in DISTS.iter().enumerate() {
        for (col, seen) in COUNTS.iter().enumerate() {
            let rate = kill_rate(*dist, *seen, 11 + row as u64);
            assert!(
                rate <= EPSILON[row][col],
                "sigma {} tail {} after {seen} samples: {rate} per mille > {}",
                dist.sigma,
                dist.tail,
                EPSILON[row][col]
            );
        }
    }
}
