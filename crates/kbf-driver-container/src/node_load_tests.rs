//! Unit tests of `node_load`: the parsers over real-format files, the reader over a
//! directory, and the tracker's covered set, windows and sequence number.

use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

const CPU_STAT: &str = "usage_usec 5000000
user_usec 3000000
system_usec 2000000
nr_periods 100
nr_throttled 4
throttled_usec 123456
nr_bursts 0
burst_usec 0
";

const MEMORY_STAT: &str = "anon 1048576
file 8388608
inactive_anon 0
active_anon 1048576
inactive_file 2097152
active_file 6291456
unevictable 0
";

const PRESSURE: &str = "some avg10=1.25 avg60=0.50 avg300=0.10 total=987654
full avg10=0.00 avg60=0.00 avg300=0.00 total=321
";

fn id(seq: u64) -> LeaseId {
    LeaseId { term: 1, seq }
}

/// A clock the test sets.
fn manual() -> (Arc<AtomicU64>, MonoClock) {
    let now = Arc::new(AtomicU64::new(0));
    let reader = Arc::clone(&now);
    (now, Arc::new(move || reader.load(Ordering::SeqCst)))
}

fn write(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).expect("write");
}

/// A cgroup directory with the files of a plain, unlimited cgroup.
fn cgroup(name: &str) -> kbf_testkit::dir::TestDir {
    let dir = crate::unit_dir(name);
    write(&dir, "cpu.stat", CPU_STAT);
    write(&dir, "memory.current", "10485760\n");
    write(&dir, "memory.stat", MEMORY_STAT);
    write(&dir, "cpu.max", "max 100000\n");
    write(&dir, "cpu.pressure", PRESSURE);
    dir
}

fn usage(dir: &Path, usec: u64) {
    write(dir, "cpu.stat", &format!("usage_usec {usec}\n"));
}

/// Catches `usage_usec` read from another line, a missing `throttled_usec` turned into 0,
/// and a file without `usage_usec` read as a reading of 0.
#[test]
fn cpu_stat_gives_usage_and_throttled_time() {
    assert_eq!(parse_cpu_stat(CPU_STAT), Ok((5_000_000, Some(123_456))));
    assert_eq!(
        parse_cpu_stat("usage_usec 7\nuser_usec 7\nsystem_usec 0\n"),
        Ok((7, None))
    );
    for bad in ["", "user_usec 1\n", "usage_usec many\n"] {
        assert!(matches!(
            parse_cpu_stat(bad),
            Err(LoadError::Malformed {
                file: "cpu.stat",
                ..
            })
        ));
    }
}

/// Catches `inactive_anon` or `active_file` read for `inactive_file`, and a missing or
/// garbled line read as 0 (which would make the whole cache look hot).
#[test]
fn memory_stat_gives_inactive_file() {
    assert_eq!(parse_inactive_file(MEMORY_STAT), Ok(2_097_152));
    for bad in [
        "",
        "inactive_anon 5\nactive_file 5\n",
        "inactive_file x\n",
        "inactive_file\n",
    ] {
        assert!(matches!(
            parse_inactive_file(bad),
            Err(LoadError::Malformed {
                file: "memory.stat",
                ..
            })
        ));
    }
    assert_eq!(parse_memory_current("42\n"), Ok(42));
    for bad in ["", "-1", "lots"] {
        assert!(matches!(
            parse_memory_current(bad),
            Err(LoadError::Malformed {
                file: "memory.current",
                ..
            })
        ));
    }
}

/// Catches `inactive_file` or `active_anon` read for `active_file`, and a missing or
/// garbled line read as 0 (which would make the admission working set look larger or smaller
/// than the cgroup's).
#[test]
fn memory_stat_gives_active_file() {
    assert_eq!(parse_active_file(MEMORY_STAT), Ok(6_291_456));
    for bad in [
        "",
        "inactive_file 5\nactive_anon 5\n",
        "active_file x\n",
        "active_file\n",
    ] {
        assert!(matches!(
            parse_active_file(bad),
            Err(LoadError::Malformed {
                file: "memory.stat",
                ..
            })
        ));
    }
}

/// Catches `max 100000` read as a limit, a quota read as none, and a garbled file read
/// as either.
#[test]
fn cpu_max_is_limited_unless_the_quota_is_max() {
    assert_eq!(parse_cpu_limited("max 100000\n"), Ok(false));
    assert_eq!(parse_cpu_limited("8700000 100000\n"), Ok(true));
    for bad in ["", "\n", "fast 100000"] {
        assert!(matches!(
            parse_cpu_limited(bad),
            Err(LoadError::Malformed {
                file: "cpu.max",
                ..
            })
        ));
    }
}

/// Catches the `full` line, or `avg10`, read for the `some` total.
#[test]
fn pressure_gives_the_some_total() {
    assert_eq!(parse_pressure_some_total(PRESSURE), Some(987_654));
    assert_eq!(parse_pressure_some_total("full avg10=0.00 total=5\n"), None);
    assert_eq!(parse_pressure_some_total("some avg10=0.00\n"), None);
    assert_eq!(parse_pressure_some_total("some total=soon\n"), None);
    assert_eq!(parse_pressure_some_total(""), None);
}

/// Catches a window computed with the wrong unit, a division by an empty window, and a
/// counter that went back wrapping to a huge number.
#[test]
fn a_window_is_millicores_or_nothing() {
    // 4 CPU-seconds in 2 seconds is 2 CPUs.
    assert_eq!(
        window_millicores((0, 0), (4_000_000, 2_000_000)),
        Some(2000)
    );
    assert_eq!(window_millicores((10, 5), (10, 6)), Some(0));
    assert_eq!(window_millicores((0, 5), (9, 5)), None);
    assert_eq!(window_millicores((0, 9), (9, 5)), None);
    assert_eq!(window_millicores((9, 0), (3, 5)), None);
    assert_eq!(window_millicores((0, 0), (u64::MAX, 1)), Some(u64::MAX));
}

/// Catches a field read from the wrong file, the clock read before `cpu.stat` rather than
/// with it, and an unlimited cgroup reported as limited.
#[test]
fn a_directory_of_cgroup_files_is_one_reading() {
    let dir = cgroup("load-plain");
    let (now, clock) = manual();
    now.store(77, Ordering::SeqCst);
    let load = read_load(&dir, &*clock).expect("reading");
    assert_eq!(
        load,
        NodeLoad {
            seq: 0,
            mono_usec: 77,
            cpu_usage_usec: 5_000_000,
            working_set_bytes: 10_485_760 - 2_097_152,
            active_file_bytes: 6_291_456,
            covered: BTreeSet::new(),
            actions_cpu_limited: false,
            throttled_usec: Some(123_456),
            pressure_some_total_usec: Some(987_654),
        }
    );
    write(&dir, "cpu.max", "8700000 100000\n");
    assert!(
        read_load(&dir, &*clock)
            .expect("limited")
            .actions_cpu_limited
    );
}

/// Catches a working set that wraps to a huge number when the inactive file memory is more
/// than the cgroup holds (the two files are read at different instants).
#[test]
fn the_working_set_is_floored_at_zero() {
    let dir = cgroup("load-floor");
    write(&dir, "memory.current", "1000\n");
    let (_, clock) = manual();
    assert_eq!(
        read_load(&dir, &*clock).expect("reading").working_set_bytes,
        0
    );
}

/// Catches a cgroup without a `cpu` controller or without pressure files failing the whole
/// reading, and a missing file the reading needs being read as 0.
#[test]
fn missing_files_are_absent_telemetry_or_a_clear_error() {
    let (_, clock) = manual();
    let dir = cgroup("load-missing-optional");
    std::fs::remove_file(dir.join("cpu.max")).expect("rm");
    std::fs::remove_file(dir.join("cpu.pressure")).expect("rm");
    write(&dir, "cpu.stat", "usage_usec 9\n");
    let load = read_load(&dir, &*clock).expect("reading");
    assert!(!load.actions_cpu_limited);
    assert_eq!(load.throttled_usec, None);
    assert_eq!(load.pressure_some_total_usec, None);

    for needed in ["cpu.stat", "memory.current", "memory.stat"] {
        let dir = cgroup(&format!("load-missing-{needed}"));
        std::fs::remove_file(dir.join(needed)).expect("rm");
        match read_load(&dir, &*clock) {
            Err(LoadError::Read { file, .. }) => assert_eq!(file, needed),
            other => panic!("{needed}: {other:?}"),
        }
    }

    // A file that cannot be read for another reason than being absent is an error too.
    let dir = cgroup("load-unreadable");
    std::fs::remove_file(dir.join("cpu.max")).expect("rm");
    std::fs::create_dir(dir.join("cpu.max")).expect("mkdir");
    assert!(matches!(
        read_load(&dir, &*clock),
        Err(LoadError::Read {
            file: "cpu.max",
            ..
        })
    ));

    // A garbled cpu.max or memory.current is malformed.
    let dir = cgroup("load-garbled");
    write(&dir, "cpu.max", "fast\n");
    assert!(matches!(
        read_load(&dir, &*clock),
        Err(LoadError::Malformed {
            file: "cpu.max",
            ..
        })
    ));
    assert_eq!(
        LoadError::Malformed {
            file: "cpu.max",
            what: "bad"
        }
        .to_string(),
        "cpu.max: bad"
    );
}

/// Catches the process clock going backwards or standing still across a sleep-free pair of
/// reads being treated as the wall clock.
#[test]
fn the_process_clock_does_not_go_back() {
    let clock = process_clock();
    let first = clock();
    assert!(clock() >= first);
}

/// Catches the first reading covering leases (there is no window yet), a lease started
/// after the previous reading being covered, an ended lease being covered, one that
/// started exactly at the previous reading being left out, and `seq` not counting up.
#[tokio::test]
async fn covered_leases_ran_the_whole_window() {
    let actions = cgroup("load-covered");
    let leases = crate::unit_dir("load-covered-leases");
    let (now, clock) = manual();
    let tracker = LoadTracker::new(clock);
    let start = |n: u64, at: u64| {
        now.store(at, Ordering::SeqCst);
        let dir = leases.join(format!("l{n}"));
        std::fs::create_dir(&dir).expect("mkdir");
        usage(&dir, 0);
        tracker.started(id(n), dir);
    };
    start(1, 10);
    start(2, 20);
    start(3, 30);
    now.store(100, Ordering::SeqCst);
    let first = tracker.reading(actions.to_path_buf()).await.expect("first");
    assert_eq!((first.seq, first.mono_usec), (1, 100));
    assert!(first.covered.is_empty());

    // Lease 4 starts after the first reading, lease 5 exactly at it, 2 ends.
    start(4, 150);
    start(5, 100);
    tracker.ended(id(2));
    now.store(200, Ordering::SeqCst);
    let second = tracker
        .reading(actions.to_path_buf())
        .await
        .expect("second");
    assert_eq!(second.seq, 2);
    assert_eq!(second.covered, BTreeSet::from([id(1), id(3), id(5)]));
}

/// Catches a failed reading counting as one (the next would then skip a sequence number
/// and measure its window from a reading nobody got).
#[tokio::test]
async fn a_failed_reading_changes_neither_the_sequence_nor_the_window() {
    let actions = cgroup("load-failed");
    let (now, clock) = manual();
    let tracker = LoadTracker::new(clock);
    now.store(5, Ordering::SeqCst);
    assert_eq!(
        tracker.reading(actions.to_path_buf()).await.expect("1").seq,
        1
    );
    std::fs::remove_file(actions.join("memory.current")).expect("rm");
    assert!(tracker.reading(actions.to_path_buf()).await.is_err());
    write(&actions, "memory.current", "1\n");
    assert_eq!(
        tracker.reading(actions.to_path_buf()).await.expect("2").seq,
        2
    );
}

/// Catches the peak being the last window or the mean rather than the largest, a lease
/// with one reading getting a window (or 0), a lease's peak given out twice, and a
/// lease whose cgroup is gone failing the reading.
#[tokio::test]
async fn a_lease_keeps_the_largest_window_it_had() {
    let actions = cgroup("load-peak");
    let leases = crate::unit_dir("load-peak-leases");
    let (now, clock) = manual();
    let tracker = LoadTracker::new(clock);
    let (one, two, gone) = (leases.join("one"), leases.join("two"), leases.join("gone"));
    for dir in [&one, &two, &gone] {
        std::fs::create_dir(dir).expect("mkdir");
        usage(dir, 0);
    }
    tracker.started(id(1), one.to_path_buf());
    tracker.started(id(2), two.to_path_buf());
    tracker.started(id(3), gone.to_path_buf());
    tracker.started(id(4), leases.join("never-made"));

    // Window 0 is the baseline. Lease 2 ends after it: no window.
    now.store(1_000_000, Ordering::SeqCst);
    tracker.reading(actions.to_path_buf()).await.expect("0");
    tracker.ended(id(2));
    // Window 1: lease 1 used 2 CPU-seconds in 1 s (2000 millicores).
    usage(&one, 2_000_000);
    now.store(2_000_000, Ordering::SeqCst);
    tracker.reading(actions.to_path_buf()).await.expect("1");
    // Window 2: 0.5 CPU-seconds in 1 s. The cgroup of lease 3 is gone.
    usage(&one, 2_500_000);
    std::fs::remove_file(gone.join("cpu.stat")).expect("rm");
    now.store(3_000_000, Ordering::SeqCst);
    tracker.reading(actions.to_path_buf()).await.expect("2");
    // Window 3: the counter went back (a restart of the cgroup): no window.
    usage(&one, 10);
    now.store(4_000_000, Ordering::SeqCst);
    tracker.reading(actions.to_path_buf()).await.expect("3");
    tracker.ended(id(1));
    tracker.ended(id(3));
    tracker.ended(id(4));

    assert_eq!(tracker.take_peak(id(1)), Some(2000));
    assert_eq!(tracker.take_peak(id(1)), None);
    assert_eq!(tracker.take_peak(id(2)), None);
    // Lease 3 sat idle for a window before its cgroup went: a window of 0, not none.
    assert_eq!(tracker.take_peak(id(3)), Some(0));
    assert_eq!(tracker.take_peak(id(4)), None);
    assert_eq!(tracker.take_peak(id(9)), None);
    // Ending a lease twice, or one never started, keeps nothing.
    tracker.ended(id(1));
    assert_eq!(tracker.take_peak(id(1)), None);
    assert!(format!("{tracker:?}").contains("LoadTracker"));
}
