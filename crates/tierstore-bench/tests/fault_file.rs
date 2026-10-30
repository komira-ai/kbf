//! The file switch against a real fault proxy over loopback: a mode set through the control file
//! is in force when `set_mode` returns, and the request count is the proxy's. No test reads the
//! wall clock; the waits are for the proxy's own counters.

use std::time::Duration;

use kbf_testkit::dir::TestDir;
use tierstore_bench::cells::engine::FaultSwitch;
use tierstore_bench::error::BenchError;
use tierstore_bench::faultfile::FileSwitch;
use tierstore_bench::proxy::{FaultMode, ProxyConfig, spawn};

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
