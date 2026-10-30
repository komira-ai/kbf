//! The machine's load read from files laid out like `/proc` and `/sys/block`: what is read, which
//! disks count, and what an unreadable file does.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::proc_tree;
use kbf_testkit::dir::TestDir;
use tierstore_bench::error::BenchError;
use tierstore_bench::loadmeter::{LoadSource, ProcLoad};

/// A `diskstats` with the given `(device, milliseconds doing IO)` rows.
fn diskstats(rows: &[(&str, u64)]) -> String {
    rows.iter()
        .enumerate()
        .map(|(i, (name, ticks))| format!("   8 {i} {name} 1 0 2 3 4 0 5 6 0 {ticks} 7\n"))
        .collect()
}

/// Catches a figure read from the wrong file or the wrong line, and a disk counted that the gate
/// must leave out: the four figures are the ones in the files, the first reading of the disk is
/// 0 (no interval yet), a rise in the busy time of loop, ram, zram and unlisted devices does not
/// show, and a rise in the busy time of the whole disk does.
#[test]
fn the_load_is_read_from_the_files_and_only_whole_disks_count() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-load-ok");
    let (proc, sys) = proc_tree(dir.path(), "0.50");
    // The clock is the test's: each read is a second after the one before, with no waiting.
    let now = Arc::new(AtomicU64::new(0));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let load = ProcLoad::with_clock(proc.clone(), sys, clock);

    let first = load.sample().unwrap();
    assert!((first.loadavg_1m - 0.5).abs() < 1e-9);
    assert!((first.psi_cpu_avg10 - 0.5).abs() < 1e-9);
    assert!((first.psi_io_avg10 - 1.5).abs() < 1e-9);
    assert!(first.disk_util_pct.abs() < 1e-9);

    let others = [("sda", 1000), ("loop0", 9_000_000), ("ram0", 9_000_000)];
    let more = [("zram0", 9_000_000), ("ghost", 9_000_000)];
    now.store(1000, Ordering::SeqCst);
    std::fs::write(
        proc.join("diskstats"),
        diskstats(&[&others[..], &more[..]].concat()),
    )
    .unwrap();
    let second = load.sample().unwrap();
    assert!(
        second.disk_util_pct.abs() < 1e-9,
        "{}",
        second.disk_util_pct
    );

    // Half a second of the disk's second busy: 50 percent.
    now.store(2000, Ordering::SeqCst);
    let half = [("sda", 1500), ("loop0", 9_000_000)];
    std::fs::write(proc.join("diskstats"), diskstats(&half)).unwrap();
    let third = load.sample().unwrap();
    assert!(
        (third.disk_util_pct - 50.0).abs() < 1e-9,
        "{}",
        third.disk_util_pct
    );

    // More busy time than the interval holds is 100, not more.
    now.store(3000, Ordering::SeqCst);
    let over = [("sda", 9_000_000), ("loop0", 9_000_000)];
    std::fs::write(proc.join("diskstats"), diskstats(&over)).unwrap();
    let fourth = load.sample().unwrap();
    assert!(
        (fourth.disk_util_pct - 100.0).abs() < 1e-9,
        "{}",
        fourth.disk_util_pct
    );
}

/// Catches a missing file or a file that does not parse being read as a calm machine: each of the
/// four files is named in an error when it is missing, and a figure that is no number is refused.
#[test]
fn a_missing_or_malformed_file_is_an_error_not_a_calm_machine() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-load-bad");
    let (proc, sys) = proc_tree(dir.path(), "0.50");
    for file in ["loadavg", "pressure/cpu", "pressure/io", "diskstats"] {
        let kept = std::fs::read_to_string(proc.join(file)).unwrap();
        std::fs::remove_file(proc.join(file)).unwrap();
        let err = ProcLoad::at(proc.clone(), sys.clone())
            .sample()
            .unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{file}: {err:?}");
        assert!(err.to_string().contains(file), "{file}: {err}");
        std::fs::write(proc.join(file), kept).unwrap();
    }
    for file in ["loadavg", "pressure/cpu", "pressure/io"] {
        let kept = std::fs::read_to_string(proc.join(file)).unwrap();
        std::fs::write(proc.join(file), "not a figure\n").unwrap();
        let err = ProcLoad::at(proc.clone(), sys.clone())
            .sample()
            .unwrap_err();
        assert!(matches!(err, BenchError::Request(_)), "{file}: {err:?}");
        std::fs::write(proc.join(file), kept).unwrap();
    }
    // Restored, it reads again.
    assert!(ProcLoad::at(proc, sys).sample().is_ok());
}
