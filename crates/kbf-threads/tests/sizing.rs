//! Sizing from a fake sysfs and flags, and the runtime built with the sizes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use clap::Parser;

use kbf_testkit::dir::TestDir;
use kbf_threads::{PoolArgs, ReserveError, Sizes, ThreadArgs, Topology};

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    threads: ThreadArgs,
}

/// The server's flags: the overrides alone, with no reserve.
#[derive(Parser)]
struct ServerCli {
    #[command(flatten)]
    threads: PoolArgs,
}

fn resolved(given: ThreadArgs, topology: Topology) -> Sizes {
    given.resolve(topology).expect("sizes")
}

fn server(flags: &[&str], topology: Topology) -> Sizes {
    ServerCli::try_parse_from(std::iter::once("t").chain(flags.iter().copied()))
        .expect("flags parse")
        .threads
        .resolve(topology)
}

fn triple(sizes: &Sizes) -> (usize, usize, usize) {
    (
        sizes.async_threads(),
        sizes.blocking_threads(),
        sizes.cpu_threads(),
    )
}

fn args(flags: &[&str]) -> ThreadArgs {
    Cli::try_parse_from(std::iter::once("t").chain(flags.iter().copied()))
        .expect("flags parse")
        .threads
}

/// A sysfs tree whose cpu0 reports `siblings` in the file `name`.
fn sysfs(name: &str, siblings: &str) -> TestDir {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "threads");
    let topo = dir.path().join("devices/system/cpu/cpu0/topology");
    std::fs::create_dir_all(&topo).expect("topology dir");
    std::fs::write(topo.join(name), siblings).expect("write list");
    dir
}

#[test]
fn the_farm_node_has_44_cores_of_two_threads() {
    // 88 logical CPUs, cpu0 sharing its core with cpu44.
    let fake = sysfs("thread_siblings_list", "0,44\n");
    let topology = Topology::detect_in(fake.path(), 88);
    assert_eq!(topology.logical(), 88);
    assert_eq!(topology.threads_per_core(), 2);
    assert_eq!(topology.physical(), 44);

    let daemon = resolved(ThreadArgs::default(), topology);
    assert_eq!(
        (
            daemon.async_threads(),
            daemon.blocking_threads(),
            daemon.cpu_threads()
        ),
        (2, 64, 4)
    );
    // The server takes every logical CPU, hyperthreads included, and tokio's blocking cap.
    assert_eq!(triple(&server(&[], topology)), (88, 512, 88));
}

#[test]
fn core_cpus_list_is_read_when_the_other_name_is_absent() {
    let fake = sysfs("core_cpus_list", "0-1");
    assert_eq!(Topology::detect_in(fake.path(), 8).physical(), 4);
}

#[test]
fn no_topology_files_is_one_thread_per_core() {
    let empty = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "threads");
    let topology = Topology::detect_in(empty.path(), 8);
    assert_eq!((topology.threads_per_core(), topology.physical()), (1, 8));
    let junk = sysfs("thread_siblings_list", "not a list");
    assert_eq!(Topology::detect_in(junk.path(), 8).threads_per_core(), 1);
}

#[test]
fn defaults_are_limited_to_the_physical_cores_but_blocking_is_not() {
    // 2 logical CPUs on one core: one physical core.
    let one_core = Topology::new(2, 2);
    let daemon = resolved(ThreadArgs::default(), one_core);
    assert_eq!(
        (
            daemon.async_threads(),
            daemon.blocking_threads(),
            daemon.cpu_threads()
        ),
        (1, 64, 1)
    );
    // The server is not limited to the cores: 2 CPUs on one core give 2 and 2.
    assert_eq!(triple(&server(&[], one_core)), (2, 512, 2));
    assert_eq!(triple(&server(&[], Topology::new(3, 1))), (3, 512, 3));
    // More threads per core than CPUs, or no CPUs, cannot make zero cores.
    assert_eq!(Topology::new(0, 0).physical(), 1);
    assert_eq!(Topology::new(1, 8).physical(), 1);
}

#[test]
fn flags_override_the_defaults_even_above_the_cpus() {
    let given = args(&[
        "--async-threads=9",
        "--blocking-threads=3",
        "--cpu-threads=100",
    ]);
    let sizes = resolved(given, Topology::new(4, 2));
    assert_eq!(
        (
            sizes.async_threads(),
            sizes.blocking_threads(),
            sizes.cpu_threads()
        ),
        (9, 3, 100)
    );
    // One flag leaves the other two at their defaults.
    let one = server(&["--cpu-threads=6"], Topology::new(88, 2));
    assert_eq!(triple(&one), (88, 512, 6));
    let all = server(
        &[
            "--async-threads=9",
            "--blocking-threads=3",
            "--cpu-threads=100",
        ],
        Topology::new(4, 2),
    );
    assert_eq!(triple(&all), (9, 3, 100));
}

#[test]
fn zero_and_junk_are_refused() {
    for flag in [
        "--async-threads=0",
        "--blocking-threads=0",
        "--cpu-threads=0",
        "--cpu-threads=x",
    ] {
        assert!(Cli::try_parse_from(["t", flag]).is_err(), "{flag} accepted");
    }
}

#[test]
fn the_start_line_names_every_size_and_the_topology() {
    let sizes = resolved(ThreadArgs::default(), Topology::new(88, 2));
    assert_eq!(
        sizes.describe(),
        "threads: async=2 blocking=64 cpu=4 reserve=8 leases=80 (cpus=88 threads_per_core=2 physical_cores=44)"
    );
    sizes.log();
}

#[test]
fn this_machine_has_a_topology() {
    let topology = Topology::detect();
    assert!(topology.logical() >= 1);
    assert!((1..=topology.logical()).contains(&topology.physical()));
}

#[test]
fn the_runtime_has_the_workers_and_the_blocking_cap_asked_for() {
    let sizes = args(&[
        "--async-threads=3",
        "--blocking-threads=2",
        "--cpu-threads=1",
    ])
    .resolve(Topology::new(16, 2))
    .expect("sizes");
    let rt = sizes.runtime().expect("runtime");
    assert_eq!(rt.metrics().num_workers(), 3);

    // Eight calls that each hold their thread for a while: no more than two run at
    // once, and two do.
    let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    rt.block_on(async {
        let mut calls = Vec::new();
        for _ in 0..8 {
            let (running, peak) = (Arc::clone(&running), Arc::clone(&peak));
            calls.push(tokio::task::spawn_blocking(move || {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(60));
                running.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for call in calls {
            call.await.expect("blocking call");
        }
    });
    assert_eq!(peak.load(Ordering::SeqCst), 2);

    // The workers carry the runtime's thread name.
    let name = rt.block_on(async {
        tokio::spawn(async { std::thread::current().name().map(str::to_owned) })
            .await
            .expect("task")
    });
    assert_eq!(name.as_deref(), Some("kbf-async"));
}

/// Catches the daemon's pools not following the reserve (a fixed size), the 2-thread
/// floor lost (N=0 or 2 giving 0 or 1 workers), the quarter and half ratios swapped, and
/// the cores limit dropped: 88 CPUs of 44 cores, a reserve of 80 is async 20, cpu 40, a
/// reserve of 200 cannot exceed the cores.
#[test]
fn the_pools_follow_the_reserve() {
    let farm = Topology::new(88, 2);
    for (reserve, want) in [
        (0, (2, 2)),
        (2, (2, 2)),
        (8, (2, 4)),
        (16, (4, 8)),
        (40, (10, 20)),
        (80, (20, 40)),
    ] {
        let given = args(&[&format!("--reserve-cpus={reserve}")]);
        let s = resolved(given, farm);
        assert_eq!(
            (s.async_threads(), s.cpu_threads()),
            want,
            "reserve {reserve}"
        );
        assert_eq!(s.blocking_threads(), 64);
    }
    // The cores limit: 4 CPUs of 2 cores never give more than 2.
    let small = resolved(args(&["--reserve-cpus=3"]), Topology::new(4, 2));
    assert_eq!((small.async_threads(), small.cpu_threads()), (2, 2));
    assert_eq!(kbf_threads::pool_sizes(1000, 3), (3, 3));
}

/// Catches leases given the machine (no reserve taken), the reserve taken twice, and the
/// default not being 4 physical cores: 8 logical CPUs on the farm node, whose cores have two
/// threads (88 minus 8 is 80), and 4 on a Mac, whose cores have one (a 32-core Mac Studio
/// leases 28, a 28-core one 24).
#[test]
fn leases_get_the_cpus_minus_the_reserve() {
    let farm = Topology::new(88, 2);
    let default = resolved(ThreadArgs::default(), farm);
    assert_eq!((default.reserve_cpus(), default.lease_cpus()), (8, 80));
    for (cores, leased) in [(32, 28), (28, 24)] {
        let mac = resolved(ThreadArgs::default(), Topology::new(cores, 1));
        assert_eq!(
            (mac.reserve_cpus(), mac.lease_cpus()),
            (4, leased),
            "{cores} cores"
        );
    }
    let given = resolved(args(&["--reserve-cpus=20"]), farm);
    assert_eq!((given.reserve_cpus(), given.lease_cpus()), (20, 68));
    let none = resolved(args(&["--reserve-cpus=0"]), farm);
    assert_eq!((none.reserve_cpus(), none.lease_cpus()), (0, 88));
    // A machine of fewer than 8 CPUs (one thread a core) reserves half, so the default always
    // starts.
    for (logical, reserve) in [(1, 0), (2, 1), (4, 2), (7, 3), (8, 4), (16, 4)] {
        let s = resolved(ThreadArgs::default(), Topology::new(logical, 1));
        assert_eq!(
            (s.reserve_cpus(), s.lease_cpus()),
            (reserve, logical - reserve)
        );
    }
    // The share changes the pools, never the leases.
    let shared = resolved(args(&["--reserve-cpus=16", "--reserve-share=2"]), farm);
    assert_eq!(shared.lease_cpus(), 72);
    assert_eq!((shared.async_threads(), shared.cpu_threads()), (2, 4));
}

/// Catches the overrides losing to the reserve, one override disturbing another, and
/// an override clamped to the cores.
#[test]
fn the_three_flags_override_the_reserve() {
    let farm = Topology::new(88, 2);
    let s = resolved(args(&["--reserve-cpus=80", "--async-threads=3"]), farm);
    assert_eq!(triple(&s), (3, 64, 40));
    let s = resolved(
        args(&[
            "--reserve-cpus=80",
            "--cpu-threads=7",
            "--blocking-threads=5",
        ]),
        farm,
    );
    assert_eq!(triple(&s), (20, 5, 7));
    // The reserve a flag names does not change the leases.
    assert_eq!(s.lease_cpus(), 8);
}

/// Catches a reserve that leaves the daemon no CPU, or is more than the machine, being
/// accepted, the off-by-one at the boundary, and 0 being refused.
#[test]
fn a_reserve_the_machine_cannot_give_is_refused() {
    let farm = Topology::new(88, 2);
    let daemon = |n: usize| args(&[&format!("--reserve-cpus={n}")]).resolve(farm);
    assert!(daemon(0).is_ok());
    assert!(daemon(87).is_ok());
    assert_eq!(
        daemon(88).expect_err("none left"),
        ReserveError::NoneLeft {
            reserve: 88,
            logical: 88
        }
    );
    assert_eq!(
        daemon(89).expect_err("too many"),
        ReserveError::MoreThanTheMachine {
            reserve: 89,
            logical: 88
        }
    );
    let text = daemon(88).expect_err("refused").to_string();
    assert!(
        text.contains("--reserve-cpus=88") && text.contains("88"),
        "{text}"
    );
    // Junk is refused by the parser.
    for flag in ["--reserve-cpus=-1", "--reserve-cpus=x", "--reserve-share=0"] {
        assert!(Cli::try_parse_from(["t", flag]).is_err(), "{flag} accepted");
    }
}

/// Catches the server growing a reserve again (it runs no actions; `--reserve-cpus` and
/// `--reserve-share` are the daemon's) and the all-in-one's daemon reserve being halved
/// by a share the server no longer takes: the server alone is 88 whatever the daemon
/// reserves, and the daemon with `--reserve-share=2` still sizes from half.
#[test]
fn the_reserve_is_the_daemons_and_the_server_takes_none() {
    let farm = Topology::new(88, 2);
    for flag in ["--reserve-cpus=16", "--reserve-share=2"] {
        assert!(ServerCli::try_parse_from(["t", flag]).is_err(), "{flag}");
    }
    let server = server(&[], farm);
    assert_eq!((server.reserve_cpus(), server.lease_cpus()), (0, 88));
    let flags = ["--reserve-cpus=16", "--reserve-share=2"];
    let daemon = resolved(args(&flags), farm);
    assert_eq!(triple(&daemon), (2, 64, 4));
    assert_eq!(daemon.lease_cpus(), 72);
    // Alone, the same reserve is not halved.
    let alone = resolved(args(&flags[..1]), farm);
    assert_eq!(triple(&alone), (4, 64, 8));
}

/// Catches a server start line that names a reserve it does not have, and the daemon's
/// losing its reserve or leases.
#[test]
fn the_server_start_line_names_no_reserve() {
    let farm = Topology::new(88, 2);
    assert_eq!(
        server(&[], farm).describe(),
        "threads: async=88 blocking=512 cpu=88 (cpus=88 threads_per_core=2 physical_cores=44)"
    );
}
