//! A naive reference model of one key, and random streams checked against it.
//!
//! The reference keeps every sample and recomputes each answer by scanning the whole
//! history; it shares no code with the crate's chunked summary. The streams also run
//! on generated farms (a host memory that changes from step to step) and assert the
//! safety properties directly.

use super::*;

const STEP: u64 = 256 * MIB;

fn up(value: u64) -> u64 {
    value.div_ceil(STEP).saturating_mul(STEP)
}

struct Reference {
    /// Every memory sample as contributed: (value, censored, day).
    samples: Vec<(u64, bool, u32)>,
    cpu: Vec<u32>,
    floor: u64,
    anchor: usize,
    floor_day: u32,
    /// `samples.len()` at each kill event.
    events: Vec<usize>,
    until: u32,
    last_seen: u32,
}

impl Reference {
    fn new(day: u32) -> Self {
        Self {
            samples: Vec::new(),
            cpu: Vec::new(),
            floor: 0,
            anchor: 0,
            floor_day: 0,
            events: Vec::new(),
            until: 0,
            last_seen: day,
        }
    }

    /// The samples of the nine most recent chunks of 32, by `held`.
    fn window(&self, held: bool) -> Vec<u64> {
        let n = self.samples.len();
        let chunk = n.saturating_sub(1) / 32;
        self.samples
            .iter()
            .enumerate()
            .filter(|(i, (_, censored, _))| *censored == held && i / 32 + 8 >= chunk)
            .map(|(_, (value, _, _))| *value)
            .collect()
    }

    fn observe(&mut self, run: &Run, day: u32, b: &Bounds) {
        self.last_seen = self.last_seen.max(day.min(MAX_DAY));
        let now = self.last_seen;
        let trusted = run.wall_us >= 1_000_000;
        match run.outcome {
            Outcome::Success => {
                let usable = |p: &u64| trusted && *p > 0 && *p < b.host_memory;
                if let Some(peak) = run.peak_memory_bytes.filter(usable) {
                    {
                        let limit = memory_max(run.booked_memory_bytes);
                        let near =
                            limit.is_some_and(|l| u128::from(peak) * 100 >= u128::from(l) * 95);
                        if run.limit_hits > 0 || run.swap_peak_bytes > 0 || near {
                            self.samples
                                .push((peak.min(run.booked_memory_bytes), true, now));
                        } else {
                            self.samples.push((peak, false, now));
                        }
                        self.decay(now);
                    }
                }
            }
            Outcome::OwnLimitKill => {
                let limit = memory_max(run.booked_memory_bytes).unwrap_or(0);
                let used = run.peak_memory_bytes.unwrap_or(0);
                let sample = used.max(limit).min(b.host_memory.saturating_sub(1));
                if sample > 0 {
                    self.samples.push((sample, false, now));
                    self.kill(run, now, b);
                }
            }
            _ => {}
        }
        if run.outcome == Outcome::Success
            && trusted
            && run.cpu_usage_us > 0
            && run.node_cpu_pressure_permille.is_some_and(|p| p < 100)
        {
            let millis = u128::from(run.cpu_usage_us) * 1000 / u128::from(run.wall_us);
            self.cpu.push(millis.min(u128::from(u32::MAX)) as u32);
        }
    }

    fn kill(&mut self, run: &Run, now: u32, b: &Bounds) {
        let n = self.samples.len();
        let ceiling = b.host_memory.saturating_sub(1) / STEP * STEP;
        let above = up(run.booked_memory_bytes.saturating_add(1));
        let candidate = run.raised_memory_bytes.unwrap_or(0).max(above).min(ceiling);
        let new_event = run.booked_memory_bytes >= self.floor;
        let raised = candidate > self.floor;
        self.floor = self.floor.max(candidate);
        if raised || new_event {
            self.anchor = n;
            self.floor_day = now;
        }
        if new_event {
            if self.events.last().is_some_and(|prev| n - prev < 256) {
                self.until = self.until.max(now.saturating_add(7));
            }
            self.events.push(n);
        }
    }

    fn decay(&mut self, now: u32) {
        let n = self.samples.len();
        let quiet = n - self.anchor >= 256 && now.saturating_sub(self.floor_day) >= 7;
        let week = self
            .samples
            .iter()
            .filter(|(_, _, day)| now >= *day && now - day < 8)
            .map(|(value, _, _)| *value)
            .max()
            .unwrap_or(0);
        let long = self.window(false).into_iter().max().unwrap_or(0);
        if self.floor > 0 && quiet && long.max(week).saturating_mul(2) < self.floor {
            let half = up(self.floor / 2);
            self.floor = if half < GIB { 0 } else { half };
            self.anchor = n;
            self.floor_day = now;
        }
    }

    fn mixed(&self) -> bool {
        let mut last: Vec<u64> = self.samples.iter().rev().take(32).map(|s| s.0).collect();
        last.sort_unstable();
        match (last.last(), last.get(last.len() / 2)) {
            (Some(max), Some(median)) => *max > median.saturating_mul(4),
            _ => false,
        }
    }

    fn estimate(&self, b: &Bounds, day: u32, hint: &Hint) -> Result<(u64, u32), Fallback> {
        let n = self.samples.len();
        if n < 2 && self.floor == 0 {
            return Err(Fallback::Cold);
        }
        let ceiling = b.host_memory.saturating_sub(1) / STEP * STEP;
        let floor = self.floor.min(ceiling);
        let raise_only = day.max(self.last_seen).min(MAX_DAY) < self.until;
        let mut memory = if self.mixed() {
            b.default_memory.max(floor)
        } else {
            let max = self.window(false).into_iter().max().unwrap_or(0);
            let held = self.window(true).into_iter().max().unwrap_or(0);
            let sized = max
                .saturating_add(max.div_ceil(5))
                .saturating_add(128 * MIB);
            let memory = up(sized.max(held).max(floor));
            if n < 5 || raise_only {
                memory.max(b.default_memory)
            } else {
                memory
            }
        };
        memory = memory.max(GIB).max(hint.memory_bytes.unwrap_or(0));
        if memory >= b.host_memory {
            return Err(Fallback::OverCap);
        }
        let mut last: Vec<u32> = self.cpu.iter().rev().take(32).copied().collect();
        last.sort_unstable();
        let rank = (last.len() * 9).div_ceil(10);
        let p90 = if rank == 0 { 0 } else { last[rank - 1] };
        let low = 1000.min(b.node_cpu_millis);
        let cpu = p90
            .div_ceil(250)
            .saturating_mul(250)
            .clamp(low, b.node_cpu_millis);
        Ok((memory, cpu.max(hint.cpu_millis.unwrap_or(0))))
    }
}

/// A random farm: nodes with memory, of which a random subset matches the request.
/// The bound is the largest matching node's memory.
fn host_of_a_farm(rng: &mut Rng) -> u64 {
    let nodes = 1 + rng.below(6);
    (0..nodes)
        .map(|_| [2, 4, 8, 16, 33, 64, 256, 512][rng.below(8) as usize] * GIB)
        .max()
        .unwrap_or(GIB)
}

pub(super) fn random_run(rng: &mut Rng, host: u64) -> Run {
    let ladder = [GIB, 2 * GIB, 4 * GIB, 8 * GIB, 16 * GIB, 32 * GIB];
    let booked = if rng.below(4) == 0 {
        rng.size(host)
    } else {
        ladder[rng.below(6) as usize]
    };
    let favourite = [500 * MIB, GIB, 3 * GIB, 6 * GIB, 9 * GIB, host / 2];
    let peak = if rng.below(3) == 0 {
        rng.size(host)
    } else {
        favourite[rng.below(6) as usize] + rng.below(3) * MIB
    };
    let outcome = [
        Outcome::Success,
        Outcome::Success,
        Outcome::Success,
        Outcome::Success,
        Outcome::OwnLimitKill,
        Outcome::NodePressureKill,
        Outcome::Failed,
        Outcome::Lost,
        Outcome::Cancelled,
    ][rng.below(9) as usize];
    Run {
        outcome,
        wall_us: if rng.below(6) == 0 {
            rng.size(3_000_000)
        } else {
            2_000_000
        },
        peak_memory_bytes: (rng.below(10) != 0).then_some(peak),
        booked_memory_bytes: booked,
        raised_memory_bytes: (rng.below(3) != 0).then(|| rng.size(host)),
        limit_hits: rng.below(8) / 7,
        swap_peak_bytes: rng.below(10) / 9,
        cpu_usage_us: if rng.below(4) == 0 {
            0
        } else {
            rng.size(40_000_000)
        },
        node_cpu_pressure_permille: (rng.below(5) != 0).then(|| rng.below(300) as u16),
    }
}

/// N8: the crate and the naive reference agree on every answer of random streams, on
/// generated farms whose largest node changes from step to step, and the safety
/// properties hold at every step: nothing reaches the host, the estimate is never
/// under the key floor, a hint is never lowered, fewer than `N_MIN` samples never go
/// under the default, and a kill is answered above the booking it died at.
#[test]
fn the_crate_agrees_with_the_naive_reference_on_generated_farms() {
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let k = key(Basis::Cgroup, None);
        let mut s = Summaries::default();
        let mut reference = Reference::new(10);
        let mut day = 10u32;
        for step in 0..500 {
            day += rng.below(3) as u32 / 2 + u32::from(rng.below(40) == 0) * 8;
            let host = host_of_a_farm(&mut rng);
            let b = Bounds {
                default_memory: [0, GIB, 4 * GIB, 7 * GIB][rng.below(4) as usize],
                host_memory: host,
                node_cpu_millis: [500, 4000, 64_000][rng.below(3) as usize],
            };
            let run = random_run(&mut rng, host);
            s.observe(&k, &run, Day(day), &b);
            reference.observe(&run, day, &b);
            let hint = if rng.below(4) == 0 {
                Hint {
                    memory_bytes: Some(rng.size(host)),
                    cpu_millis: Some(rng.below(9000) as u32),
                }
            } else {
                Hint::default()
            };
            let got = s.estimate(&k, Day(day), &b, &hint);
            let want = reference.estimate(&b, day, &hint);
            let at = format!("seed {seed} step {step} {run:?} {b:?}");
            match (&got, &want) {
                (Ok(est), Ok((memory, cpu))) => {
                    assert_eq!((est.memory_bytes, est.cpu_millis), (*memory, *cpu), "{at}");
                    assert!(est.memory_bytes < host, "{at}");
                    assert!(est.memory_bytes >= s.key_floor(&k, &b), "{at}");
                    assert!(est.memory_bytes >= hint.memory_bytes.unwrap_or(0), "{at}");
                    assert!(est.cpu_millis >= hint.cpu_millis.unwrap_or(0), "{at}");
                    if s.get(&k).unwrap().samples() < N_MIN {
                        assert!(est.memory_bytes >= b.default_memory, "{at}");
                    }
                    let ceiling = host.saturating_sub(1) / STEP * STEP;
                    if run.outcome == Outcome::OwnLimitKill && run.booked_memory_bytes < ceiling {
                        assert!(est.memory_bytes > run.booked_memory_bytes, "{at}");
                    }
                }
                _ => assert_eq!(got.map(|_| ()), want.map(|_| ()), "{at}"),
            }
            assert!(s.key_floor(&k, &b) < host, "{at}");
        }
    }
}
