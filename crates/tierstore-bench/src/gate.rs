//! The load gate: a run in normal hours stops when the machine is busier than a threshold.
//!
//! The thresholds are flags ([`crate::config::ConfigArgs`]); this module holds the rule, which
//! is pure, and the sample it judges. Reading `/proc` for a sample is the preflight module
//! owner's ([`LoadSample::read`]). A pass during which the gate trips is aborted, and its
//! samples are marked busy: a p99 is claimed only from passes that stayed under every limit.

use serde::{Deserialize, Serialize};

use crate::error::BenchError;

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

impl LoadSample {
    /// Reads the sample from `/proc/loadavg`, `/proc/pressure/{cpu,io}` and `/proc/diskstats`
    /// (utilisation over the interval since the previous call), for the disks the run names.
    ///
    /// Signature for the preflight owner; not built yet.
    ///
    /// # Errors
    /// [`BenchError::NotYet`] today.
    pub fn read() -> Result<Self, BenchError> {
        Err(BenchError::NotYet("load sample"))
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

    /// Catches the stub answering as if it had read a machine.
    #[test]
    fn reading_the_machine_is_not_built_yet() {
        assert!(matches!(LoadSample::read(), Err(BenchError::NotYet(_))));
    }
}
