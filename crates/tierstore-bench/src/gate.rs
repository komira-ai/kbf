//! The load gate: a run in normal hours stops when the machine is busier than a threshold.
//!
//! The thresholds are flags ([`crate::config::ConfigArgs`]); this module holds the rule, which
//! is pure, and the sample it judges. Reading `/proc` for a sample is [`LoadSample::read`], over
//! [`crate::procload`].
//!
//! Where it is judged: the preflight judges all four limits on the averages the kernel keeps. A
//! run judges three of them, once, before each pass, over a quiet window of its own (the
//! one-minute load average is left out, see [`LoadGate::check_run`]); a trip there ends the run
//! with the passes so far written. Inside a pass the figures are only recorded
//! ([`crate::result::PassLoad`]): the machine's work and the run's cannot be told apart there.
//! What the gate does not see: with the load average out, the gap between passes is judged on
//! disk time and CPU and IO stall time alone, so a co-tenant on idle cores, or contention for
//! memory bandwidth, shows in neither.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::error::BenchError;
use crate::procload::{QuietTiming, Reader, Writeback, wait_for_writeback};

/// The limits. A sample above any one of them trips the gate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadGate {
    /// One-minute load average of the machine that sends the load; judged at preflight only
    /// (see [`LoadGate::check_run`]).
    pub max_loadavg_1m: f64,
    /// Pressure stall information, CPU, "some" share, in percent: over ten seconds at preflight,
    /// over the quiet window before a pass.
    pub max_psi_cpu_avg10: f64,
    /// Pressure stall information, IO, "some" share, in percent: over ten seconds at preflight,
    /// over the quiet window before a pass.
    pub max_psi_io_avg10: f64,
    /// Utilisation of the busiest disk, in percent: since the previous reading at preflight, over
    /// the quiet window before a pass.
    pub max_disk_util_pct: f64,
}

/// What the machine looked like at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadSample {
    /// One-minute load average.
    pub loadavg_1m: f64,
    /// CPU pressure, "some", percent: the ten-second average, or the share of a quiet window.
    pub psi_cpu_avg10: f64,
    /// IO pressure, "some", percent: the ten-second average, or the share of a quiet window.
    pub psi_io_avg10: f64,
    /// Utilisation of the busiest disk, percent.
    pub disk_util_pct: f64,
}

/// The first limit a sample went over.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("load gate: {metric} is {value}, over the limit {limit}")]
pub struct Trip {
    /// Which measure (`loadavg_1m`, `psi_cpu_avg10`, `psi_io_avg10`, `disk_util_pct`).
    pub metric: &'static str,
    /// What the sample said.
    pub value: f64,
    /// The limit it exceeded.
    pub limit: f64,
}

impl LoadGate {
    /// `Ok` if `sample` is within every limit (equal to a limit is within), else the first
    /// measure over, in the order of the fields of [`LoadSample`].
    ///
    /// # Errors
    /// [`Trip`], naming the measure.
    pub fn check(&self, sample: &LoadSample) -> Result<(), Trip> {
        let rows = [
            ("loadavg_1m", sample.loadavg_1m, self.max_loadavg_1m),
            (
                "psi_cpu_avg10",
                sample.psi_cpu_avg10,
                self.max_psi_cpu_avg10,
            ),
            ("psi_io_avg10", sample.psi_io_avg10, self.max_psi_io_avg10),
            (
                "disk_util_pct",
                sample.disk_util_pct,
                self.max_disk_util_pct,
            ),
        ];
        match rows.into_iter().find(|&(_, value, limit)| value > limit) {
            None => Ok(()),
            Some((metric, value, limit)) => Err(Trip {
                metric,
                value,
                limit,
            }),
        }
    }
}

impl LoadGate {
    /// As [`LoadGate::check`], for the gate a run holds before and inside its passes: the
    /// one-minute load average is not judged. It counts the run's own in-flight requests and
    /// fsyncs and decays over a minute, so no window short enough to sit between passes can
    /// tell the run's load from anyone else's; the preflight judges it, on a machine the run
    /// has not touched yet. The other three measures are as in [`LoadGate::check`].
    ///
    /// # Errors
    /// [`Trip`], naming the measure.
    pub fn check_run(&self, sample: &LoadSample) -> Result<(), Trip> {
        self.check(&LoadSample {
            loadavg_1m: 0.0,
            ..*sample
        })
    }
}

impl LoadSample {
    /// Reads the sample from `/proc/loadavg`, `/proc/pressure/{cpu,io}` and `/proc/diskstats`
    /// (utilisation over the interval since the previous call of this process).
    ///
    /// # Errors
    /// [`BenchError`] if the machine has no such files or they are not `/proc` text.
    pub fn read() -> Result<Self, BenchError> {
        READER.read(Path::new("/proc"), Path::new("/sys/block"), now_ms())
    }

    /// Reads the sample inside a pass: pressure and disk time over the interval since the
    /// previous reading of this process (see [`Reader::read_interval`]). Recorded by the run,
    /// not judged.
    ///
    /// # Errors
    /// [`BenchError`] if the machine has no such files or they are not `/proc` text.
    pub fn read_interval() -> Result<Self, BenchError> {
        READER.read_interval(Path::new("/proc"), Path::new("/sys/block"), now_ms())
    }

    /// Reads the sample for the gate before a pass. First waits, up to a cap, until the memory
    /// not yet written to disk is under a threshold, so that the pass that just ended has landed;
    /// if the cap comes first the window is read as it is. Then the disk and the pressure are
    /// measured over a quiet window that begins then (see [`Reader::read_quiet`]). Blocks for
    /// both waits. With the sample, what the wait for the disks found.
    ///
    /// # Errors
    /// [`BenchError`] if the machine has no such files or they are not `/proc` text.
    pub fn read_quiet(timing: &QuietTiming) -> Result<(Self, Writeback), BenchError> {
        let (proc_root, sys_block) = (Path::new("/proc"), Path::new("/sys/block"));
        let writeback = wait_for_writeback(
            proc_root,
            timing.writeback_below_kib,
            u64::try_from(timing.writeback_cap.as_millis()).unwrap_or(u64::MAX),
            now_ms(),
            || {
                std::thread::sleep(timing.poll);
                now_ms()
            },
        )?;
        let sample = READER.read_quiet(proc_root, sys_block, now_ms(), || {
            std::thread::sleep(timing.settle);
            now_ms()
        })?;
        Ok((sample, writeback))
    }
}

static START: OnceLock<Instant> = OnceLock::new();
static READER: Reader = Reader::new();

/// Milliseconds since the first reading of this process, on a monotonic clock.
fn now_ms() -> u64 {
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATE: LoadGate = LoadGate {
        max_loadavg_1m: 8.0,
        max_psi_cpu_avg10: 10.0,
        max_psi_io_avg10: 10.0,
        max_disk_util_pct: 60.0,
    };

    fn calm() -> LoadSample {
        LoadSample {
            loadavg_1m: 1.0,
            psi_cpu_avg10: 0.0,
            psi_io_avg10: 0.0,
            disk_util_pct: 5.0,
        }
    }

    /// Catches a gate that trips on a calm machine, or on a sample exactly at a limit.
    #[test]
    fn a_sample_within_every_limit_passes_and_a_limit_itself_is_within() {
        assert_eq!(GATE.check(&calm()), Ok(()));
        let at_limit = LoadSample {
            loadavg_1m: 8.0,
            psi_cpu_avg10: 10.0,
            psi_io_avg10: 10.0,
            disk_util_pct: 60.0,
        };
        assert_eq!(GATE.check(&at_limit), Ok(()));
    }

    /// Catches a measure that the gate ignores: each of the four trips it alone, and the
    /// trip names the measure.
    #[test]
    fn each_measure_over_its_limit_trips_the_gate_by_name() {
        let cases: [(&str, LoadSample); 4] = [
            (
                "loadavg_1m",
                LoadSample {
                    loadavg_1m: 9.0,
                    ..calm()
                },
            ),
            (
                "psi_cpu_avg10",
                LoadSample {
                    psi_cpu_avg10: 11.0,
                    ..calm()
                },
            ),
            (
                "psi_io_avg10",
                LoadSample {
                    psi_io_avg10: 11.0,
                    ..calm()
                },
            ),
            (
                "disk_util_pct",
                LoadSample {
                    disk_util_pct: 61.0,
                    ..calm()
                },
            ),
        ];
        for (metric, sample) in cases {
            let trip = GATE.check(&sample).unwrap_err();
            assert_eq!(trip.metric, metric);
            assert!(trip.value > trip.limit);
            assert!(trip.to_string().contains(metric));
        }
    }

    /// Catches the load average judged by the gate of a run: it alone over its limit passes
    /// there and trips in [`LoadGate::check`] (the preflight's), and the other three still trip.
    #[test]
    fn the_gate_of_a_run_does_not_judge_the_load_average() {
        let loaded = LoadSample {
            loadavg_1m: 99.0,
            ..calm()
        };
        assert_eq!(GATE.check(&loaded).unwrap_err().metric, "loadavg_1m");
        assert_eq!(GATE.check_run(&loaded), Ok(()));
        for (metric, sample) in [
            (
                "psi_cpu_avg10",
                LoadSample {
                    psi_cpu_avg10: 11.0,
                    ..loaded
                },
            ),
            (
                "psi_io_avg10",
                LoadSample {
                    psi_io_avg10: 11.0,
                    ..loaded
                },
            ),
            (
                "disk_util_pct",
                LoadSample {
                    disk_util_pct: 61.0,
                    ..loaded
                },
            ),
        ] {
            assert_eq!(GATE.check_run(&sample).unwrap_err().metric, metric);
        }
    }

    /// Catches the reader of this machine failing on a Linux host: the figures are there.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_machine_can_be_read() {
        let sample = LoadSample::read().unwrap();
        assert!(sample.loadavg_1m >= 0.0);
        let now = QuietTiming {
            settle: std::time::Duration::ZERO,
            writeback_cap: std::time::Duration::ZERO,
            poll: std::time::Duration::ZERO,
            ..QuietTiming::DEFAULT
        };
        assert!(LoadSample::read_quiet(&now).unwrap().0.loadavg_1m >= 0.0);
        assert!(LoadSample::read_interval().unwrap().loadavg_1m >= 0.0);
    }
}
