//! Unit tests of the fault proxy: the pieces that need no socket, and the handle.

use super::*;
use kbf_testkit::dir::TestDir;

fn config(control: Option<PathBuf>, result_dir: Option<PathBuf>, allow: bool) -> ProxyConfig {
    ProxyConfig {
        control,
        result_dir,
        allow_date_rewrite: allow,
        ..ProxyConfig::new("127.0.0.1:0".parse().unwrap(), "http://127.0.0.1:9")
    }
}

fn ticker(config: &ProxyConfig) -> Ticker {
    Ticker {
        shared: Arc::new(Shared::new("127.0.0.1:9".to_owned(), config)),
        applied: None,
        written: None,
    }
}

/// The mode at `at`, read from the state with no clock.
fn mode_at(ticker: &Ticker, at: Instant) -> FaultMode {
    ticker.shared.mode.borrow().effective(at)
}

/// Catches a mode that does not parse back from the text a flag gives it, and a printed
/// mode that does not read back as itself (the counters file prints it).
#[test]
fn every_mode_parses_from_its_flag_text_and_prints_as_it() {
    let cases = [
        ("pass", FaultMode::Pass),
        ("refuse", FaultMode::Refuse),
        ("blackhole", FaultMode::Blackhole),
        ("latency=500", FaultMode::Latency { ms: 500 }),
        (
            "bandwidth=10485760",
            FaultMode::Bandwidth {
                bytes_per_sec: 10_485_760,
            },
        ),
        ("skew=-40", FaultMode::DateSkew { secs: -40 }),
    ];
    for (text, mode) in cases {
        assert_eq!(text.parse::<FaultMode>(), Ok(mode), "{text}");
        assert_eq!(mode.to_string(), text);
    }
    assert_eq!(
        "delay=7".parse::<FaultMode>(),
        Ok(FaultMode::Latency { ms: 7 })
    );
}

/// Catches a malformed mode being taken for a fault, which would run an arm unfaulted under
/// a faulted label.
#[test]
fn a_malformed_mode_is_refused() {
    for text in [
        "",
        "slow",
        "latency",
        "latency=fast",
        "bandwidth=-1",
        "skew=1.5",
        "x=1",
    ] {
        assert_eq!(
            text.parse::<FaultMode>(),
            Err(ModeError(text.to_owned())),
            "{text}"
        );
    }
}

/// Catches a fault that never ends, one that ends early, `pass` being given an end, an
/// expiry counted twice, and an end so far away that the sum overflows an `Instant`.
#[test]
fn a_fault_falls_back_to_pass_at_its_time_to_live_once() {
    let base = Instant::now();
    let mut state = ModeState::new();
    let latency = FaultMode::Latency { ms: 5 };
    state.set(latency, Some(Duration::from_secs(10)), base);
    assert_eq!(state.effective(base), latency);
    assert_eq!(state.effective(base + Duration::from_secs(9)), latency);
    assert_eq!(
        state.effective(base + Duration::from_secs(10)),
        FaultMode::Pass
    );
    assert!(!state.settle(base + Duration::from_secs(9)));
    assert!(state.settle(base + Duration::from_secs(10)));
    assert!(!state.settle(base + Duration::from_secs(11)));
    assert_eq!(
        (state.mode, state.expired, state.changes),
        (FaultMode::Pass, 1, 1)
    );

    state.set(FaultMode::Pass, Some(Duration::from_secs(1)), base);
    assert_eq!(state.until, None);
    state.set(latency, Some(Duration::MAX), base);
    assert_eq!(state.until, None);
    state.set(latency, None, base);
    assert_eq!(
        state.effective(base + Duration::from_secs(1 << 30)),
        latency
    );
}

/// Catches a cap that holds a frame for the wrong time: the time is the bytes over the
/// rate, a rate of zero cannot divide, and a size past a `u64` of nanoseconds saturates.
#[test]
fn a_frame_is_held_for_its_bytes_over_the_rate() {
    assert_eq!(pace(1000, 1000), Duration::from_secs(1));
    assert_eq!(pace(500, 1000), Duration::from_millis(500));
    assert_eq!(pace(0, 1000), Duration::ZERO);
    assert_eq!(pace(3, 0), Duration::from_secs(3));
    assert_eq!(pace(usize::MAX, 1), Duration::from_nanos(u64::MAX));
}

/// Catches an upstream the proxy would dial wrong: it must be `http://host:port`.
#[test]
fn the_upstream_must_be_a_plain_http_host_and_port() {
    assert_eq!(
        upstream_authority("http://store:9000").unwrap(),
        "store:9000"
    );
    assert_eq!(
        upstream_authority("http://192.0.2.1:9000/").unwrap(),
        "192.0.2.1:9000"
    );
    assert_eq!(upstream_authority("http://[::1]:80").unwrap(), "[::1]:80");
    for bad in [
        "https://store:9000",
        "store:9000",
        "http://store",
        "http://:9000",
        "http://store:port",
        "http://store:9000/bucket",
        "http://user@store:9000",
        "http://store:9000?x",
        "http://sto re:9000",
        "http://",
    ] {
        assert!(
            matches!(upstream_authority(bad), Err(BenchError::Proxy(_))),
            "{bad}"
        );
    }
}

/// Catches a control line read wrong: the mode, an optional time to live, nothing else.
#[test]
fn a_control_line_is_a_mode_and_an_optional_time_to_live() {
    let mode = FaultMode::Latency { ms: 5 };
    assert_eq!(parse_control("latency=5"), Ok((mode, None)));
    assert_eq!(
        parse_control("  latency=5   ttl=30\n"),
        Ok((mode, Some(Duration::from_secs(30))))
    );
    for bad in [
        "",
        "\n",
        "slow",
        "latency=5 30",
        "latency=5 ttl=0",
        "latency=5 ttl=x",
        "latency=5 ttl=3 more",
    ] {
        assert!(parse_control(bad).is_err(), "{bad:?}");
    }
}

/// Catches a mode applied that must not be: a bandwidth of zero (a request that never ends)
/// and a date rewrite nobody asked for.
#[test]
fn a_mode_the_proxy_will_not_apply_is_refused() {
    let plain = config(None, None, false);
    let state = ticker(&plain);
    let now = Instant::now();
    for bad in [
        FaultMode::Bandwidth { bytes_per_sec: 0 },
        FaultMode::DateSkew { secs: 1 },
    ] {
        assert!(matches!(
            state.shared.apply_at(bad, None, now),
            Err(BenchError::Proxy(_))
        ));
    }
    assert_eq!(state.shared.mode.borrow().changes, 0);
    let allowing = ticker(&config(None, None, true));
    allowing
        .shared
        .apply_at(FaultMode::DateSkew { secs: 1 }, None, now)
        .unwrap();
    assert_eq!(allowing.shared.mode.borrow().changes, 1);
}

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(*name, HeaderValue::from_str(value).unwrap());
    }
    map
}

/// Catches a date rewrite that moves the wrong header, the wrong way, or breaks a value it
/// cannot read: both dates move, a value that is no date and a shift past the range stay.
#[test]
fn the_skew_moves_date_and_last_modified_and_nothing_else() {
    let mut map = headers(&[
        ("date", "Sun, 06 Nov 1994 08:49:37 GMT"),
        ("last-modified", "Sun, 06 Nov 1994 08:49:37 GMT"),
        ("etag", "\"x\""),
    ]);
    skew_headers(&mut map, 60);
    assert_eq!(map["date"], "Sun, 06 Nov 1994 08:50:37 GMT");
    assert_eq!(map["last-modified"], "Sun, 06 Nov 1994 08:50:37 GMT");
    assert_eq!(map["etag"], "\"x\"");
    skew_headers(&mut map, -3660);
    assert_eq!(map["date"], "Sun, 06 Nov 1994 07:49:37 GMT");

    let mut odd = headers(&[("date", "yesterday")]);
    skew_headers(&mut odd, 60);
    assert_eq!(odd["date"], "yesterday");
    assert!(!odd.contains_key("last-modified"));

    let mut far = headers(&[("date", "Sun, 06 Nov 1994 08:49:37 GMT")]);
    for secs in [i64::MIN, -800_000_000, i64::MAX, 300_000_000_000] {
        skew_headers(&mut far, secs);
        assert_eq!(far["date"], "Sun, 06 Nov 1994 08:49:37 GMT", "{secs}");
    }
}

/// Catches a hop-by-hop header forwarded, and an end-to-end one (Host, the signature)
/// dropped with it.
#[test]
fn only_hop_by_hop_headers_are_stripped() {
    let mut map = headers(&[
        ("connection", "close"),
        ("keep-alive", "1"),
        ("transfer-encoding", "chunked"),
        ("te", "trailers"),
        ("host", "127.0.0.1:1"),
        ("authorization", "AWS4-HMAC-SHA256 x"),
    ]);
    strip_hop_by_hop(&mut map);
    let mut left: Vec<_> = map.keys().map(|k| k.as_str().to_owned()).collect();
    left.sort();
    assert_eq!(left, ["authorization", "host"]);
}

/// Catches a control file applied twice, one never applied, a bad one that changes the
/// mode, one counted on every tick, and a missing file taken for an error.
#[test]
fn the_control_file_is_applied_when_it_changes_and_a_bad_one_is_counted_once() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-bench-proxy-control");
    let path = dir.path().join("mode");
    let mut t = ticker(&config(Some(path.clone()), None, false));
    let base = Instant::now();
    let at = |s: u64| base + Duration::from_secs(s);

    t.read_control(base);
    assert_eq!(t.shared.counters().mode_changes, 0);
    assert_eq!(t.shared.counters().control_rejected, 0);

    std::fs::write(&path, "latency=5 ttl=30\n").unwrap();
    t.read_control(base);
    assert_eq!(mode_at(&t, base), FaultMode::Latency { ms: 5 });
    assert_eq!(mode_at(&t, at(31)), FaultMode::Pass);
    assert_eq!(t.shared.counters().mode_changes, 1);

    // Unchanged: read again, not applied again, so a mode set by hand since stays.
    t.shared.apply_at(FaultMode::Blackhole, None, base).unwrap();
    t.read_control(base);
    assert_eq!(mode_at(&t, base), FaultMode::Blackhole);
    assert_eq!(t.shared.counters().mode_changes, 2);

    std::fs::write(&path, "refuse").unwrap();
    t.read_control(base);
    assert_eq!(mode_at(&t, at(1000)), FaultMode::Refuse);

    for bad in ["slow", "skew=5", "bandwidth=0"] {
        std::fs::write(&path, bad).unwrap();
        t.read_control(base);
        t.read_control(base);
    }
    assert_eq!(mode_at(&t, at(1000)), FaultMode::Refuse);
    assert_eq!(t.shared.counters().control_rejected, 3);
}

/// Catches a control path that cannot be read being ignored: it is counted, once.
#[test]
fn an_unreadable_control_file_is_counted_once() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-bench-proxy-unreadable");
    let mut t = ticker(&config(Some(dir.path().to_path_buf()), None, false));
    let base = Instant::now();
    t.read_control(base);
    t.read_control(base);
    assert_eq!(t.shared.counters().control_rejected, 1);
    assert_eq!(t.shared.counters().mode_changes, 0);
}

/// Catches an expired fault that the tick does not settle, which would leave the counters
/// naming a fault that ended.
#[test]
fn the_tick_settles_an_expired_fault_and_counts_it() {
    let mut t = ticker(&config(None, None, false));
    let base = Instant::now();
    t.shared
        .apply_at(FaultMode::Refuse, Some(Duration::from_secs(5)), base)
        .unwrap();
    t.tick(base + Duration::from_secs(4));
    assert_eq!(t.shared.counters().expired, 0);
    t.tick(base + Duration::from_secs(5));
    let counters = t.shared.counters();
    assert_eq!((counters.expired, counters.mode_changes), (1, 1));
    assert_eq!(t.shared.mode.borrow().mode, FaultMode::Pass);
}

/// Catches a counters file that is missing, rewritten when nothing changed, not rewritten
/// when something did, or that names more than the role; and a failed write that is not
/// counted.
#[test]
fn the_counters_are_written_when_they_change_and_a_failed_write_is_counted() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-bench-proxy-counters");
    let mut t = ticker(&config(None, Some(dir.path().to_path_buf()), false));
    let file = dir.path().join(COUNTERS_FILE);
    let read =
        |file: &Path| serde_json::from_slice::<Counters>(&std::fs::read(file).unwrap()).unwrap();

    t.tick(Instant::now());
    let first = read(&file);
    assert_eq!(first.role, "proxy");
    assert_eq!(first.schema_version, COUNTERS_SCHEMA_VERSION);
    assert_eq!(first.mode, "pass");

    std::fs::remove_file(&file).unwrap();
    t.tick(Instant::now());
    assert!(!file.exists(), "unchanged counters were written again");

    t.shared.bump(&t.shared.requests, 3);
    t.tick(Instant::now());
    assert_eq!(read(&file).requests, 3);

    let mut broken = ticker(&config(None, Some(dir.path().join("none")), false));
    broken.tick(Instant::now());
    assert_eq!(broken.shared.counters().write_failures, 1);
    assert!(matches!(broken.flush(), Err(BenchError::Io { .. })));
}

/// Catches a handle that cannot be asked for its state in process.
#[tokio::test]
async fn the_handle_sets_a_mode_and_reports_it() {
    let proxy = spawn(config(None, None, false)).await.unwrap();
    assert_eq!(proxy.mode(), FaultMode::Pass);
    proxy.set_mode(FaultMode::Refuse).unwrap();
    assert_eq!(proxy.mode(), FaultMode::Refuse);
    proxy
        .set_mode_for(FaultMode::Blackhole, Some(Duration::from_secs(3600)))
        .unwrap();
    assert_eq!(proxy.mode(), FaultMode::Blackhole);
    assert!(proxy.set_mode(FaultMode::DateSkew { secs: 1 }).is_err());
    assert_eq!(proxy.request_count(), 0);
    let counters = proxy.stop().await.unwrap();
    assert_eq!(
        (counters.mode_changes, counters.mode.as_str()),
        (3, "blackhole")
    );
}
