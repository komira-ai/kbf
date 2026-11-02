use clap::Parser;
use kbf_daemon::DriverReport;
use kbf_proto::worker::XcodeState;

use super::*;

mod more;

/// The first report the watch sends with no Xcode left not surveyed, waited for up
/// to a minute.
pub(crate) async fn surveyed(reports: &mut Reports) -> DriverReport {
    let pending = |r: &DriverReport| {
        r.xcodes
            .iter()
            .any(|x| x.state() == XcodeState::NotSurveyed)
    };
    let surveyed = reports.wait_for(|r| !pending(r));
    tokio::time::timeout(Duration::from_secs(60), surveyed)
        .await
        .expect("the first survey ends in time")
        .expect("the watch runs")
        .clone()
}

/// A CAS no test here reaches.
#[derive(Debug)]
struct NoCas;

impl Cas for NoCas {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(
        &self,
        digest: &kbf_proto::reapi::Digest,
    ) -> Result<Vec<u8>, kbf_transfer::CasError> {
        Err(kbf_transfer::CasError::Missing(digest.hash.clone()))
    }

    async fn put(&self, _: Vec<u8>) -> Result<kbf_proto::reapi::Digest, kbf_transfer::CasError> {
        Err(kbf_transfer::CasError::Unavailable(
            "-".into(),
            "no CAS".into(),
        ))
    }
}

/// Catches a driver watch whose slots' part is not wired to the runtime (the number of
/// slots that can run a lease would again wait for an Xcode survey to change): both
/// parts are taken by the wiring, the slots' first, so another taker finds neither.
#[test]
fn the_slots_part_and_the_xcodes_part_are_both_taken_by_the_wiring() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-node-watch-parts");
    let runtime =
        NativeRuntime::new(NativeConfig::new(dir.to_path_buf()), Arc::new(NoCas)).expect("runtime");
    let (driver, _reports, _xcodes) = watch_parts(&runtime).expect("the parts");
    assert!(driver.part(kbf_driver_native::slots::SOURCE).is_none());
    assert!(driver.part(xcode_watch::SOURCE).is_none());
    drop(runtime);
}

/// With the host's memory above the actions cgroup's cap, the placer's whole-host
/// figure is the capped memory the scheduler sees, not the host's.
#[test]
fn the_placers_whole_host_is_the_memory_the_scheduler_sees() {
    let detected = || NodeReport::new([("mem_gib", "251"), ("cpus", "64")]);
    let cap = Capacity {
        cpus: None,
        memory_bytes: Some(175 << 30),
    };
    let reported = offered(detected().with_entries([("zone", "a")]), cap);
    let whole_host = offered(detected(), cap).mem_bytes();
    assert_eq!(whole_host, Some(175 << 30));
    assert_eq!(whole_host, reported.mem_bytes());
}

fn parse(extra: &[&str]) -> Result<Cli, clap::Error> {
    let base = [
        "kbf-daemon",
        "--server=https://front:7070",
        "--ca-cert=ca.pem",
        "--cert=c.pem",
        "--key=c.key",
        "--node-id=mac-1",
    ];
    Cli::try_parse_from(base.iter().chain(extra))
}

/// Catches a drain deadline that comes back as a default (the old 600 s), one that is
/// capped, and a 0 that is refused (a live `kbf.toml` may hold it) or read as "no
/// deadline" (it was "stop at once"): absent is `None`, any number is kept as given,
/// and 0 is `Some(0)`, a warning at once.
#[test]
fn the_drain_deadline_is_absent_by_default_and_has_no_cap() {
    let deadline = |extra: &[&str]| {
        parse(&[&["--driver=native"], extra].concat())
            .expect("flags")
            .drain_deadline_secs
    };
    assert_eq!(deadline(&[]), None);
    assert_eq!(deadline(&["--drain-deadline-secs=600"]), Some(600));
    assert_eq!(deadline(&["--drain-deadline-secs=0"]), Some(0));
    assert_eq!(
        deadline(&["--drain-deadline-secs=315360000"]),
        Some(315_360_000)
    );
    assert!(parse(&["--driver=native", "--drain-deadline-secs=-1"]).is_err());
}

/// Catches: native flags that do not reach the driver's configuration (the
/// memory limit, poll and output limits, where Xcodes are looked for, how often
/// they are asked again, whether the Metal toolchain is required), a re-check
/// every 0 s (a busy loop), and a relative or missing scratch directory accepted.
#[test]
fn native_flags_reach_the_configuration() {
    let cli = parse(&[
        "--driver=native",
        "--data-dir=/var/kbf",
        "--memory-limit-percent=200",
        "--memory-headroom-mib=64",
        "--memory-poll-ms=0",
        "--native-term-grace-ms=750",
        "--output-max-bytes=99",
        "--output-upload-deadline-secs=7",
        "--xcode-apps=/var/kbf/apps",
    ])
    .expect("flags");
    assert_eq!(cli.drivers, [Driver::Native]);
    let config = native_config(&cli).expect("config");
    assert_eq!(config.scratch, PathBuf::from("/var/kbf/leases"));
    assert_eq!(config.memory.limit(1 << 20), Some((2 << 20) + (64 << 20)));
    assert_eq!(config.poll, Duration::from_millis(1));
    assert_eq!(config.term_grace, Duration::from_millis(750));
    assert_eq!(config.outputs.max_bytes, 99);
    assert_eq!(config.upload_deadline, Duration::from_secs(7));
    assert_eq!(cli.xcode_apps, PathBuf::from("/var/kbf/apps"));
    assert_eq!(
        xcode_watch_args(&cli),
        (xcode::Probe::system(false), xcode_watch::EVERY)
    );
    let metal = parse(&[
        "--driver=native",
        "--require-metal-toolchain",
        "--xcode-recheck-secs=0",
    ])
    .expect("flags");
    assert_eq!(
        xcode_watch_args(&metal),
        (xcode::Probe::system(true), Duration::from_secs(1))
    );
    let defaults = parse(&["--driver=native", "--data-dir=/s"]).expect("flags");
    assert_eq!(
        native_config(&defaults).expect("config").memory,
        MemoryPolicy::DEFAULT
    );
    assert_eq!(defaults.xcode_apps, PathBuf::from("/Applications"));
    assert_eq!(
        native_config(&defaults).expect("config").term_grace,
        kbf_driver_native::DEFAULT_TERM_GRACE
    );
    let at_once = parse(&[
        "--driver=native",
        "--data-dir=/s",
        "--native-term-grace-ms=0",
    ])
    .expect("flags");
    assert_eq!(
        native_config(&at_once).expect("config").term_grace,
        Duration::ZERO
    );
    let forever = parse(&[
        "--driver=native",
        "--data-dir=/s",
        "--native-term-grace-ms=18446744073709551615",
    ])
    .expect("flags");
    assert_eq!(
        native_config(&forever).expect("config").term_grace,
        kbf_driver_native::grace::MAX_GRACE
    );
    let relative = parse(&["--driver=native", "--data-dir=leases"]).expect("flags");
    assert!(native_config(&relative).is_err());
    let missing = parse(&["--driver=native"]).expect("flags");
    assert_eq!(
        native_config(&missing).expect("config").scratch,
        PathBuf::from("/var/lib/kbf/leases"),
        "the default data directory"
    );
    assert_eq!(native_config(&defaults).expect("config").pool, None);
}

/// Catches: a default other than 10 percent, a value that does not reach the
/// field, and a threshold the daemon cannot use accepted (0 or less pauses a node
/// on any stall at all or never; over 100 never pauses; a word or NaN is no
/// percentage).
#[test]
fn the_memory_pressure_threshold_is_a_percentage() {
    let pause = |args: &[&str]| parse(args).map(|cli| cli.memory_pressure_pause_percent);
    assert_eq!(pause(&["--driver=fake"]).expect("flags"), 10.0);
    assert_eq!(
        pause(&["--driver=fake", "--memory-pressure-pause-percent=2.5"]).expect("flags"),
        2.5
    );
    assert_eq!(
        pause(&["--driver=fake", "--memory-pressure-pause-percent=100"]).expect("flags"),
        100.0
    );
    for bad in ["0", "-1", "100.5", "lots", "NaN", ""] {
        let flag = format!("--memory-pressure-pause-percent={bad}");
        assert!(pause(&["--driver=fake", &flag]).is_err(), "{bad:?} parsed");
    }
}

/// Catches a backstop that is attached without a readable pressure file (a node
/// with no pressure stall information would be paused or crash), one that is not
/// attached with one (a Linux node would never stop taking leases), one attached
/// with another threshold than the flag's, and a sampler that does not read the
/// file it is given.
#[tokio::test]
async fn the_backstop_is_attached_only_where_the_kernel_reports_pressure() {
    let dir = scratch("psi");
    tls_files(&dir);
    let file = dir.join("memory");
    std::fs::write(&file, "some avg10=1.50 avg60=0.00 avg300=0.00 total=1\n").expect("write");
    let cli = |psi: &Path| {
        let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
        Cli::try_parse_from([
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            flag("ca-cert", &dir.join("ca.pem")),
            flag("cert", &dir.join("node.pem")),
            flag("key", &dir.join("node.key")),
            "--node-id=mac-1".to_owned(),
            "--driver=fake".to_owned(),
            "--memory-pressure-pause-percent=7.5".to_owned(),
            flag("psi-memory-file", psi),
        ])
        .expect("flags")
    };
    let attached = |cli: &Cli| {
        let report = NodeReport::detect(&[]).expect("a report");
        let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
        let config = DaemonConfig::from_args(&cli.daemon);
        let daemon = Daemon::new(config, runtime, report).expect("a daemon");
        backstop(cli, daemon).memory_backstop()
    };
    let on = cli(&file);
    assert_eq!(attached(&on), Some(7.5));
    assert_eq!(*memory_stall(&on).expect("readable").borrow(), Some(1.5));
    let off = cli(&dir.join("missing"));
    assert_eq!(attached(&off), None);
    assert!(memory_stall(&off).is_none());
}

/// Catches pool flags that do not reach the driver (a root daemon would refuse to
/// start, or run every action as one user), a pool of other users than
/// kbf-mac-provision makes, one flag without the other taken, and a pool at root's
/// uid accepted.
#[test]
fn the_pool_flags_reach_the_configuration() {
    let pooled = |extra: &[&str]| {
        let flags = [&["--driver=native", "--data-dir=/s"][..], extra].concat();
        parse(&flags)
    };
    let cli = pooled(&["--pool-users=2", "--pool-first-uid=700"]).expect("flags");
    let pool = native_config(&cli).expect("config").pool.expect("a pool");
    assert_eq!(Some(pool), Pool::provisioned(2, 700).ok());
    for one in ["--pool-users=2", "--pool-first-uid=700"] {
        assert!(pooled(&[one]).is_err(), "{one} alone");
    }
    for count in ["0", "101"] {
        assert!(pooled(&[&format!("--pool-users={count}"), "--pool-first-uid=700"]).is_err());
    }
    let root = pooled(&["--pool-users=1", "--pool-first-uid=0"]).expect("flags");
    let refused = native_config(&root).expect_err("a pool at uid 0");
    assert!(refused.to_string().contains("root"), "{refused}");
}

/// Catches a native daemon with a lease-user pool that makes `leases` 0700, which a
/// pool user cannot search: its first lease fails with "working directory:
/// Permission denied" (measured on a Mac with `--pool-users`). Starts the data
/// directory the way the daemon does: with a pool `leases` is
/// `kbf_driver_native::slots::LEASES_ROOT_MODE` (0711: searched, not listed, by the
/// slot users), and a leftover 0700 one from a start without the pool is opened;
/// without a pool the daemon's own user alone keeps it (0700); the container driver
/// keeps its 0711; the fake driver with a pool flag and no native driver does not
/// open it.
#[test]
fn leases_are_searchable_by_the_pool_users_of_a_native_daemon_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let base = tests::scratch("leases-pool-mode");
    let mode = |dir: &Path| {
        std::fs::metadata(dir.join("leases"))
            .expect("leases")
            .permissions()
            .mode()
            & 0o777
    };
    let start = |name: &str, extra: &[&str]| {
        let dir = base.join(name);
        let data = format!("--data-dir={}", dir.display());
        let flags = [&["--driver=native", data.as_str()][..], extra].concat();
        let cli = parse(&flags).expect("flags");
        data_dir(&cli).expect("started").expect("a data dir");
        dir
    };
    let pool = ["--pool-users=2", "--pool-first-uid=700"];
    let pooled = start("pooled", &pool);
    assert_eq!(mode(&pooled), kbf_driver_native::slots::LEASES_ROOT_MODE);
    assert_eq!(mode(&pooled), 0o711, "traverse, no list, no write");
    let alone = start("alone", &[]);
    assert_eq!(mode(&alone), 0o700, "a daemon without a pool");
    // The same data directory started again, now with a pool.
    let again = start("alone", &pool);
    assert_eq!(again, alone);
    assert_eq!(mode(&alone), 0o711, "an existing 0700 is opened for a pool");
    let flags = [format!("--data-dir={}", base.join("oci").display())];
    let oci = parse(&["--driver=container", flags[0].as_str()]).expect("flags");
    assert!(leases_open(&oci), "the container driver");
    let no_native = parse(&["--driver=fake", flags[0].as_str(), pool[0], pool[1]]).expect("flags");
    assert!(
        !leases_open(&no_native),
        "a pool flag without the native driver"
    );
}

/// Catches an upload deadline that defaults to anything but
/// `kbf_daemon::UPLOAD_DEADLINE` (30 minutes), and a deadline of 0 accepted (every
/// lease with outputs would fail).
#[test]
fn the_upload_deadline_defaults_to_thirty_minutes_and_is_never_zero() {
    let cli = parse(&["--driver=native"]).expect("flags");
    assert_eq!(upload_deadline(&cli), Duration::from_secs(1800));
    assert_eq!(upload_deadline(&cli), kbf_daemon::UPLOAD_DEADLINE);
    assert!(parse(&["--driver=native", "--output-upload-deadline-secs=0"]).is_err());
}

/// Catches a `--container-*` flag that does not reach the driver's limits, and a
/// flag default that differs from the driver's documented
/// `ContainerLimits::DEFAULT`.
#[cfg(target_os = "linux")]
#[test]
fn container_limit_flags_reach_the_configuration() {
    use kbf_driver_container::ContainerLimits;
    let cli = parse(&[
        "--driver=container",
        "--container-pids-limit=11",
        "--container-shm-mib=12",
        "--container-nofile=13",
        "--container-nproc=14",
    ])
    .expect("flags");
    assert_eq!(
        container::limits(&cli),
        ContainerLimits {
            pids: 11,
            shm_mib: 12,
            nofile: 13,
            nproc: 14,
        }
    );
    let defaults = parse(&["--driver=container"]).expect("flags");
    assert_eq!(container::limits(&defaults), ContainerLimits::DEFAULT);
}

/// Catches `--cpu-threads` not parsed, not reaching the sizes the CPU pool is built
/// from, or accepting 0 (a pool that never runs a job).
#[test]
fn cpu_threads_is_a_flag() {
    let cli = parse(&["--driver=fake", "--cpu-threads=6"]).expect("flags");
    assert_eq!(sizes(&cli).expect("sizes").cpu_threads(), 6);
    let cli = parse(&["--driver=fake"]).expect("flags");
    assert!(sizes(&cli).expect("sizes").cpu_threads() >= 1);
    assert!(parse(&["--driver=fake", "--cpu-threads=0"]).is_err());
}

/// Catches the container driver's pool not following `--cpu-threads` (a fixed size
/// in its construction). 6 and 3 are not the default on any machine's topology.
#[cfg(target_os = "linux")]
#[test]
fn cpu_threads_reaches_the_container_pool() {
    let cli = parse(&["--driver=container", "--cpu-threads=6"]).expect("flags");
    assert_eq!(container::cpu_pool(&cli).expect("pool").threads(), 6);
    let cli = parse(&["--driver=container", "--cpu-threads=3"]).expect("flags");
    assert_eq!(container::cpu_pool(&cli).expect("pool").threads(), 3);
}

/// Catches the daemon resolving another role's defaults: on the 44-core,
/// 2-threads-per-core farm node it is a reserve of 8, so 2 async, 64 blocking and 4
/// CPU threads and 80 CPUs for leases (the server's would be 22, 128, 44), a
/// flag still wins over its default, the pools follow `--reserve-cpus`, and a
/// reserve of the whole machine is refused.
#[test]
fn the_daemon_resolves_the_daemon_role() {
    let topology = || kbf_threads::Topology::new(88, 2);
    let cli = parse(&["--driver=fake"]).expect("flags");
    let sizes = sizes_on(&cli, topology()).expect("sizes");
    assert_eq!(
        (
            sizes.async_threads(),
            sizes.blocking_threads(),
            sizes.cpu_threads(),
            sizes.lease_cpus()
        ),
        (2, 64, 4, 80)
    );
    let cli = parse(&["--driver=fake", "--async-threads=5"]).expect("flags");
    assert_eq!(
        sizes_on(&cli, topology()).expect("sizes").async_threads(),
        5
    );
    let cli = parse(&["--driver=fake", "--reserve-cpus=40"]).expect("flags");
    let sizes = sizes_on(&cli, topology()).expect("sizes");
    assert_eq!(
        (
            sizes.async_threads(),
            sizes.cpu_threads(),
            sizes.lease_cpus()
        ),
        (10, 20, 48)
    );
    let cli = parse(&["--driver=fake", "--reserve-cpus=88"]).expect("flags");
    assert!(sizes_on(&cli, topology()).is_err());
}

/// Catches a `--lease-swap-*` flag that does not reach the driver's swap watch (in
/// its unit: MiB, percent, milliseconds), a default that differs from the driver's
/// documented `SwapKill::DEFAULT`, and a poll of 0 accepted (a busy loop).
#[cfg(target_os = "linux")]
#[test]
fn swap_kill_flags_reach_the_configuration() {
    use kbf_driver_container::SwapKill;
    let cli = parse(&[
        "--driver=container",
        "--lease-swap-kill-mib=3",
        "--lease-swap-kill-percent=40",
        "--lease-swap-poll-ms=250",
    ])
    .expect("flags");
    assert_eq!(
        container::swap_kill(&cli),
        SwapKill {
            floor_bytes: 3 << 20,
            percent: 40,
            every: Duration::from_millis(250),
        }
    );
    let defaults = parse(&["--driver=container"]).expect("flags");
    assert_eq!(container::swap_kill(&defaults), SwapKill::DEFAULT);
    assert!(parse(&["--driver=container", "--lease-swap-poll-ms=0"]).is_err());
}

// This test binary answers as kbf-testkit's `kbf-act`, or as its fake `xcrun`,
// `xcodebuild` or `sandbox-exec`, when run through a link of that name: the
// programs the tests here (and src/startup_tests.rs) give the daemon to run.
kbf_testkit::act_test_hook!();

/// Makes `path` a link to this test binary, which answers as the fake its base name
/// names, with `rules` beside it (see `kbf_testkit::fake`).
pub(crate) fn fake(path: &Path, rules: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    kbf_testkit::act::link_self(path).expect("link");
    std::fs::write(format!("{}.rules", path.display()), rules).expect("rules");
}

/// A fresh directory for one test, under the test binary's directory.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-node-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = kbf_outputs::remove_tree(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A CA and a client certificate signed by it, written to `dir` as the flags name.
pub(crate) fn tls_files(dir: &Path) {
    use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    let key = KeyPair::generate().expect("key");
    let cert = CertificateParams::new(Vec::new())
        .expect("params")
        .signed_by(&key, &ca)
        .expect("sign");
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    std::fs::write(dir.join("node.pem"), cert.pem()).expect("write");
    std::fs::write(dir.join("node.key"), key.serialize_pem()).expect("write");
}

/// Catches: the native driver's Xcode survey not handed to the daemon (issue #164),
/// so the node's `NodeStatus` lists no Xcode at all, ready or not, and its Hello
/// advertises none: the silent removal an Xcode that is not ready must never get.
/// The Xcode here is an empty app, which no `xcodebuild` accepts (on Linux there is
/// none), so the survey lists it as not ready, with why.
#[tokio::test]
async fn the_native_daemon_reports_every_installed_xcode() {
    let dir = scratch("native");
    tls_files(&dir);
    let apps = dir.join("Applications");
    let app = apps.join("Xcode_1.app");
    std::fs::create_dir_all(app.join("Contents/Developer")).expect("app");
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
        flag("xcode-apps", &apps),
    ])
    .expect("flags");
    let mut config = native_config(&cli).expect("config");
    // Not the runner's own (on macOS): the start sweeps them.
    config.user_folders = None;
    let (daemon, mut reports) =
        native_with(&cli, config, &dir.join("no-xcrun")).expect("the native daemon");
    // Not surveyed yet, or already surveyed (this one fails at once): either way
    // listed. The gated tests (xcode_watch, startup_tests) hold the survey.
    let listed = daemon.node_status().xcodes;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].app, app.display().to_string());
    let xcodes = surveyed(&mut reports).await.xcodes;
    assert_eq!(xcodes.len(), 1, "{xcodes:?}");
    assert_eq!(xcodes[0].app, app.display().to_string());
    assert_eq!(xcodes[0].state(), XcodeState::Failed);
    assert!(!xcodes[0].reason.is_empty(), "{xcodes:?}");
    drop((daemon, reports));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches (decision on issue #164): the native daemon removing `xcrun`'s cache
/// before its first survey. Every `xcrun` lookup of the survey then has nothing
/// cached and takes seconds, while no Xcode is ready: on the macOS runner (15
/// `Xcode*.app`) that survey took over 30 s, against about 7 s with the cache kept. The survey runs under the sandbox, so
/// what a lease wrote there reaches only sandboxed tools. The Xcode's own
/// `xcodebuild` is a fake that answers build `CACHE` while the cache is there, and
/// fails once it is gone.
#[tokio::test]
async fn the_first_survey_finds_the_xcrun_cache_kept() {
    let dir = scratch("forget");
    tls_files(&dir);
    let (temp, cache) = (dir.join("T"), dir.join("C"));
    std::fs::create_dir_all(&temp).expect("T");
    std::fs::create_dir_all(&cache).expect("C");
    std::fs::write(temp.join("xcrun_db"), b"a lease's").expect("cache");
    let apps = dir.join("Applications");
    let program = apps
        .join("Xcode_1.app/Contents/Developer")
        .join(xcode::XCODEBUILD);
    fake(
        &program,
        &format!(
            "*\t-\twait-for {}/xcrun_db 0 ; echo Build version CACHE\n",
            temp.display()
        ),
    );
    // On macOS the survey runs under the real sandbox, which lets it write only in
    // its lease directory and under /dev: the fake's log of its runs (beside it,
    // see kbf_testkit::fake) goes to /dev/null.
    std::os::unix::fs::symlink("/dev/null", format!("{}.log", program.display()))
        .expect("log link");
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
        flag("xcode-apps", &apps),
    ])
    .expect("flags");
    let mut config = native_config(&cli).expect("config");
    config.user_folders = kbf_driver_native::user_folders::UserFolders::new(temp.clone(), cache);
    let (daemon, mut reports) =
        native_with(&cli, config, &dir.join("no-xcrun")).expect("the native daemon");
    let xcodes = surveyed(&mut reports).await.xcodes;
    assert_eq!(xcodes.len(), 1, "{xcodes:?}");
    assert_eq!(xcodes[0].build, "CACHE", "{xcodes:?}");
    assert!(
        temp.join("xcrun_db").exists(),
        "the start removed the cache"
    );
    drop((daemon, reports));
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches (decision on issue #164): the native daemon's survey of its Xcodes
/// asking a question outside the actions' sandbox, `xcrun`'s lookup above all (it
/// reads and fills a cache leases can write), or with an `xcrun` other than the one
/// the daemon warms. The sandbox program is a fake that marks what it runs (its
/// `-D KBF_LEASE=<dir>` in the command's environment); the Xcode's `xcodebuild` and
/// the `xcrun` are fakes that log each run and the mark, empty when unmarked. The
/// warm-up of the Xcode the survey finds ready runs too, also
/// sandboxed; the test waits for it to end. Also catches the warm-up not run, or
/// run before the survey's questions are answered (its lookups would make the
/// survey's miss): the fake `xcodebuild` answers after half a second, so a warm-up
/// started with the daemon logs before it.
#[tokio::test]
async fn the_daemons_survey_runs_under_the_sandbox() {
    let dir = scratch("sandboxed");
    tls_files(&dir);
    let log = dir.join("ran");
    let sandbox_exec = dir.join("sandbox-exec");
    fake(&sandbox_exec, "");
    // `<tool> <the sandbox's lease directory, empty when unsandboxed> <arguments>`.
    let logged = |tool: &str| format!("append {} {tool} ${{KBF_LEASE}} $*", log.display());
    let apps = dir.join("Applications");
    let xcodebuild = apps
        .join("Xcode_1.app/Contents/Developer")
        .join(xcode::XCODEBUILD);
    fake(
        &xcodebuild,
        &format!(
            "*\t-\tsleep 0.5 ; {} ; echo Build version 1A1\n",
            logged("xcodebuild")
        ),
    );
    let xcrun = dir.join("bin/xcrun");
    fake(
        &xcrun,
        &format!("*\t-\t{} ; echo /x/clang\n", logged("xcrun")),
    );
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
        flag("xcode-apps", &apps),
    ])
    .expect("flags");
    let mut config = native_config(&cli).expect("config");
    // Not the runner's own (on macOS): the start sweeps them.
    config.user_folders = None;
    config.isolation = kbf_driver_native::network::Isolation::Sandbox(sandbox_exec);
    let (daemon, mut reports) = native_with(&cli, config, &xcrun).expect("the native daemon");
    let xcodes = surveyed(&mut reports).await.xcodes;
    assert_eq!(xcodes.len(), 1, "{xcodes:?}");
    assert_eq!(xcodes[0].state(), XcodeState::Ready, "{xcodes:?}");
    let leases = std::fs::canonicalize(dir.join("leases")).expect("real");
    let survey = leases.join(kbf_driver_native::SURVEY_DIR);
    assert!(!survey.exists(), "the survey's directory stays");
    // The warm-up removes its directory when it ends: waited for up to 20 s
    // (without a branch of this test's own, which coverage would count).
    let warm = leases.join("lease-warm-up");
    (0..1000)
        .take_while(|_| warm.exists())
        .for_each(|_| std::thread::sleep(Duration::from_millis(20)));
    let ran = std::fs::read_to_string(&log).expect("ran");
    let at = |lease: &Path, what: &str| format!("{} {what}", lease.display());
    let warmed = format!("xcrun {}", at(&warm, ""));
    let asked = at(&survey, "");
    assert!(
        ran.lines().any(|l| l.starts_with(&warmed)),
        "no warm-up: {ran}"
    );
    let asked_first = ran
        .lines()
        .take_while(|l| !l.starts_with(&warmed))
        .filter(|l| l.contains(&asked))
        .count();
    assert_eq!(
        asked_first, 4,
        "the warm-up ran before the survey's answers: {ran}"
    );
    // The survey's questions, asked at once, come before the warm-up's lookups.
    let mut surveyed: Vec<String> = ran
        .lines()
        .filter(|l| l.starts_with("xcodebuild ") || l.ends_with(" --find clang"))
        .take(4)
        .map(str::to_owned)
        .collect();
    surveyed.sort();
    assert_eq!(
        surveyed,
        [
            format!("xcodebuild {}", at(&survey, "-checkFirstLaunchStatus")),
            format!("xcodebuild {}", at(&survey, "-license check")),
            format!("xcodebuild {}", at(&survey, "-version")),
            format!("xcrun {}", at(&survey, "--find clang")),
        ],
        "{ran}"
    );
    assert!(
        ran.lines()
            .all(|l| l.starts_with("xcodebuild ") || l.starts_with("xcrun ")),
        "{ran}"
    );
    let unsandboxed: Vec<&str> = ran
        .lines()
        .filter(|l| !l.contains(&*leases.to_string_lossy()))
        .collect();
    assert_eq!(unsandboxed, Vec::<&str>::new(), "{ran}");
    drop((daemon, reports));
    kbf_outputs::remove_tree(&dir).expect("clean");
}
