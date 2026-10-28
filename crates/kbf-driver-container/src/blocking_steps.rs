//! The blocking steps of a lease's run, each one task of tokio's blocking pool.
//!
//! Each step reads or writes files of the node (a cgroup, a log, the pressure file) or
//! waits for a child, and the kernel can take seconds to answer. They are separate
//! functions so that `workers_tests` can hold each one at its first system call and
//! check that the call is not made on an async worker.

use std::path::{Path, PathBuf};
use std::time::Instant;

use kbf_daemon::RuntimeError;
use kbf_proto::worker::ResourceUsage;
use kbf_types::Resources;

use crate::cgroup::LeaseCgroup;
use crate::cgroup_usage::{NumaSample, Start, resource_usage};
use crate::keeper::Keeper;
use crate::numa::Pin;
use crate::off_worker::off_worker;
use crate::runtime::failed;

/// Creates the files the container's stdout and stderr go to.
pub(crate) async fn create_logs(
    stdout: &Path,
    stderr: &Path,
) -> Result<(std::fs::File, std::fs::File), RuntimeError> {
    let (out, err) = (stdout.to_owned(), stderr.to_owned());
    off_worker(move || {
        let stdout = std::fs::File::create(&out).map_err(|e| failed(&out, &e))?;
        let stderr = std::fs::File::create(&err).map_err(|e| failed(&err, &e))?;
        Ok((stdout, stderr))
    })
    .await
}

/// What [`make_cgroup`] did.
pub(crate) struct Made {
    /// The parent's own OOM count before the lease, if it could be read.
    pub(crate) backstop_ooms: Option<u64>,
    /// The cgroup made and its keeper started, or what failed.
    pub(crate) keeper: std::io::Result<Keeper>,
    /// The pin, given back.
    pub(crate) pin: Option<Pin>,
}

/// Makes the lease cgroup with `resources`, and starts the keeper that kills it if the
/// daemon dies. Waits on the kernel and on the keeper's first answer.
pub(crate) async fn make_cgroup(
    cgroup: LeaseCgroup,
    resources: Resources,
    pin: Option<Pin>,
) -> Made {
    off_worker(move || {
        // `actions/`'s own OOM count before the lease, to say afterwards whether its
        // limit killed the lease.
        let backstop_ooms = cgroup.parent_ooms().ok();
        let kill = cgroup.dir().join("cgroup.kill");
        let keeper = (cgroup.create(resources, pin.as_ref())).and_then(|()| Keeper::watch(&kill));
        Made {
            backstop_ooms,
            keeper,
            pin,
        }
    })
    .await
}

/// The CPU pressure file `psi` and the CPU booked, at the start of a lease.
pub(crate) async fn read_start(psi: PathBuf, millicpus: u64) -> Start {
    off_worker(move || Start::read(&psi, millicpus)).await
}

/// Whether the kernel's OOM killer ended the lease (see [`LeaseCgroup::oom_outcome`]).
pub(crate) async fn oom_outcome(
    cgroup: LeaseCgroup,
    cap: Option<u64>,
    before: Option<u64>,
) -> std::io::Result<Option<RuntimeError>> {
    off_worker(move || cgroup.oom_outcome(cap, before)).await
}

/// What the cgroup `dir` counted, with the best NUMA sample, `ran` after the container
/// started.
pub(crate) async fn usage(
    dir: PathBuf,
    numa: NumaSample,
    ran: Instant,
    started_under: Start,
) -> ResourceUsage {
    off_worker(move || resource_usage(numa.finish(&dir), ran.elapsed(), started_under)).await
}
