//! Reading the load of the machine that sends the requests, for the gate ([`crate::gate`]).
//!
//! [`LoadSource`] is what a cell asks; [`ProcLoad`] answers it from `/proc/loadavg`,
//! `/proc/pressure/{cpu,io}` and `/proc/diskstats` (Linux). The parsing is in pure functions.
//! The disk figure is the busiest whole disk's share of the wall time it spent doing IO since the
//! previous sample, so the first sample of a [`ProcLoad`] reports 0 for it; loop, ram and zram
//! devices are left out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use crate::error::BenchError;
use crate::gate::LoadSample;

/// Where a cell gets a [`LoadSample`].
pub trait LoadSource: Send + Sync {
    /// The load now.
    ///
    /// # Errors
    /// [`BenchError`] if the figures cannot be read; a cell then stops, since it cannot tell
    /// that the machine is calm.
    fn sample(&self) -> Result<LoadSample, BenchError>;
}

/// The one-minute load average of the text of `/proc/loadavg`.
#[must_use]
pub fn parse_loadavg_1m(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// The `avg10` of the `some` line of a `/proc/pressure/*` file.
#[must_use]
pub fn parse_psi_some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// `(device, milliseconds spent doing IO so far)` for every line of `/proc/diskstats`.
#[must_use]
pub fn parse_diskstats(text: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let name = fields.get(2)?;
            let io_ticks_ms = fields.get(12)?.parse().ok()?;
            Some(((*name).to_owned(), io_ticks_ms))
        })
        .collect()
}

/// The share of `wall_ms` the busiest device in `now` spent doing IO since `before`, percent,
/// at most 100. A device missing from `before` is skipped; `wall_ms` of 0 gives 0.
#[must_use]
pub fn busiest_disk_pct(
    before: &BTreeMap<String, u64>,
    now: &[(String, u64)],
    wall_ms: u64,
) -> f64 {
    if wall_ms == 0 {
        return 0.0;
    }
    let busiest_ms = now
        .iter()
        .filter_map(|(name, ticks)| before.get(name).map(|b| ticks.saturating_sub(*b)))
        .max()
        .unwrap_or(0);
    // At most a few days of milliseconds: exactly representable.
    let pct = busiest_ms as f64 * 100.0 / wall_ms as f64;
    pct.min(100.0)
}

/// Whether `name` is a device whose busy time the gate should count.
fn counted_device(name: &str) -> bool {
    !(name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram"))
        && Path::new("/sys/block").join(name).exists()
}

/// A [`LoadSource`] over the real `/proc`.
pub struct ProcLoad {
    root: PathBuf,
    previous: Mutex<Option<(Instant, BTreeMap<String, u64>)>>,
}

impl ProcLoad {
    /// Reads `/proc`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: PathBuf::from("/proc"),
            previous: Mutex::new(None),
        }
    }

    fn read(&self, relative: &str) -> Result<String, BenchError> {
        let path = self.root.join(relative);
        std::fs::read_to_string(&path).map_err(|source| BenchError::Io { path, source })
    }
}

impl Default for ProcLoad {
    fn default() -> Self {
        Self::new()
    }
}

impl LoadSource for ProcLoad {
    fn sample(&self) -> Result<LoadSample, BenchError> {
        let unreadable = || BenchError::Request("load figures of /proc cannot be parsed");
        let loadavg_1m = parse_loadavg_1m(&self.read("loadavg")?).ok_or_else(unreadable)?;
        let psi_cpu_avg10 =
            parse_psi_some_avg10(&self.read("pressure/cpu")?).ok_or_else(unreadable)?;
        let psi_io_avg10 =
            parse_psi_some_avg10(&self.read("pressure/io")?).ok_or_else(unreadable)?;
        let now_at = Instant::now();
        let now: Vec<(String, u64)> = parse_diskstats(&self.read("diskstats")?)
            .into_iter()
            .filter(|(name, _)| counted_device(name))
            .collect();
        let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
        let disk_util_pct = match previous.as_ref() {
            Some((then, before)) => {
                let wall_ms =
                    u64::try_from(now_at.duration_since(*then).as_millis()).unwrap_or(u64::MAX);
                busiest_disk_pct(before, &now, wall_ms)
            }
            None => 0.0,
        };
        *previous = Some((now_at, now.into_iter().collect()));
        Ok(LoadSample {
            loadavg_1m,
            psi_cpu_avg10,
            psi_io_avg10,
            disk_util_pct,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches the load average read from the wrong column.
    #[test]
    fn the_one_minute_load_is_the_first_column() {
        assert_eq!(parse_loadavg_1m("0.52 0.58 0.59 1/467 12345\n"), Some(0.52));
        assert_eq!(parse_loadavg_1m(""), None);
        assert_eq!(parse_loadavg_1m("x 1 2"), None);
    }

    /// Catches the `full` line, or another window, read instead of `some avg10`.
    #[test]
    fn pressure_is_the_some_line_ten_second_average() {
        let text = "some avg10=1.25 avg60=2.00 avg300=3.00 total=99\n\
                    full avg10=7.50 avg60=8.00 avg300=9.00 total=11\n";
        assert_eq!(parse_psi_some_avg10(text), Some(1.25));
        assert_eq!(parse_psi_some_avg10("full avg10=7.50\n"), None);
        assert_eq!(parse_psi_some_avg10("some avg60=2.00\n"), None);
    }

    /// Catches the busy-time column taken from a neighbour: the field after the device name
    /// and nine counters, in milliseconds.
    #[test]
    fn diskstats_gives_the_io_milliseconds_of_each_device() {
        let text = "   8       0 sda 100 1 2000 30 400 5 6000 70 0 8888 99\n\
                    259       0 nvme0n1 1 2 3 4 5 6 7 8 9 4242 11\n\
                    short line\n";
        assert_eq!(
            parse_diskstats(text),
            [("sda".to_owned(), 8888), ("nvme0n1".to_owned(), 4242)]
        );
    }

    /// Catches a utilisation computed over the wrong interval, over a device that was not
    /// there before, or above 100.
    #[test]
    fn the_busiest_device_sets_the_disk_figure() {
        let before = BTreeMap::from([("a".to_owned(), 1000), ("b".to_owned(), 1000)]);
        let now = [
            ("a".to_owned(), 1500),
            ("b".to_owned(), 1200),
            ("new".to_owned(), 999_999),
        ];
        let pct = busiest_disk_pct(&before, &now, 1000);
        assert!((pct - 50.0).abs() < 1e-9, "{pct}");
        assert!((busiest_disk_pct(&before, &now, 100) - 100.0).abs() < 1e-9);
        assert!(busiest_disk_pct(&before, &now, 0).abs() < 1e-9);
        assert!(busiest_disk_pct(&BTreeMap::new(), &now, 1000).abs() < 1e-9);
    }
}
