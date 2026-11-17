//! Sizing from a fake sysfs and flags, and the runtime built with the sizes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use clap::Parser;

use kbf_testkit::dir::TestDir;
use kbf_threads::{PoolArgs, Sizes, Topology};

/// The server's flags: the overrides alone, with no reserve.
#[derive(Parser)]
struct ServerCli {
    #[command(flatten)]
    threads: PoolArgs,
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
fn defaults_are_every_logical_cpu_and_tokios_blocking_cap() {
    // 2 logical CPUs on one core: one physical core.
    let one_core = Topology::new(2, 2);
    // The server is not limited to the cores: 2 CPUs on one core give 2 and 2.
    assert_eq!(triple(&server(&[], one_core)), (2, 512, 2));
    assert_eq!(triple(&server(&[], Topology::new(3, 1))), (3, 512, 3));
    // More threads per core than CPUs, or no CPUs, cannot make zero cores.
    assert_eq!(Topology::new(0, 0).physical(), 1);
    assert_eq!(Topology::new(1, 8).physical(), 1);
}

#[test]
fn flags_override_the_defaults_even_above_the_cpus() {
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
        assert!(
            ServerCli::try_parse_from(["t", flag]).is_err(),
            "{flag} accepted"
        );
    }
}

#[test]
fn the_start_line_names_every_size_and_the_topology() {
    let sizes = server(&["--cpu-threads=4"], Topology::new(88, 2));
    assert_eq!(
        sizes.describe(),
        "threads: async=88 blocking=512 cpu=4 (cpus=88 threads_per_core=2 physical_cores=44)"
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
    let sizes = server(
        &[
            "--async-threads=3",
            "--blocking-threads=2",
            "--cpu-threads=1",
        ],
        Topology::new(16, 2),
    );
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

/// Catches the server growing a reserve again: it runs no actions, so `--reserve-cpus` and
/// `--reserve-share` are not its flags, and the server alone takes every CPU.
#[test]
fn the_server_takes_no_reserve() {
    let farm = Topology::new(88, 2);
    for flag in ["--reserve-cpus=16", "--reserve-share=2"] {
        assert!(ServerCli::try_parse_from(["t", flag]).is_err(), "{flag}");
    }
    assert_eq!(triple(&server(&[], farm)), (88, 512, 88));
}

/// Catches a server start line that names a reserve it does not have.
#[test]
fn the_server_start_line_names_no_reserve() {
    let farm = Topology::new(88, 2);
    assert_eq!(
        server(&[], farm).describe(),
        "threads: async=88 blocking=512 cpu=88 (cpus=88 threads_per_core=2 physical_cores=44)"
    );
}
