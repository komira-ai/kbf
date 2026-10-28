//! The busy rate from a cgroup's CPU counter.

use std::path::PathBuf;

use super::*;

/// A fresh `cpu.stat` path beside the test binary (inside the target directory).
fn stat(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-daemon-unit")
        .join("busy")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir.join("cpu.stat")
}

fn at(secs: u64) -> Moment {
    Moment::from_origin(Duration::from_secs(secs))
}

fn write(path: &PathBuf, usage: u64) {
    std::fs::write(
        path,
        format!("usage_usec {usage}\nuser_usec 1\nsystem_usec 1\nnr_periods 0\n"),
    )
    .expect("write cpu.stat");
}

/// Catches a counter read from the wrong line (`user_usec` for `usage_usec`), a line that
/// is not a number taken as 0, and a missing line taken as idle.
#[test]
fn usage_is_the_usage_usec_line_of_cpu_stat() {
    assert_eq!(
        usage_usec("user_usec 9\nusage_usec 4812345\nsystem_usec 7\n"),
        Some(4_812_345)
    );
    assert_eq!(usage_usec("user_usec 9\nsystem_usec 7\n"), None);
    assert_eq!(usage_usec("usage_usec lots\n"), None);
    assert_eq!(usage_usec("usage_usec\n"), None);
    assert_eq!(usage_usec(""), None);
}

/// Catches a rate off by a factor of 1000 or by the window length: 20 CPU-seconds used
/// over 10 s is 2 CPUs, 2000 CPU-ms per second; and a zero window has no rate.
#[test]
fn the_rate_is_cpu_milliseconds_per_second() {
    assert_eq!(
        busy_millis(20_000_000, Duration::from_secs(10)),
        Some(2_000)
    );
    assert_eq!(busy_millis(0, Duration::from_secs(10)), Some(0));
    assert_eq!(
        busy_millis(1_500_000, Duration::from_millis(500)),
        Some(3_000)
    );
    assert_eq!(busy_millis(5, Duration::ZERO), None);
    assert_eq!(
        busy_millis(u64::MAX, Duration::from_micros(1)),
        Some(u64::MAX)
    );
}

/// Catches a first window that reports a rate from nothing (it has no start), a window
/// measured from the first reading instead of the previous one, and a rate that includes
/// the earlier window's CPU time.
#[test]
fn each_window_is_measured_from_the_previous_reading() {
    let path = stat("windows");
    let mut meter = CpuMeter::new(path.clone());
    write(&path, 1_000_000);
    assert_eq!(meter.sample(at(0)), None, "the first reading has no window");
    write(&path, 11_000_000);
    assert_eq!(meter.sample(at(5)), Some(2_000));
    write(&path, 11_000_000);
    assert_eq!(meter.sample(at(10)), Some(0));
    write(&path, 41_000_000);
    assert_eq!(meter.sample(at(20)), Some(3_000));
}

/// Catches a counter that went backwards (the cgroup was made again) reported as a huge
/// or zero rate, and a window after a gap measured across the gap: the reading after a
/// failed or backwards one starts a new window.
#[test]
fn a_backwards_or_unreadable_counter_gives_no_rate_and_restarts_the_window() {
    let path = stat("gaps");
    let mut meter = CpuMeter::new(path.clone());
    write(&path, 50_000_000);
    assert_eq!(meter.sample(at(0)), None);
    write(&path, 1_000_000);
    assert_eq!(meter.sample(at(5)), None, "backwards");
    write(&path, 6_000_000);
    assert_eq!(meter.sample(at(10)), Some(1_000), "from the new value");
    std::fs::remove_file(&path).expect("remove cpu.stat");
    assert_eq!(meter.sample(at(15)), None, "unreadable");
    write(&path, 9_000_000);
    assert_eq!(
        meter.sample(at(20)),
        None,
        "the window after a gap starts over"
    );
    write(&path, 14_000_000);
    assert_eq!(meter.sample(at(25)), Some(1_000));
}

/// Catches two readings at the same instant (a clock that did not move) dividing by zero.
#[test]
fn two_readings_at_one_moment_give_no_rate() {
    let path = stat("same_moment");
    let mut meter = CpuMeter::new(path.clone());
    write(&path, 1_000_000);
    assert_eq!(meter.sample(at(3)), None);
    write(&path, 2_000_000);
    assert_eq!(meter.sample(at(3)), None);
}
