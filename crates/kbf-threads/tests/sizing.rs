//! Sizing from a fake sysfs and flags, and the runtime built with the sizes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use clap::Parser;

use kbf_threads::{Role, ThreadArgs, Topology};

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    threads: ThreadArgs,
}

fn args(flags: &[&str]) -> ThreadArgs {
    Cli::try_parse_from(std::iter::once("t").chain(flags.iter().copied()))
        .expect("flags parse")
        .threads
}

/// A sysfs tree whose cpu0 reports `siblings` in the file `name`.
fn sysfs(name: &str, siblings: &str) -> tempdir::Dir {
    let dir = tempdir::Dir::new();
    let topo = dir.path().join("devices/system/cpu/cpu0/topology");
    std::fs::create_dir_all(&topo).expect("topology dir");
    std::fs::write(topo.join(name), siblings).expect("write list");
    dir
}

mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// A scratch directory under the build's target directory, removed on drop.
    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> Self {
            let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
            let n = NEXT.fetch_add(1, Ordering::SeqCst);
            let path = base.join(format!("threads-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Self(path)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[test]
fn the_farm_node_has_44_cores_of_two_threads() {
    // 88 logical CPUs, cpu0 sharing its core with cpu44.
    let fake = sysfs("thread_siblings_list", "0,44\n");
    let topology = Topology::detect_in(fake.path(), 88);
    assert_eq!(topology.logical(), 88);
    assert_eq!(topology.threads_per_core(), 2);
    assert_eq!(topology.physical(), 44);

    let daemon = ThreadArgs::default().resolve(Role::Daemon, topology);
    assert_eq!(
        (
            daemon.async_threads(),
            daemon.blocking_threads(),
            daemon.cpu_threads()
        ),
        (2, 64, 4)
    );
    let server = ThreadArgs::default().resolve(Role::Server, topology);
    assert_eq!(
        (
            server.async_threads(),
            server.blocking_threads(),
            server.cpu_threads()
        ),
        (4, 128, 4)
    );
}

#[test]
fn core_cpus_list_is_read_when_the_other_name_is_absent() {
    let fake = sysfs("core_cpus_list", "0-1");
    assert_eq!(Topology::detect_in(fake.path(), 8).physical(), 4);
}

#[test]
fn no_topology_files_is_one_thread_per_core() {
    let empty = tempdir::Dir::new();
    let topology = Topology::detect_in(empty.path(), 8);
    assert_eq!((topology.threads_per_core(), topology.physical()), (1, 8));
    let junk = sysfs("thread_siblings_list", "not a list");
    assert_eq!(Topology::detect_in(junk.path(), 8).threads_per_core(), 1);
}

#[test]
fn defaults_are_limited_to_the_physical_cores_but_blocking_is_not() {
    // 2 logical CPUs on one core: one physical core.
    let one_core = Topology::new(2, 2);
    let daemon = ThreadArgs::default().resolve(Role::Daemon, one_core);
    assert_eq!(
        (
            daemon.async_threads(),
            daemon.blocking_threads(),
            daemon.cpu_threads()
        ),
        (1, 64, 1)
    );
    // Three cores: the server's 4 workers and the CPU pool's 4 come down to 3.
    let three = Topology::new(3, 1);
    let server = ThreadArgs::default().resolve(Role::Server, three);
    assert_eq!(
        (
            server.async_threads(),
            server.blocking_threads(),
            server.cpu_threads()
        ),
        (3, 128, 3)
    );
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
    let sizes = given.resolve(Role::Daemon, Topology::new(4, 2));
    assert_eq!(
        (
            sizes.async_threads(),
            sizes.blocking_threads(),
            sizes.cpu_threads()
        ),
        (9, 3, 100)
    );
    // One flag leaves the other two at their defaults.
    let one = args(&["--cpu-threads=6"]).resolve(Role::Server, Topology::new(88, 2));
    assert_eq!(
        (
            one.async_threads(),
            one.blocking_threads(),
            one.cpu_threads()
        ),
        (4, 128, 6)
    );
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
    let sizes = ThreadArgs::default().resolve(Role::Daemon, Topology::new(88, 2));
    assert_eq!(
        sizes.describe(),
        "threads: async=2 blocking=64 cpu=4 (cpus=88 threads_per_core=2 physical_cores=44)"
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
    .resolve(Role::Server, Topology::new(16, 2));
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
