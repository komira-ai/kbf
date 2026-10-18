//! The installer's own view of the leases on this host: the check that refuses an
//! activation while work is running is made here, from what root observes, never from
//! anything in the order record (the node's check is a courtesy, and the record is the
//! service user's word).
//!
//! The real sources are [`CgroupLeases`] (Linux: the lease cgroups of the daemon's
//! `actions/` cgroup) and [`PoolLeases`] (macOS: the processes of the lease-user pool,
//! which launchd runs a lease's jobs as). They are not yet chosen by the binary; until
//! they are, it uses [`NotWired`], which says it cannot tell, and an activation is
//! refused (fail closed).

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::run::{Runner, Spawn};

/// How many leases are live on this host.
pub trait LeaseSource {
    /// The count, or why it cannot be told.
    fn live(&self) -> Result<usize, String>;
}

/// The prefix of a lease's cgroup, `kbf-lease-<term>-<seq>` (kbf-driver-container).
pub const LEASE_PREFIX: &str = "kbf-lease-";

/// The cgroup2 mount.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The slice every kbf unit is in (`Slice=kbf.slice`).
pub const SLICE: &str = "kbf.slice";

/// Linux: a lease is live while its cgroup (`<unit>/actions/kbf-lease-*`) is populated,
/// that is while a process runs in it or under it (`cgroup.events` says `populated 1`; the
/// container's own cgroup is a child, so the lease's `cgroup.procs` would miss it). A lease
/// cgroup outlives its action, empty, to be read for its OOM events, and is not counted.
///
/// The daemon delegates `actions/` under its own unit's cgroup, so the path depends on
/// the mode's unit (`kbf-daemon.service`, `kbf-storage-and-daemon.service` or
/// `kbf-all-in-one.service`; a frontend or a storage-only node runs no builds). No
/// `actions/` in a unit cgroup that exists means the daemon never ran an action: none is
/// live. A unit cgroup that does not exist cannot tell: the path may be wrong, and a
/// wrong path must not read as "idle".
pub struct CgroupLeases {
    /// The unit's cgroup directory, `<cgroup root>/kbf.slice/<unit>`.
    pub unit: PathBuf,
}

impl CgroupLeases {
    /// The lease source of the daemon in `unit` (a unit file name) under a cgroup2 mount
    /// at `cgroup_root` ([`CGROUP_ROOT`] on a host).
    pub fn of_unit(cgroup_root: &Path, unit: &str) -> CgroupLeases {
        CgroupLeases {
            unit: cgroup_root.join(SLICE).join(unit),
        }
    }

    /// The daemon's delegated `actions/` cgroup.
    pub fn actions(&self) -> PathBuf {
        self.unit.join("actions")
    }
}

impl LeaseSource for CgroupLeases {
    fn live(&self) -> Result<usize, String> {
        let actions = self.actions();
        let listed = match fs::read_dir(&actions) {
            Ok(listed) => listed,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return if self.unit.is_dir() {
                    Ok(0)
                } else {
                    Err(format!("{}: no such cgroup", self.unit.display()))
                };
            }
            Err(e) => return Err(format!("{}: {e}", actions.display())),
        };
        let mut live = 0;
        for entry in listed {
            let entry = entry.map_err(|e| format!("{}: {e}", actions.display()))?;
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with(LEASE_PREFIX) {
                continue;
            }
            let events = entry.path().join("cgroup.events");
            let text =
                fs::read_to_string(&events).map_err(|e| format!("{}: {e}", events.display()))?;
            if text.lines().any(|l| l.trim() == "populated 1") {
                live += 1;
            }
        }
        Ok(live)
    }
}

/// The program that lists every process's uid.
pub const PS: &str = "/bin/ps";

/// macOS: a lease runs as one user of the pool, `first_uid ..< first_uid + users`, and
/// everything it starts (launchd jobs included) runs as that uid. A pool uid with a
/// process is a live lease. The processes are listed by `ps` (absolute path, empty
/// environment).
pub struct PoolLeases<'a> {
    pub runner: &'a dyn Runner,
    pub first_uid: u32,
    pub users: u32,
    pub timeout: Duration,
}

impl LeaseSource for PoolLeases<'_> {
    fn live(&self) -> Result<usize, String> {
        let spawn = Spawn {
            program: PathBuf::from(PS),
            args: vec!["-A".into(), "-o".into(), "uid=".into()],
            env: Vec::new(),
            cwd: PathBuf::from("/"),
            run_as: None,
            timeout: self.timeout,
        };
        let ran = self.runner.run(&spawn).map_err(|e| format!("{PS}: {e}"))?;
        if ran.timed_out || ran.code != Some(0) {
            return Err(format!("{PS} did not finish well (status {:?})", ran.code));
        }
        let text = String::from_utf8(ran.stdout).map_err(|_| format!("{PS}: not UTF-8"))?;
        let end = self.first_uid.saturating_add(self.users);
        let mut busy = BTreeSet::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let uid: u32 = line
                .trim()
                .parse()
                .map_err(|_| format!("{PS}: {line:?} is not a uid"))?;
            if (self.first_uid..end).contains(&uid) {
                busy.insert(uid);
            }
        }
        Ok(busy.len())
    }
}

/// The source of a build that has none.
pub struct NotWired;

impl LeaseSource for NotWired {
    fn live(&self) -> Result<usize, String> {
        Err("this installer has no lease source wired in".to_owned())
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::LeaseSource;

    /// A count a test sets; `usize::MAX` is a source that cannot answer.
    #[derive(Clone, Default)]
    pub struct FakeLeases(pub Arc<AtomicUsize>);

    impl FakeLeases {
        pub fn set(&self, n: usize) {
            self.0.store(n, Ordering::SeqCst);
        }
    }

    impl LeaseSource for FakeLeases {
        fn live(&self) -> Result<usize, String> {
            match self.0.load(Ordering::SeqCst) {
                usize::MAX => Err("the cgroup cannot be read".to_owned()),
                n => Ok(n),
            }
        }
    }
}

#[cfg(test)]
#[path = "leases_tests.rs"]
mod tests;
