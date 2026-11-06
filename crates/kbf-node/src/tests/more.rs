use super::*;

/// Catches: `--driver` taken once only (a node could not run two runtimes), not
/// required, a driver given twice set up twice, and the node's own driver set up
/// after another (native always comes first).
#[test]
fn drivers_come_once_each_with_the_nodes_own_first() {
    let cli = parse(&["--driver=fake", "--driver=native", "--driver=fake"]).expect("flags");
    assert_eq!(drivers(&cli), [Driver::Native, Driver::Fake]);
    let cli = parse(&["--driver=fake", "--driver=container"]).expect("flags");
    assert_eq!(drivers(&cli), [Driver::Container, Driver::Fake]);
    assert!(parse(&[]).is_err(), "no --driver");
}

/// Catches: a daemon that starts with two drivers serving one lease kind (each
/// Start of that kind would run on whichever comes first, silently), and one whose
/// refusal does not name both drivers and the kind. Native and fake both serve
/// `action`.
#[tokio::test]
async fn two_drivers_serving_one_kind_refuse_to_start() {
    let dir = scratch("two-drivers");
    tls_files(&dir);
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let cli = |drivers: &[&str]| {
        let flags = [
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            flag("ca-cert", &dir.join("ca.pem")),
            flag("cert", &dir.join("node.pem")),
            flag("key", &dir.join("node.key")),
            "--node-id=mac-1".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            flag("data-dir", &dir),
            flag("xcode-apps", &dir.join("Applications")),
        ];
        let drivers = drivers.iter().map(|d| format!("--driver={d}"));
        Cli::try_parse_from(flags.into_iter().chain(drivers)).expect("flags")
    };
    let both = cli(&["fake", "native"]);
    let native = || {
        let mut config = native_config(&both)?;
        // Not the runner's own (on macOS): the start sweeps them.
        config.user_folders = None;
        Ok(config)
    };
    let why = node(&both, native, &dir.join("no-xcrun"))
        .err()
        .expect("two drivers serve action")
        .to_string();
    assert_eq!(
        why,
        "drivers \"native\" and \"fake\" both serve lease kind action: each kind must \
         have one driver on a node"
    );
    let fakes = cli(&["fake", "fake"]);
    let (daemon, reports) = node(&fakes, native, Path::new("none")).expect("one fake runtime");
    assert!(reports.is_none());
    drop(daemon);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches: a plain-text `--cas` accepted (the daemon would send blobs where no
/// certificate admits them, or fall back to an unauthenticated port), a CAS URL of
/// another scheme accepted, `--cas` not required, and an https URL that does not
/// read the daemon's own TLS files.
#[tokio::test]
async fn the_cas_url_must_be_https() {
    for url in ["http://front:8981", "ftp://front", "front:8981"] {
        let cli = parse(&["--driver=native", &format!("--cas={url}")]).expect("flags");
        let why = transfer::engine(&cli).expect_err(url).to_string();
        assert!(why.contains("must be an https:// URL"), "{url}: {why}");
    }
    let none = parse(&["--driver=native"]).expect("flags");
    let why = transfer::engine(&none).expect_err("no --cas").to_string();
    assert!(why.contains("--cas is required"), "{why}");
    // https reads the daemon's TLS files, which these flags name but do not hold.
    let https = parse(&["--driver=native", "--cas=https://front:8981"]).expect("flags");
    let why = transfer::engine(&https)
        .expect_err("no TLS files")
        .to_string();
    assert!(why.contains("ca.pem"), "{why}");
}

/// The three liveness flags, each different from the others and from the defaults, so
/// that a default, or two values swapped, differs from what was asked.
const LIVENESS_FLAGS: [&str; 3] = [
    "--storage-dial-timeout-ms=7000",
    "--storage-ping-interval-ms=3000",
    "--storage-ping-timeout-ms=2000",
];

fn asked() -> Liveness {
    Liveness {
        connect: Duration::from_millis(7000),
        keepalive_interval: Duration::from_millis(3000),
        keepalive_timeout: Duration::from_millis(2000),
    }
}

/// Catches the storage-liveness flags stopping short of the nodes the daemon dials
/// storage through: `start` handing its plane the defaults instead of the checked
/// flags, or interval and timeout swapped on the way. Both the plane factory's
/// argument (`start_with`'s and `start`'s path through `start_over`) and the
/// `TlsNodes` that `farm` builds are read back.
#[test]
fn the_storage_liveness_flags_reach_the_plane_and_its_nodes() {
    assert_ne!(asked(), Liveness::default());
    let dir = scratch("liveness");
    tls_files(&dir);
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let mut flags = vec![
        "kbf-daemon".to_owned(),
        "--server=https://127.0.0.1:1".to_owned(),
        flag("ca-cert", &dir.join("ca.pem")),
        flag("cert", &dir.join("node.pem")),
        flag("key", &dir.join("node.key")),
        "--node-id=mac-1".to_owned(),
        // A driver that moves blobs, so that the plane is made.
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        flag("data-dir", &dir),
        flag("xcode-apps", &dir.join("Applications")),
    ];
    flags.extend(LIVENESS_FLAGS.map(str::to_owned));
    let cli = Cli::try_parse_from(flags).expect("flags");
    // `farm` builds the nodes from what it is given; what it holds is read back.
    // The lazy channel to --cas needs a runtime's context.
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    {
        let _context = runtime.enter();
        let (_, held) = farm(&cli, &Arc::new(MapHolder::new()), asked()).expect("the plane");
        assert_eq!(held, asked());
    }
    drop(runtime);
    // start_over checks the flags and hands the plane factory the checked values.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let record = Arc::clone(&seen);
    let handle = start_over(&cli, None, move |cli, liveness| {
        *record.lock().expect("lock") = Some(liveness);
        remote(cli)
    })
    .expect("start");
    drop(handle);
    assert_eq!(*seen.lock().expect("lock"), Some(asked()));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// The plane factory the retired-flags test hands `start_over` three times: one function
/// item, so the starts are one instantiation (the coverage ratchet counts the best
/// instantiation, which must run the map and no-map arms and the error return).
fn plane(cli: &Cli, _: Liveness) -> Result<Arc<kbf_transfer::Transfers>, Error> {
    remote(cli)
}

/// Starting with a retired flag logs the warning naming it, and starting without one logs
/// nothing of the kind: the call in `start_over` is what makes the flag's text reach a log.
#[test]
fn start_over_logs_the_retired_flag_warning() {
    #[derive(Clone, Default)]
    struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }
    let dir = scratch("retired-warn");
    tls_files(&dir);
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let argv = |extra: Option<String>| {
        let mut all = vec![
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            flag("ca-cert", &dir.join("ca.pem")),
            flag("cert", &dir.join("node.pem")),
            flag("key", &dir.join("node.key")),
            "--node-id=mac-1".to_owned(),
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            flag("data-dir", &dir),
            flag("xcode-apps", &dir.join("Applications")),
        ];
        all.extend(extra);
        Cli::try_parse_from(all).expect("flags")
    };
    let logged = |cli: &Cli| {
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            drop(start_over(cli, None, plane).expect("start"));
        });
        let bytes = sink.0.lock().expect("lock").clone();
        String::from_utf8(bytes).expect("utf8")
    };
    let with = logged(&argv(Some(flag("update-dir", &dir.join("update")))));
    assert!(with.contains("--update-dir is no longer read"), "{with}");
    let without = logged(&argv(None));
    std::io::Write::flush(&mut Sink::default()).expect("the sink flushes");
    assert!(!without.contains("no longer read"), "{without}");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// The retired `--update-dir` and `--orders-dir` are still taken, with or without a
/// storage map and `--orders-dir` without `--update-dir`: a node whose `kbf.toml` or
/// plist still carries them starts, and nothing is refused in the all-in-one binary's
/// in-process daemon either. A start that fails inside `node_over` still returns its
/// error.
#[test]
fn the_retired_update_flags_are_accepted_with_or_without_a_map() {
    let dir = scratch("update-dir");
    tls_files(&dir);
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let base = vec![
        "kbf-daemon".to_owned(),
        "--server=https://127.0.0.1:1".to_owned(),
        flag("ca-cert", &dir.join("ca.pem")),
        flag("cert", &dir.join("node.pem")),
        flag("key", &dir.join("node.key")),
        "--node-id=mac-1".to_owned(),
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        flag("data-dir", &dir),
        flag("xcode-apps", &dir.join("Applications")),
    ];
    let mut asked = base.clone();
    asked.push(flag("update-dir", &dir.join("update")));
    asked.push(flag("orders-dir", &dir.join("orders")));
    let with_dirs = Cli::try_parse_from(asked).expect("flags");
    let mut orders_only = base.clone();
    orders_only.push(flag("orders-dir", &dir.join("orders")));
    let orders_only = Cli::try_parse_from(orders_only).expect("--orders-dir alone parses");
    let started = start_over(&with_dirs, Some(Arc::new(MapHolder::new())), plane);
    drop(started.expect("a daemon with a map takes the retired flags"));
    drop(start_over(&with_dirs, None, plane).expect("so does the in-process daemon"));
    drop(start_over(&orders_only, None, plane).expect("--orders-dir needs no --update-dir"));
    assert!(!dir.join("update").exists(), "the flags create nothing");
    assert!(!dir.join("orders").exists(), "the flags create nothing");
    // The last start fails inside `node_over`: this instantiation also takes the
    // error return of that call, so it is the one the ratchet counts as fully run.
    let mut broken = base.clone();
    broken.push("--lease-placement=numa-node".to_owned());
    let broken = Cli::try_parse_from(broken).expect("flags");
    let why = start_over(&broken, None, plane).err().expect("refused");
    assert!(why.to_string().contains("only --driver container"), "{why}");
    // `--actions-cpu-max` other than `auto` needs the container driver too.
    for mode in ["on", "off"] {
        let mut broken = base.clone();
        broken.push(format!("--actions-cpu-max={mode}"));
        let broken = Cli::try_parse_from(broken).expect("flags");
        let why = start_over(&broken, None, plane).err().expect("refused");
        let why = why.to_string();
        assert!(why.contains(&format!("--actions-cpu-max={mode}")), "{why}");
        assert!(why.contains("only --driver container"), "{why}");
    }
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches a daemon that serves on with a storage map none of whose members it can
/// reach: `start` and `start_member` not arming the check on the map they keep (the
/// daemon would then run, take leases and fail every blob call), or the error not
/// reaching the caller of `serve_until`. A healthy start, whose member answers, is
/// not ended by it.
#[test]
fn serving_ends_with_an_error_when_no_storage_member_answers() {
    use kbf_map::{Map, Member, MemberId, MemberState, Rule};

    let dir = scratch("reach");
    tls_files(&dir);
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let cli = Cli::try_parse_from([
        "kbf-daemon".to_owned(),
        "--server=https://127.0.0.1:1".to_owned(),
        flag("ca-cert", &dir.join("ca.pem")),
        flag("cert", &dir.join("node.pem")),
        flag("key", &dir.join("node.key")),
        "--node-id=mac-1".to_owned(),
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        flag("data-dir", &dir),
        flag("xcode-apps", &dir.join("Applications")),
    ])
    .expect("flags");
    let map_at = |addr: String| {
        let member = Member {
            id: MemberId(1),
            addr,
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
            boot: 1,
        };
        Map::new(1, vec![member], Rule::Rendezvous, 1, 0, Vec::new()).expect("a map")
    };
    // A low port nothing listens on: refused at once.
    let dead = "127.0.0.1:1".to_owned();

    // One round, so that the refusal is at once and not after the default window.
    let once = kbf_daemon::reach::Plan {
        attempts: 1,
        ..kbf_daemon::reach::Plan::DEFAULT
    };
    let refused = |node: &mut NodeHandle| {
        let reach = node.reach.as_mut().expect("armed");
        reach.plan = once;
        assert!(reach.maps.offer(map_at(dead.clone())));
    };
    // The shutdown is bounded: a check that is ignored fails this test in seconds
    // (the daemon is told to stop and returns Ok) instead of hanging the run.
    let mut node = start(&cli).expect("starts");
    refused(&mut node);
    let error = node
        .serve_until(async { tokio::time::sleep(Duration::from_secs(5)).await })
        .expect_err("no member answers");
    assert!(error.to_string().contains(&dead), "{error}");

    // `serve`, the entry the binary and the SIGTERM path use, ends the same way. It
    // runs on a thread the test waits on for 5 s: a `serve` that ignores the check
    // would wait for a signal for ever, and fails here instead of hanging the run.
    let mut node = start(&cli).expect("starts");
    refused(&mut node);
    let (ended, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        ended.send(node.serve().map_err(|e| e.to_string())).ok();
    });
    let served = result.recv_timeout(Duration::from_secs(5));
    assert!(served.is_ok(), "serve did not end: it ignores the check");
    let error = served
        .expect("ended")
        .expect_err("serve ends with the refusal");
    assert!(error.contains(&dead), "{error}");

    // The same map with a member that answers: the daemon serves until told to stop.
    let live = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let node = start(&cli).expect("starts");
    let maps = Arc::clone(&node.reach.as_ref().expect("armed").maps);
    assert!(maps.offer(map_at(live.local_addr().expect("addr").to_string())));
    node.serve_until(async { tokio::time::sleep(Duration::from_millis(500)).await })
        .expect("a reachable member does not end it");
    drop(live);

    // A daemon that keeps no map (the in-process one) has no check to end it.
    start_with(&cli, remote)
        .expect("starts")
        .serve_until(async { tokio::time::sleep(Duration::from_millis(100)).await })
        .expect("nothing to refuse");

    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches a daemon beside another half that does not drain on the first signal
/// (`serve_beside` running the daemon with `run`, which ignores signals), and one that
/// keeps running when the half beside it has stopped. An idle node with a signal
/// waiting returns at once, and so does one whose other half has stopped, with no
/// signal; each runs on a thread the test waits on for 5 s, so a defect fails here
/// instead of hanging the run.
#[test]
fn a_daemon_beside_another_half_drains_on_a_signal_and_stops_when_the_other_does() {
    let dir = scratch("drain-on");
    tls_files(&dir);
    let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
    let cli = Cli::try_parse_from([
        "kbf-daemon".to_owned(),
        "--server=https://127.0.0.1:1".to_owned(),
        flag("ca-cert", &dir.join("ca.pem")),
        flag("cert", &dir.join("node.pem")),
        flag("key", &dir.join("node.key")),
        "--node-id=mac-1".to_owned(),
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        flag("data-dir", &dir),
        flag("xcode-apps", &dir.join("Applications")),
    ])
    .expect("flags");
    let ends = |signalled: bool| {
        let node = start_with(&cli, remote).expect("starts");
        let (signals, received) = tokio::sync::mpsc::unbounded_channel();
        if signalled {
            signals.send(()).expect("sent");
        }
        let (ended, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let other = async move {
                if signalled {
                    std::future::pending::<()>().await;
                }
            };
            ended.send(node.serve_beside(received, other).is_ok()).ok();
            drop(signals);
        });
        result.recv_timeout(Duration::from_secs(5))
    };
    assert_eq!(ends(true), Ok(true), "no drain on the first signal");
    assert_eq!(ends(false), Ok(true), "the other half's stop was ignored");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches a daemon that starts with storage-liveness flags it cannot honour: a zero
/// value, or a ping timeout past the interval, is refused by `start` (and by
/// `start_with`, as the same check) before any driver is set up, naming the flag.
#[test]
fn start_refuses_unusable_storage_liveness_flags() {
    for (flag, said) in [
        (
            "--storage-dial-timeout-ms=0",
            "--storage-dial-timeout-ms must be at least 1",
        ),
        (
            "--storage-ping-interval-ms=0",
            "--storage-ping-interval-ms must be at least 1",
        ),
        (
            "--storage-ping-timeout-ms=0",
            "--storage-ping-timeout-ms must be at least 1",
        ),
        (
            "--storage-ping-timeout-ms=10001",
            "is longer than --storage-ping-interval-ms",
        ),
        (
            "--storage-dial-timeout-ms=3600001",
            "--storage-dial-timeout-ms must be at most 3600000",
        ),
        (
            "--storage-ping-interval-ms=3600001",
            "--storage-ping-interval-ms must be at most 3600000",
        ),
        (
            "--storage-ping-timeout-ms=3600001",
            "--storage-ping-timeout-ms must be at most 3600000",
        ),
    ] {
        let cli = parse(&["--driver=fake", flag]).expect("flags parse");
        for refused in [start(&cli).err(), start_with(&cli, remote).err()] {
            let why = refused.expect(flag).to_string();
            assert!(why.contains(said), "{flag}: {why}");
        }
    }
}
