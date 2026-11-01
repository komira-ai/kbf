//! The load gate: a run in normal hours stops when the machine is busier than a threshold.
//!
//! The thresholds are flags ([`crate::config::ConfigArgs`]); this module holds the rule, which
//! is pure, and the sample it judges. Reading `/proc` for a sample is [`LoadSample::read`], over
//! [`crate::procload`]. A pass during which the gate trips is aborted, and its
//! samples are marked busy: a p99 is claimed only from passes that stayed under every limit.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::error::BenchError;
use crate::procload::Reader;

/// The limits. A sample above any one of them trips the gate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadGate {
    /// One-minute load average of the machine that sends the load.
    pub max_loadavg_1m: f64,
    /// Pressure stall information, CPU, "some" share over ten seconds, in percent.
    pub max_psi_cpu_avg10: f64,
    /// Pressure stall information, IO, "some" share over ten seconds, in percent.
    pub max_psi_io_avg10: f64,
    /// Utilisation of the busiest disk the run touches, in percent.
    pub max_disk_util_pct: f64,
}

/// What the machine looked like at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadSample {
    /// One-minute load average.
    pub loadavg_1m: f64,
    /// CPU pressure, "some", ten-second average, percent.
    pub psi_cpu_avg10: f64,
    /// IO pressure, "some", ten-second average, percent.
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
    /// As [`LoadGate::check`] for the gate a run holds between and inside passes (stub of the
    /// scaffold: it checks everything).
    ///
    /// # Errors
    /// [`Trip`], naming the measure.
    pub fn check_run(&self, sample: &LoadSample) -> Result<(), Trip> {
        self.check(sample)
    }
}

impl LoadSample {
    /// Reads the sample from `/proc/loadavg`, `/proc/pressure/{cpu,io}` and `/proc/diskstats`
    /// (utilisation over the interval since the previous call of this process).
    ///
    /// # Errors
    /// [`BenchError`] if the machine has no such files or they are not `/proc` text.
    pub fn read() -> Result<Self, BenchError> {
        static START: OnceLock<Instant> = OnceLock::new();
        static READER: Reader = Reader::new();
        let now_ms = u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis())
            .unwrap_or(u64::MAX);
        READER.read(Path::new("/proc"), Path::new("/sys/block"), now_ms)
    }
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
    }
}
