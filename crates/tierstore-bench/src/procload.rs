//! Reading the load of the machine that sends the requests, for the gate ([`crate::gate`]).
//!
//! `/proc/loadavg`, `/proc/pressure/{cpu,io}` and `/proc/diskstats` (Linux). The parsers are pure
//! functions. The disk figure is the busiest whole disk's share of the wall time it spent doing IO
//! since the previous read, so the first read of a [`Reader`] reports 0 for it; loop, ram and
//! zram devices are left out. A machine without these files is an error: a gate that cannot see
//! is not a gate.
//!
//! [`Reader::read`] is the reading inside a pass and at preflight: the averages as the kernel
//! keeps them. [`Reader::read_quiet`] is the reading before a pass: the disk and the pressure
//! are measured over a window that begins when it is called, so the pass that just ended (whose
//! IO and stalls the averages still carry) is not read as other work on the machine.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::error::BenchError;
use crate::gate::LoadSample;
use crate::result::DiskBusy;

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

/// The quiet window before a pass: long enough for the disk's busy counter (milliseconds) and the
/// pressure totals (microseconds) to move by a figure worth a percentage, short against a pass.
pub const SETTLE: Duration = Duration::from_secs(2);

/// The `total=` of the `some` line of a `/proc/pressure/*` file: microseconds some task stalled.
#[must_use]
pub fn parse_psi_some_total_us(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("total="))?
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

/// The busy time of each counted disk now, by device name, from the `/proc` at `proc_root` and
/// the `/sys/block` at `sys_block`.
///
/// # Errors
/// [`BenchError::Io`] if `diskstats` cannot be read.
pub fn disk_busy_snapshot(
    proc_root: &Path,
    sys_block: &Path,
) -> Result<BTreeMap<String, u64>, BenchError> {
    let path = proc_root.join("diskstats");
    let text = std::fs::read_to_string(&path).map_err(|source| BenchError::Io { path, source })?;
    Ok(parse_diskstats(&text)
        .into_iter()
        .filter(|(name, _)| counted(sys_block, name))
        .collect())
}

/// What each disk of `after` that is also in `before` did over a pass of `elapsed_us`: its busy
/// time and that over the wall time, percent, at most 100.
#[must_use]
pub fn disk_busy_over(
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
    elapsed_us: u64,
) -> BTreeMap<String, DiskBusy> {
    // A pass shorter than a millisecond is read as one, so the share is never a division by 0.
    let wall_ms = (elapsed_us / 1000).max(1);
    after
        .iter()
        .filter_map(|(name, ticks)| {
            let busy_ms = ticks.saturating_sub(*before.get(name)?);
            // At most a few days of milliseconds: exactly representable.
            let util_pct = (busy_ms as f64 * 100.0 / wall_ms as f64).min(100.0);
            Some((name.clone(), DiskBusy { busy_ms, util_pct }))
        })
        .collect()
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
        let text = |relative: &str| read_text(proc_root, relative);
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

    /// The load inside a pass (stub of the scaffold: it reads like [`Reader::read`]).
    ///
    /// # Errors
    /// As [`Reader::read`].
    pub fn read_interval(
        &self,
        proc_root: &Path,
        sys_block: &Path,
        now_ms: u64,
    ) -> Result<LoadSample, BenchError> {
        self.read(proc_root, sys_block, now_ms)
    }

    /// The load before a pass, over a quiet window of its own: the disk figure and both pressure
    /// figures are what the disks and the stall totals did between the start of the window and
    /// its end, not the kernel's ten-second averages and not the time since the previous read.
    /// The load average is the kernel's, as in [`Reader::read`]. The window becomes the
    /// interval the next [`Reader::read`] measures from.
    ///
    /// `start_ms` is a reading of the caller's monotonic clock; `wait` lets the window pass and
    /// gives the clock again (a test moves the fake files and returns a number).
    ///
    /// # Errors
    /// As [`Reader::read`].
    pub fn read_quiet(
        &self,
        proc_root: &Path,
        sys_block: &Path,
        start_ms: u64,
        wait: impl FnOnce() -> u64,
    ) -> Result<LoadSample, BenchError> {
        let disks_before = disk_busy_snapshot(proc_root, sys_block)?;
        let (cpu_before, io_before) = pressure_totals(proc_root)?;
        let now_ms = wait();
        let wall_ms = now_ms.saturating_sub(start_ms);
        let loadavg_1m =
            parse_loadavg_1m(&read_text(proc_root, "loadavg")?).ok_or_else(unreadable)?;
        let (cpu_after, io_after) = pressure_totals(proc_root)?;
        let disks_after = disk_busy_snapshot(proc_root, sys_block)?;
        let disks_now: Vec<(String, u64)> = disks_after
            .iter()
            .map(|(name, ticks)| (name.clone(), *ticks))
            .collect();
        let disk_util_pct = busiest_disk_pct(&disks_before, &disks_now, wall_ms);
        *self.previous.lock().unwrap_or_else(PoisonError::into_inner) = Some((now_ms, disks_after));
        Ok(LoadSample {
            loadavg_1m,
            psi_cpu_avg10: stall_pct(cpu_before, cpu_after, wall_ms),
            psi_io_avg10: stall_pct(io_before, io_after, wall_ms),
            disk_util_pct,
        })
    }
}

/// How long the reading before a pass waits (see [`wait_for_writeback`] and [`Reader::read_quiet`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuietTiming {
    /// The quiet window itself.
    pub settle: Duration,
    /// Dirty plus writeback memory, KiB, under which the disks are taken to be drained.
    pub writeback_below_kib: u64,
    /// The longest wait for the disks to drain before the window is read as it is.
    pub writeback_cap: Duration,
    /// The time between two reads of the memory figures while waiting.
    pub poll: Duration,
}

impl Default for QuietTiming {
    fn default() -> Self {
        Self {
            settle: SETTLE,
            writeback_below_kib: 4096,
            writeback_cap: Duration::from_secs(45),
            poll: Duration::from_millis(500),
        }
    }
}

/// `Dirty` plus `Writeback` of the text of `/proc/meminfo`, KiB (stub of the scaffold).
#[must_use]
pub fn parse_dirty_writeback_kib(_text: &str) -> Option<u64> {
    None
}

/// Waits until the memory not yet written to disk is under `below_kib`, or until `cap_ms` have
/// passed since `start_ms` (stub of the scaffold: it does not wait).
///
/// # Errors
/// [`BenchError`] as [`Reader::read`].
pub fn wait_for_writeback(
    _proc_root: &Path,
    _below_kib: u64,
    _cap_ms: u64,
    _start_ms: u64,
    _poll: impl FnMut() -> u64,
) -> Result<bool, BenchError> {
    Ok(true)
}

/// The text of `relative` under `proc_root`.
fn read_text(proc_root: &Path, relative: &str) -> Result<String, BenchError> {
    let path = proc_root.join(relative);
    std::fs::read_to_string(&path).map_err(|source| BenchError::Io { path, source })
}

/// The error for a file that is there and is not `/proc` text.
fn unreadable() -> BenchError {
    BenchError::Request("load figures of /proc cannot be parsed")
}

/// The stall totals (microseconds) of CPU and IO pressure now.
fn pressure_totals(proc_root: &Path) -> Result<(u64, u64), BenchError> {
    let total = |relative: &str| {
        parse_psi_some_total_us(&read_text(proc_root, relative)?).ok_or_else(unreadable)
    };
    Ok((total("pressure/cpu")?, total("pressure/io")?))
}

/// The share of `wall_ms` that went to stalls, percent, at most 100, from the totals before and
/// after in microseconds; a `wall_ms` of 0 is read as 1, a total that went back as no stall.
fn stall_pct(before_us: u64, after_us: u64, wall_ms: u64) -> f64 {
    // At most a few days of microseconds: exactly representable.
    let pct = after_us.saturating_sub(before_us) as f64 / (wall_ms.max(1) as f64 * 10.0);
    pct.min(100.0)
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

    /// Catches a snapshot that counts a loop device or a partition, or reads the wrong field: the
    /// busy time is field 13 of the whole disks that `/sys/block` lists.
    #[test]
    fn the_snapshot_lists_the_busy_time_of_whole_disks_only() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-snap");
        let (proc_root, sys_block) = machine(&dir, 1_000);
        let snapshot = disk_busy_snapshot(&proc_root, &sys_block).unwrap();
        assert_eq!(snapshot, BTreeMap::from([("sda".to_owned(), 1_000)]));
        let err = disk_busy_snapshot(&dir.path().join("absent"), &sys_block).unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");
    }

    /// Catches the busy time of a pass taken from the whole counter instead of its change, the
    /// share taken over the wrong wall time, a share over 100, a disk that appeared during the
    /// pass being reported from a baseline it never had, and a counter that went back being
    /// reported as a huge busy time.
    #[test]
    fn the_busy_time_of_a_pass_is_the_change_of_each_counter_over_its_wall_time() {
        let map = |pairs: &[(&str, u64)]| -> BTreeMap<String, u64> {
            pairs.iter().map(|(d, ms)| ((*d).to_owned(), *ms)).collect()
        };
        let before = map(&[("sda", 1_000), ("sdb", 5), ("sdd", 900)]);
        let after = map(&[("sda", 1_500), ("sdb", 5), ("sdc", 9), ("sdd", 100)]);
        let over = disk_busy_over(&before, &after, 2_000_000);
        assert_eq!(over.keys().collect::<Vec<_>>(), ["sda", "sdb", "sdd"]);
        assert_eq!((over["sda"].busy_ms, over["sda"].util_pct), (500, 25.0));
        assert_eq!((over["sdb"].busy_ms, over["sdb"].util_pct), (0, 0.0));
        assert_eq!((over["sdd"].busy_ms, over["sdd"].util_pct), (0, 0.0));

        let saturated = disk_busy_over(&before, &map(&[("sda", 9_000)]), 2_000_000);
        assert_eq!(
            (saturated["sda"].busy_ms, saturated["sda"].util_pct),
            (8_000, 100.0)
        );
        let instant = disk_busy_over(&before, &map(&[("sda", 1_005)]), 0);
        assert_eq!(instant["sda"].util_pct, 100.0);
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

    /// Catches a quiet sample that counts the pass that just ran: the disk was saturated for the
    /// whole interval before the window began, and idle inside it, so the figure is 0; and the
    /// next ordinary read measures from the end of the window, not from before the pass.
    #[test]
    fn the_busy_time_before_the_quiet_window_is_not_counted() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-quiet-idle");
        let (proc_root, sys_block) = machine(&dir, 1_000);
        let reader = Reader::new();
        reader.read(&proc_root, &sys_block, 10_000).unwrap();
        // The pass: the disk was busy for all of the ten seconds it took.
        machine(&dir, 11_000);
        let quiet = reader
            .read_quiet(&proc_root, &sys_block, 20_000, || 21_000)
            .unwrap();
        assert_eq!(quiet.disk_util_pct, 0.0);
        assert_eq!(quiet.loadavg_1m, 0.52);
        machine(&dir, 11_100);
        let next = reader.read(&proc_root, &sys_block, 22_000).unwrap();
        assert_eq!(next.disk_util_pct, 10.0);
    }

    /// Catches a quiet sample that does not see other work: the disk gets 900 ms of IO during
    /// the one-second window (the wait is injected, and changes the fake `diskstats`), so the
    /// figure is 90 and a gate with a 60 limit names `disk_util_pct`.
    #[test]
    fn a_busy_quiet_window_trips_the_gate() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-quiet-busy");
        let (proc_root, sys_block) = machine(&dir, 1_000);
        let reader = Reader::new();
        reader.read(&proc_root, &sys_block, 10_000).unwrap();
        let quiet = reader
            .read_quiet(&proc_root, &sys_block, 20_000, || {
                machine(&dir, 1_900);
                21_000
            })
            .unwrap();
        assert_eq!(quiet.disk_util_pct, 90.0);
        let gate = crate::gate::LoadGate {
            max_loadavg_1m: 8.0,
            max_psi_cpu_avg10: 10.0,
            max_psi_io_avg10: 10.0,
            max_disk_util_pct: 60.0,
        };
        assert_eq!(gate.check(&quiet).unwrap_err().metric, "disk_util_pct");
    }

    /// Catches a quiet sample as the first reading of a reader being measured against nothing:
    /// the window itself is the interval, so a disk busy in it is seen on the very first call.
    #[test]
    fn the_first_reading_may_be_a_quiet_one() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-quiet-first");
        let (proc_root, sys_block) = machine(&dir, 0);
        let quiet = Reader::new()
            .read_quiet(&proc_root, &sys_block, 0, || {
                machine(&dir, 500);
                1_000
            })
            .unwrap();
        assert_eq!(quiet.disk_util_pct, 50.0);
    }

    /// Catches a quiet sample that reads pressure from the ten-second average: the files say
    /// 1.5 throughout, the stall total moves 300 ms (cpu) and 50 ms (io) inside the one-second
    /// window and 4 s before it, so the figures are 30 and 5.
    #[test]
    fn pressure_in_a_quiet_window_is_the_stall_over_the_window() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-quiet-psi");
        let (proc_root, sys_block) = machine(&dir, 0);
        let psi = |cpu_us: u64, io_us: u64| {
            let text = |us: u64| {
                format!(
                    "some avg10=1.50 avg60=0.00 avg300=0.00 total={us}\nfull avg10=9.00 avg60=0.00 avg300=0.00 total=1\n"
                )
            };
            write(&proc_root, "pressure/cpu", &text(cpu_us));
            write(&proc_root, "pressure/io", &text(io_us));
        };
        psi(5_000_000, 5_000_000);
        let reader = Reader::new();
        reader.read(&proc_root, &sys_block, 10_000).unwrap();
        psi(9_000_000, 9_000_000);
        let quiet = reader
            .read_quiet(&proc_root, &sys_block, 20_000, || {
                psi(9_300_000, 9_050_000);
                21_000
            })
            .unwrap();
        assert_eq!((quiet.psi_cpu_avg10, quiet.psi_io_avg10), (30.0, 5.0));
    }

    /// Catches a quiet sample that takes a file it cannot read for a calm machine: a pressure
    /// file without a stall total, and a machine without the files.
    #[test]
    fn a_machine_that_cannot_be_read_quietly_is_an_error() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-quiet-bad");
        let (proc_root, sys_block) = machine(&dir, 0);
        assert_eq!(parse_psi_some_total_us(PSI), Some(1));
        assert_eq!(parse_psi_some_total_us("some avg10=1.0\n"), None);
        assert_eq!(parse_psi_some_total_us("full total=1\n"), None);
        write(&proc_root, "pressure/io", "some avg10=1.0\n");
        let reader = Reader::new();
        let err = reader
            .read_quiet(&proc_root, &sys_block, 0, || 1)
            .unwrap_err();
        assert!(matches!(err, BenchError::Request(_)), "{err}");
        let err = reader
            .read_quiet(&dir.path().join("absent"), &sys_block, 0, || 1)
            .unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");
        let (proc_root, sys_block) = machine(&dir, 0);
        write(&proc_root, "loadavg", "nonsense");
        let err = reader
            .read_quiet(&proc_root, &sys_block, 0, || 1)
            .unwrap_err();
        assert!(matches!(err, BenchError::Request(_)), "{err}");
    }

    fn write_psi(root: &Path, cpu_us: u64, io_us: u64) {
        let text = |us: u64| {
            format!(
                "some avg10=1.50 avg60=0.00 avg300=0.00 total={us}\nfull avg10=9.00 avg60=0.00 avg300=0.00 total=1\n"
            )
        };
        write(root, "pressure/cpu", &text(cpu_us));
        write(root, "pressure/io", &text(io_us));
    }

    /// Catches a reading inside a pass taken from the ten-second averages, or from before the
    /// last reading: over 2 s the stalls and the disk moved 20, 5 and 25 percent while the files'
    /// averages say 1.5, and over the next second from the end of the first, 50 and 10; and a
    /// quiet window is the base of the reading after it.
    #[test]
    fn a_reading_inside_a_pass_is_the_change_since_the_previous_reading() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-interval");
        let (proc_root, sys_block) = machine(&dir, 1_000);
        write_psi(&proc_root, 1_000_000, 2_000_000);
        let reader = Reader::new();
        reader.read(&proc_root, &sys_block, 10_000).unwrap();
        machine(&dir, 1_500);
        write_psi(&proc_root, 1_400_000, 2_100_000);
        let first = reader
            .read_interval(&proc_root, &sys_block, 12_000)
            .unwrap();
        assert_eq!(
            (first.psi_cpu_avg10, first.psi_io_avg10, first.disk_util_pct),
            (20.0, 5.0, 25.0)
        );
        machine(&dir, 1_600);
        write_psi(&proc_root, 1_900_000, 2_100_000);
        let second = reader
            .read_interval(&proc_root, &sys_block, 13_000)
            .unwrap();
        assert_eq!(
            (
                second.psi_cpu_avg10,
                second.psi_io_avg10,
                second.disk_util_pct
            ),
            (50.0, 0.0, 10.0)
        );
        let quiet = reader
            .read_quiet(&proc_root, &sys_block, 20_000, || 21_000)
            .unwrap();
        assert_eq!(quiet.psi_cpu_avg10, 0.0);
        write_psi(&proc_root, 2_000_000, 2_100_000);
        let third = reader
            .read_interval(&proc_root, &sys_block, 22_000)
            .unwrap();
        assert_eq!(third.psi_cpu_avg10, 10.0);
    }

    /// Catches the first reading of a reader inside a pass being measured against nothing.
    #[test]
    fn the_first_reading_inside_a_pass_is_calm() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-interval-first");
        let (proc_root, sys_block) = machine(&dir, 1_000);
        let first = Reader::new()
            .read_interval(&proc_root, &sys_block, 5)
            .unwrap();
        assert_eq!(
            (first.psi_cpu_avg10, first.psi_io_avg10, first.disk_util_pct),
            (0.0, 0.0, 0.0)
        );
        assert_eq!(first.loadavg_1m, 0.52);
    }

    fn meminfo(root: &Path, dirty_kib: u64, writeback_kib: u64) {
        write(
            root,
            "meminfo",
            &format!(
                "MemTotal:       16000000 kB\nDirty:  {dirty_kib:>12} kB\nWriteback:  {writeback_kib:>8} kB\nSlab: 5 kB\n"
            ),
        );
    }

    /// Catches a parser that reads the wrong lines or takes one figure for both.
    #[test]
    fn dirty_and_writeback_are_added_from_meminfo() {
        let text = "MemTotal: 1 kB\nDirty:  100 kB\nWritebackTmp: 7 kB\nWriteback:  20 kB\n";
        assert_eq!(parse_dirty_writeback_kib(text), Some(120));
        assert_eq!(parse_dirty_writeback_kib("Dirty: 100 kB\n"), None);
        assert_eq!(
            parse_dirty_writeback_kib("Dirty: x kB\nWriteback: 1 kB\n"),
            None
        );
    }

    /// Catches a wait that does not wait for the disks: memory not yet written is 50 000 KiB, then
    /// 20 000 after the first read of it, then 10 after the third, and the answer is "drained"
    /// after exactly three waits (the clock is the test's, 500 ms a wait).
    #[test]
    fn the_wait_ends_when_dirty_and_writeback_memory_is_under_the_threshold() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-wb-drain");
        let (proc_root, _) = machine(&dir, 0);
        meminfo(&proc_root, 40_000, 10_000);
        let (mut polls, mut now) = (0u32, 1_000u64);
        let drained = wait_for_writeback(&proc_root, 4_096, 45_000, now, || {
            polls += 1;
            now += 500;
            match polls {
                1 => meminfo(&proc_root, 15_000, 5_000),
                3 => meminfo(&proc_root, 10, 0),
                _ => {}
            }
            now
        })
        .unwrap();
        assert!(drained);
        assert_eq!(polls, 3);
    }

    /// Catches a wait with no end: memory never drains, and after the cap (45 s of the test's
    /// clock, 10 s a wait) the answer is "not drained" after exactly five waits; a disk already
    /// drained is not waited for at all.
    #[test]
    fn the_wait_gives_up_at_the_cap() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-wb-cap");
        let (proc_root, _) = machine(&dir, 0);
        meminfo(&proc_root, 90_000, 0);
        let (mut polls, mut now) = (0u32, 100u64);
        let drained = wait_for_writeback(&proc_root, 4_096, 45_000, now, || {
            polls += 1;
            now += 10_000;
            now
        })
        .unwrap();
        assert!(!drained);
        assert_eq!(polls, 5);
        meminfo(&proc_root, 0, 0);
        let drained = wait_for_writeback(&proc_root, 4_096, 45_000, 0, || unreachable!()).unwrap();
        assert!(drained);
    }

    /// Catches a wait that takes a machine it cannot read for a drained one.
    #[test]
    fn a_meminfo_that_cannot_be_read_is_an_error() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-procload-wb-bad");
        let (proc_root, _) = machine(&dir, 0);
        let err = wait_for_writeback(&proc_root, 1, 1, 0, || 1).unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");
        write(&proc_root, "meminfo", "nonsense\n");
        let err = wait_for_writeback(&proc_root, 1, 1, 0, || 1).unwrap_err();
        assert!(matches!(err, BenchError::Request(_)), "{err}");
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
