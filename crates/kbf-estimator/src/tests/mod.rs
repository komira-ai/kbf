//! Each rule of the estimator has a test here that goes red when the rule is broken;
//! the doc comment of a test names the rule and the defect it catches.

mod calibration;
mod memory;
mod reference;
mod safety;
mod storage;

use crate::*;

pub(super) const DAY: Day = Day(100);

pub(super) fn key(basis: Basis, cfg: Option<u64>) -> Key {
    Key::new(
        Instance::new("main"),
        PlatformClass::new("linux-x86_64"),
        basis,
        Shape(7),
        cfg.map(Cfg),
    )
}

pub(super) fn linux(cfg: u64) -> Key {
    key(Basis::Cgroup, Some(cfg))
}

/// A success that used `peak`, booked high enough never to have touched its limit.
pub(super) fn success(peak: u64) -> Run {
    Run {
        outcome: Outcome::Success,
        wall_us: 2_000_000,
        peak_memory_bytes: Some(peak),
        booked_memory_bytes: peak.max(4 * GIB),
        raised_memory_bytes: None,
        limit_hits: 0,
        swap_peak_bytes: 0,
        cpu_usage_us: 0,
        node_cpu_pressure_permille: None,
    }
}

/// An own-limit kill of an attempt booked at `booked` that had used `used`.
pub(super) fn kill(booked: u64, used: u64, raised: Option<u64>) -> Run {
    Run {
        outcome: Outcome::OwnLimitKill,
        peak_memory_bytes: Some(used),
        booked_memory_bytes: booked,
        raised_memory_bytes: raised,
        ..success(0)
    }
}

/// The kill that sets a 16 GiB floor (booked 4 GiB, ladder retries at 16 GiB).
pub(super) fn heavy_kill() -> Run {
    kill(4 * GIB, 6 * GIB + GIB / 2, Some(16 * GIB))
}

/// A success of 1 GiB that used `par` millicores on average.
pub(super) fn busy(par: u64, pressure: Option<u16>) -> Run {
    Run {
        cpu_usage_us: par * 2000,
        node_cpu_pressure_permille: pressure,
        ..success(GIB)
    }
}

pub(super) fn feed_on(s: &mut Summaries, key: &Key, run: &Run, times: usize, day: Day) {
    for _ in 0..times {
        s.observe(key, run, day, &Bounds::default());
    }
}

pub(super) fn feed(s: &mut Summaries, key: &Key, run: &Run, times: usize) {
    feed_on(s, key, run, times, DAY);
}

pub(super) fn estimate(s: &Summaries, key: &Key) -> Result<Estimate, Fallback> {
    s.estimate(key, DAY, &Bounds::default(), &Hint::default())
}

pub(super) fn memory(s: &Summaries, key: &Key) -> u64 {
    estimate(s, key).unwrap().memory_bytes
}

pub(super) fn floor(s: &Summaries, key: &Key) -> u64 {
    s.key_floor(key, &Bounds::default())
}

/// A small deterministic generator (xorshift64): the tests are repeatable and the pure
/// crate itself stays free of randomness.
pub(super) struct Rng(pub u64);

impl Rng {
    pub(super) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Uniform in (0, 1).
    pub(super) fn unit(&mut self) -> f64 {
        ((self.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Standard normal (Box-Muller).
    pub(super) fn normal(&mut self) -> f64 {
        let (a, b) = (self.unit(), self.unit());
        (-2.0 * a.ln()).sqrt() * (2.0 * std::f64::consts::PI * b).cos()
    }

    /// Mostly a plausible size, sometimes an extreme.
    pub(super) fn size(&mut self, scale: u64) -> u64 {
        match self.below(20) {
            0 => u64::MAX,
            1 => 0,
            _ => self.below(scale.saturating_mul(2).max(1)),
        }
    }
}
