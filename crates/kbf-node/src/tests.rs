use std::path::Path;

use super::*;
use clap::Parser;

mod more;

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

/// Catches a native driver that tells the daemon nothing: the node report must carry how
/// the driver keeps the network off (`network_isolation`), which `Daemon::with_driver_report`
/// takes from the channel the wiring returns.
#[test]
fn the_native_driver_reports_how_it_keeps_the_network_off() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-node-reports");
    let (runtime, reports) =
        native_runtime(NativeConfig::new(dir.to_path_buf()), Arc::new(NoCas)).expect("runtime");
    assert_eq!(
        reports.borrow().entries,
        [("network_isolation".to_owned(), "none".to_owned())]
    );
    assert!(reports.borrow().xcodes.is_empty());
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
/// memory limit, poll, grace and output limits), a poll of 0 ms (a busy loop), and a
/// relative or missing scratch directory accepted.
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
    let defaults = parse(&["--driver=native", "--data-dir=/s"]).expect("flags");
    assert_eq!(
        native_config(&defaults).expect("config").memory,
        MemoryPolicy::DEFAULT
    );
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

/// Catches a native daemon whose `leases` is searchable by others (the container driver's
/// ids are not the daemon's; a native daemon runs as its own user and keeps 0700), and a
/// container daemon whose `leases` is closed.
#[test]
fn leases_are_searchable_by_others_for_the_container_driver_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let base = tests::scratch("leases-mode");
    let mode = |dir: &Path| {
        std::fs::metadata(dir.join("leases"))
            .expect("leases")
            .permissions()
            .mode()
            & 0o777
    };
    let start = |driver: &str, name: &str| {
        let dir = base.join(name);
        let flags = [
            format!("--driver={driver}"),
            format!("--data-dir={}", dir.display()),
        ];
        let cli = parse(&[flags[0].as_str(), flags[1].as_str()]).expect("flags");
        data_dir(&cli).expect("started").expect("a data dir");
        (cli, dir)
    };
    let (native, alone) = start("native", "alone");
    assert_eq!(mode(&alone), 0o700, "a native daemon");
    assert!(!leases_open(&native));
    let (oci, open) = start("container", "oci");
    assert!(leases_open(&oci), "the container driver");
    assert_eq!(mode(&open), 0o711, "traverse, no list, no write");
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

// This test binary answers as kbf-testkit's `kbf-act` when run through a link of that name:
// the programs the tests here (and src/startup_tests.rs) give the daemon to run.
kbf_testkit::act_test_hook!();

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
