//! The file switch against a real fault proxy over loopback: a mode set through the control file
//! is in force when `set_mode` returns, and the request count is the proxy's. No test reads the
//! wall clock; the waits are for the proxy's own counters.

use std::time::Duration;

use kbf_testkit::dir::TestDir;
use tierstore_bench::cells::engine::FaultSwitch;
use tierstore_bench::error::BenchError;
use tierstore_bench::faultfile::FileSwitch;
use tierstore_bench::proxy::{COUNTERS_FILE, Counters, FaultMode, ProxyConfig, spawn};

fn config(dir: &TestDir) -> ProxyConfig {
    ProxyConfig {
        control: Some(dir.path().join("control")),
        result_dir: Some(dir.path().to_path_buf()),
        tick: Duration::from_millis(10),
        ..ProxyConfig::new("127.0.0.1:0".parse().unwrap(), "http://127.0.0.1:9")
    }
}

/// Catches a switch that returns before the proxy is in the mode (the next request would be
/// measured unfaulted), and one that sets a mode other than the one asked: after each call the
/// proxy's own mode is the one set. Two threads, since the call waits on the proxy's tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mode_set_through_the_file_is_in_force_when_the_call_returns() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-faultfile");
    let proxy = spawn(config(&dir)).await.unwrap();
    let switch = FileSwitch::new(dir.path().join("control"), dir.path());
    for mode in [
        FaultMode::Refuse,
        FaultMode::Latency { ms: 5 },
        FaultMode::Pass,
    ] {
        switch.set_mode(mode).unwrap();
        assert_eq!(proxy.mode(), mode);
    }
    proxy.stop().await.unwrap();
}

/// Catches a request count that is not the proxy's: a proxy that has sent nothing on counts 0,
/// and counters that cannot be read count 0 too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_request_count_is_the_proxys_counters() {
    let dir = TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        "tierstore-bench-faultfile-count",
    );
    let silent = FileSwitch::new(dir.path().join("control"), dir.path());
    assert_eq!(silent.request_count(), 0);
    let proxy = spawn(config(&dir)).await.unwrap();
    silent.set_mode(FaultMode::Pass).unwrap();
    assert_eq!(silent.request_count(), 0);
    proxy.stop().await.unwrap();
}

/// Catches counts by method that are lost between the proxy's file and the switch: the switch
/// reports the map of the counters file as written, and an empty one for counters it cannot read.
#[test]
fn the_method_counts_are_the_proxys_counters() {
    let dir = TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        "tierstore-bench-faultfile-methods",
    );
    let switch = FileSwitch::new(dir.path().join("control"), dir.path());
    assert!(switch.method_counts().is_empty());
    let counters = Counters {
        requests: 3,
        requests_by_method: [("HEAD".to_owned(), 2), ("PUT".to_owned(), 1)].into(),
        ..Counters::default()
    };
    std::fs::write(
        dir.path().join(COUNTERS_FILE),
        serde_json::to_vec(&counters).unwrap(),
    )
    .unwrap();
    assert_eq!(switch.method_counts(), counters.requests_by_method);
}

/// Catches a fault arm measured with no proxy listening to the file: with no proxy the call
/// fails, and so does a control file that cannot be written.
#[test]
fn no_proxy_means_an_error_and_not_a_healthy_arm() {
    let dir = TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        "tierstore-bench-faultfile-none",
    );
    let none =
        FileSwitch::new(dir.path().join("control"), dir.path()).settling_within(Duration::ZERO);
    let err = none.set_mode(FaultMode::Refuse).unwrap_err();
    assert!(matches!(err, BenchError::Proxy(_)), "{err}");
    let nowhere = FileSwitch::new(dir.path().join("absent").join("control"), dir.path());
    let err = nowhere.set_mode(FaultMode::Refuse).unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err}");
}

/// Catches a control file that would leave a dead run's fault in force for ever (the line had no
/// time to live), and one written in place (the proxy could read half a line): with no proxy
/// listening the call fails, and what it wrote is the mode with its time to live, whole seconds,
/// at least one; the directory holds the control file alone.
#[test]
fn every_line_carries_a_time_to_live_and_is_written_whole() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-faultfile-ttl");
    let control = dir.path().join("control");
    let line = |switch: &FileSwitch, mode| {
        switch.set_mode(mode).unwrap_err();
        std::fs::read_to_string(&control).unwrap()
    };
    let make = || FileSwitch::new(control.clone(), dir.path()).settling_within(Duration::ZERO);
    assert_eq!(make().ttl(), tierstore_bench::faultfile::DEFAULT_TTL);
    assert_eq!(line(&make(), FaultMode::Refuse), "refuse ttl=300\n");
    let short = make().with_ttl(Duration::from_secs(42));
    assert_eq!(
        line(&short, FaultMode::Latency { ms: 5 }),
        "latency=5 ttl=42\n"
    );
    assert_eq!(line(&short, FaultMode::Pass), "pass ttl=42\n");
    let tiny = make().with_ttl(Duration::from_millis(1500));
    assert_eq!(line(&tiny, FaultMode::Blackhole), "blackhole ttl=1\n");
    let zero = make().with_ttl(Duration::ZERO);
    assert_eq!(line(&zero, FaultMode::Refuse), "refuse ttl=1\n");

    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["control"], "no temporary file is left beside it");
}

/// Catches a fault set through the file that the proxy does not end by itself: the proxy takes
/// `refuse ttl=...` from the switch and reports the mode, so the line the switch writes is one the
/// proxy's control parser accepts (a rejected line would leave the proxy healthy and the call
/// would time out instead).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_proxy_accepts_the_line_with_its_time_to_live() {
    let dir = TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        "tierstore-bench-faultfile-ttl-proxy",
    );
    let proxy = spawn(config(&dir)).await.unwrap();
    let switch =
        FileSwitch::new(dir.path().join("control"), dir.path()).with_ttl(Duration::from_secs(3600));
    switch.set_mode(FaultMode::Refuse).unwrap();
    assert_eq!(proxy.mode(), FaultMode::Refuse);
    proxy.stop().await.unwrap();
}
