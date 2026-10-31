//! Reading the load of the machine that sends the requests, for the gate ([`crate::gate`]).
//!
//! `/proc/loadavg`, `/proc/pressure/{cpu,io}` and `/proc/diskstats` (Linux). The parsers are pure
//! functions. The disk figure is the busiest whole disk's share of the wall time it spent doing IO
//! since the previous read, so the first read of a [`Reader`] reports 0 for it; loop, ram and
//! zram devices are left out. A machine without these files is an error: a gate that cannot see
//! is not a gate.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use crate::error::BenchError;
use crate::gate::LoadSample;

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

/// The share of `wall_ms` the busiest device of `now` spent doing IO since `before`, percent, at
/// most 100. A device missing from `before` is skipped; a `wall_ms` of 0 is read as 1.
#[must_use]
pub fn busiest_disk_pct(
    before: &BTreeMap<String, u64>,
    now: &[(String, u64)],
    wall_ms: u64,
) -> f64 {
    let busiest_ms = now
        .iter()
        .filter_map(|(name, ticks)| before.get(name).map(|b| ticks.saturating_sub(*b)))
        .max()
        .unwrap_or(0);
    // At most a few days of milliseconds: exactly representable.
    let pct = busiest_ms as f64 * 100.0 / wall_ms.max(1) as f64;
    pct.min(100.0)
}

/// Whether the busy time of `name` is counted: not a loop, ram or zram device, and a whole disk,
/// which is what a directory of that name in `sys_block` says.
fn counted(sys_block: &Path, name: &str) -> bool {
    let virtual_device = ["loop", "ram", "zram"].iter().any(|p| name.starts_with(p));
    !virtual_device & sys_block.join(name).exists()
}

/// What the previous read saw: when (milliseconds on the caller's clock) and the busy time of
/// every counted device.
type Previous = Option<(u64, BTreeMap<String, u64>)>;

/// A reader of the load; it remembers the previous read for the disk figure.
#[derive(Debug, Default)]
pub struct Reader {
    previous: Mutex<Previous>,
}

impl Reader {
    /// A reader that has not read yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            previous: Mutex::new(None),
        }
    }

    /// The load now, from the `/proc` at `proc_root` and the `/sys/block` at `sys_block`;
    /// `now_ms` is a reading of a monotonic clock, the caller's.
    ///
    /// # Errors
    /// [`BenchError::Io`] for a file that cannot be read, [`BenchError::Request`] for one whose
    /// text is not what `/proc` writes.
    pub fn read(
        &self,
        proc_root: &Path,
        sys_block: &Path,
        now_ms: u64,
    ) -> Result<LoadSample, BenchError> {
        let text = |relative: &str| {
            let path = proc_root.join(relative);
            std::fs::read_to_string(&path).map_err(|source| BenchError::Io { path, source })
        };
        let unreadable = || BenchError::Request("load figures of /proc cannot be parsed");
        let loadavg_1m = parse_loadavg_1m(&text("loadavg")?).ok_or_else(unreadable)?;
        let psi_cpu_avg10 = parse_psi_some_avg10(&text("pressure/cpu")?).ok_or_else(unreadable)?;
        let psi_io_avg10 = parse_psi_some_avg10(&text("pressure/io")?).ok_or_else(unreadable)?;
        let now: Vec<(String, u64)> = parse_diskstats(&text("diskstats")?)
            .into_iter()
            .filter(|(name, _)| counted(sys_block, name))
            .collect();
        let mut previous = self.previous.lock().unwrap_or_else(PoisonError::into_inner);
        let (since_ms, before) = previous
            .take()
            .unwrap_or_else(|| (now_ms, now.iter().cloned().collect()));
        let disk_util_pct = busiest_disk_pct(&before, &now, now_ms.saturating_sub(since_ms));
        *previous = Some((now_ms, now.into_iter().collect()));
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
    use kbf_testkit::dir::TestDir;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const PSI: &str = "some avg10=1.50 avg60=0.00 avg300=0.00 total=1\nfull avg10=9.00 avg60=0.00 avg300=0.00 total=1\n";

    fn machine(dir: &TestDir, sda_busy_ms: u64) -> (std::path::PathBuf, std::path::PathBuf) {
        let (proc_root, sys_block) = (dir.path().join("proc"), dir.path().join("sys"));
        write(&proc_root, "loadavg", "0.52 0.40 0.30 1/200 999\n");
        write(&proc_root, "pressure/cpu", PSI);
        write(&proc_root, "pressure/io", PSI);
        let line = |name: &str, busy: u64| format!("8 0 {name} 1 2 3 4 5 6 7 8 9 {busy} 11\n");
        write(
            &proc_root,
            "diskstats",
            &format!("{}{}", line("sda", sda_busy_ms), line("loop0", 99_999)),
        );
        std::fs::create_dir_all(sys_block.join("sda")).unwrap();
        std::fs::create_dir_all(sys_block.join("loop0")).unwrap();
        (proc_root, sys_block)
    }

    /// Catches a parser that reads the wrong field or line: the load is the first figure, the
    /// pressure is the `some` line not the `full` one, and the busy time is field 13.
    #[test]
    fn the_parsers_read_the_named_figures() {
        assert_eq!(parse_loadavg_1m("0.52 0.40 0.30 1/200 999"), Some(0.52));
        assert_eq!(parse_psi_some_avg10(PSI), Some(1.5));
        let stats = parse_diskstats("8 0 sda 1 2 3 4 5 6 7 8 9 4000 11\nshort line\n");
        assert_eq!(stats, vec![("sda".to_owned(), 4000)]);
    }

    /// Catches text that is not `/proc` being read as zero: each parser says no.
    #[test]
    fn text_that_is_not_proc_is_no_figure() {
        assert_eq!(parse_loadavg_1m(""), None);
        assert_eq!(parse_psi_some_avg10("full avg10=1.0\n"), None);
        assert_eq!(parse_psi_some_avg10("some total=1\n"), None);
        assert_eq!(parse_diskstats("8 0 sda x\n"), vec![]);
    }

    /// Catches the disk figure: the busy share of the wall time between two reads, loop devices
    /// left out, the first read 0, and no more than 100.
    #[test]
    fn the_disk_figure_is_the_busy_share_between_two_reads() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload");
        let (proc_root, sys_block) = machine(&dir, 1_000);
        let reader = Reader::new();
        let first = reader.read(&proc_root, &sys_block, 10_000).unwrap();
        assert_eq!(
            (first.loadavg_1m, first.psi_cpu_avg10, first.psi_io_avg10),
            (0.52, 1.5, 1.5)
        );
        assert_eq!(first.disk_util_pct, 0.0);
        machine(&dir, 1_500);
        let second = reader.read(&proc_root, &sys_block, 12_000).unwrap();
        assert_eq!(second.disk_util_pct, 25.0);
        machine(&dir, 9_000);
        let third = reader.read(&proc_root, &sys_block, 12_000).unwrap();
        assert_eq!(third.disk_util_pct, 100.0);
    }

    /// Catches a machine without the files being read as calm, and one whose files are not
    /// `/proc` text.
    #[test]
    fn a_machine_that_cannot_be_read_is_an_error() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-bad");
        let (proc_root, sys_block) = machine(&dir, 0);
        write(&proc_root, "loadavg", "nonsense");
        let reader = Reader::new();
        let err = reader.read(&proc_root, &sys_block, 1).unwrap_err();
        assert!(matches!(err, BenchError::Request(_)), "{err}");
        let err = reader
            .read(&dir.path().join("absent"), &sys_block, 1)
            .unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");
    }
}
