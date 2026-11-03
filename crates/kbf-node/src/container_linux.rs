use std::sync::Arc;

use std::time::Duration;

use kbf_driver_container::{
    ContainerConfig, ContainerLimits, ContainerRuntime, IdFiles, ImageRef, LeasePlacement,
    OutputLimits, SwapKill,
};

use super::{Cli, Error, ImageUnusable, Node, scratch};
use crate::data_dir::Plan as DataPlan;
use kbf_transfer::Cas;

/// Builds the container driver, over the daemon's data plane, and adds it to `node`
/// with what its leases may use.
pub(super) fn add<C: Cas>(
    cli: &Cli,
    node: &mut Node<C>,
    plan: &Option<DataPlan>,
) -> Result<(), Error> {
    // The checks that change nothing come first.
    let scratch = scratch(cli)?;
    let cas = node.plane(cli)?;
    // Every container's ids are this user's subordinate ids (`--userns=nomap`).
    let files = cli
        .id_files
        .as_deref()
        .map_or_else(IdFiles::system, IdFiles::in_dir);
    kbf_driver_container::check_daemon_user(&files)?;
    let mount = &cli.cgroup_root;
    let memory_max = cli
        .actions_memory_max_gib
        .map(|gib| gib.saturating_mul(1 << 30));
    let parent = match &cli.cgroup_parent {
        Some(parent) => {
            kbf_driver_container::adopt(mount, parent, memory_max)?;
            parent.clone()
        }
        None => {
            let own = std::fs::read_to_string(&cli.self_cgroup).map_err(|e| {
                super::HostRefused(format!("read {}: {e}", cli.self_cgroup.display()))
            })?;
            let memory_min = cli.supervisor_memory_min_mib.saturating_mul(1 << 20);
            let delegation = kbf_driver_container::delegate(mount, &own, memory_min, memory_max)?;
            tracing::info!(
                "cgroup {}: this daemon in {}/supervisor (memory.min {} MiB), leases under {}",
                delegation.root,
                delegation.root,
                cli.supervisor_memory_min_mib,
                delegation.actions
            );
            if let Some((cgroup, min)) = &delegation.memory_min_capped {
                tracing::warn!(
                    "cgroup {cgroup} has memory.min {min} bytes, below the daemon's {memory_min}: \
                     the kernel protects the daemon no further; set MemoryMin= on its unit \
                     and slice to at least {} MiB",
                    cli.supervisor_memory_min_mib
                );
            }
            delegation.actions
        }
    };
    // The leases' share of the CPUs: all the daemon may use but `--reserve-cpus`.
    // A limit on the actions cgroup, not a pinning: any CPU, at most this many.
    let lease_cpus = super::sizes(cli)?.lease_cpus() as u64;
    kbf_driver_container::limit_cpus(mount, &parent, lease_cpus)?;
    let capacity = kbf_driver_container::capacity(mount, &parent)?;
    tracing::info!(
        "leases may use {} CPUs and {} of memory (cgroup {parent})",
        capacity.cpus.map_or("all".to_owned(), |n| n.to_string()),
        capacity
            .memory_bytes
            .map_or("all".to_owned(), |b| format!("{} GiB", b >> 30))
    );
    // After the cgroup setup it builds on, before the runtime sweeps and serves.
    let placer = match cli.lease_placement.parse()? {
        LeasePlacement::None => None,
        LeasePlacement::NumaNode => {
            // The memory the scheduler books on this node (the report's `mem_gib`):
            // a lease that asks for all of it is the whole host's and is not pinned.
            let whole_host =
                super::offered(kbf_daemon::NodeReport::detect(&[])?, capacity).mem_bytes();
            kbf_driver_container::numa::start(&cli.numa_dir, mount, &parent, whole_host)?
        }
    };
    let mut config = ContainerConfig::new(scratch, parent);
    config.placer = placer;
    // Applied by each Welcome from here on; the limit above stands until the first.
    config.actions_cpu = Some(cli.actions_cpu.config(lease_cpus));
    config.oci.runtime = cli.oci_runtime.clone();
    config.oci.id_files = files;
    config.cgroup_root = mount.clone();
    config.outputs = OutputLimits {
        max_depth: cli.outputs.max_depth,
        max_entries: cli.outputs.max_entries,
        max_bytes: cli.outputs.max_bytes,
        max_stdio_bytes: cli.outputs.max_stdio_bytes,
    };
    config.upload_deadline = super::upload_deadline(cli);
    config.cpu = cpu_pool(cli)?;
    config.limits = limits(cli);
    config.swap_kill = swap_kill(cli);
    config.cache = crate::data_dir::given(plan.as_ref(), &cli.data);
    let runtime = ContainerRuntime::new(config, cas)?;
    // From `start`, in the runtime's context but outside any task.
    tokio::runtime::Handle::current().block_on(check_images(&runtime, &cli.images))?;
    node.add(Arc::new(runtime))?;
    node.capacity = capacity;
    Ok(())
}

/// Checks each `--image` as a lease's prepare step would: named by digest, in this
/// node's image store, one architecture's manifest. The first that is not stops the
/// start, with its fix.
async fn check_images<C: Cas>(
    runtime: &ContainerRuntime<C>,
    images: &[String],
) -> Result<(), ImageUnusable> {
    for value in images {
        let image = ImageRef::parse(value).map_err(|e| ImageUnusable(e.to_string()))?;
        runtime
            .check_pinned(&image)
            .await
            .map_err(|e| ImageUnusable(e.to_string()))?;
        tracing::info!("image {image}: in this node's image store");
    }
    Ok(())
}

/// The `--lease-swap-*` flags.
pub(super) fn swap_kill(cli: &Cli) -> SwapKill {
    SwapKill {
        floor_bytes: cli.lease_swap_kill_mib.saturating_mul(1 << 20),
        percent: cli.lease_swap_kill_percent,
        every: Duration::from_millis(cli.lease_swap_poll_ms),
    }
}

/// The CPU pool the container driver hashes outputs on, sized by `--cpu-threads`.
pub(super) fn cpu_pool(cli: &Cli) -> Result<kbf_daemon::CpuPool, Error> {
    Ok(kbf_daemon::CpuPool::new(super::sizes(cli)?.cpu_threads()))
}

/// The `--container-*` limits.
pub(super) fn limits(cli: &Cli) -> ContainerLimits {
    ContainerLimits {
        pids: cli.container_pids_limit,
        shm_mib: cli.container_shm_mib,
        nofile: cli.container_nofile,
        nproc: cli.container_nproc,
    }
}
