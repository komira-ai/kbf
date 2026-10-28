//! `role-reap`: ends the process group a `role-run` started, for a daemon that crashed
//! and lost the run's handle.
//!
//! The daemon recorded the run's leader before it could crash: its pid (which is the
//! group's id, because the started process leads a group of its own), its start time and
//! the boot it ran in. The helper does not take the group id on trust. It kills only
//! while, in this boot, the process with that pid is still the one that started then
//! (same start time), still leads its group, and is the VM user's (real and effective
//! uid), so a recycled pid, a group someone else leads and a record naming a lease
//! user's or root's process all end in no signal. A group id of 1 or less (`-1`, `0` and
//! `1` mean every process, the caller's group and init's) is refused before anything is
//! read.
//!
//! The kill repeats until the group has no live member: SIGKILL to the group, a pause,
//! the table again. From the second round on, a member that is not the VM user's stops
//! it. A leader that is gone, a start time that differs or a boot that differs is a
//! success that kills nothing: there is no such run any more.
//!
//! Not covered: a member that left the group (`setsid`) before the leader died; the VM
//! helper starts no such process.

use super::{Helper, Process};

/// How many times the group is killed and looked at before the reap gives up.
pub const REAP_ROUNDS: usize = 10;

impl Helper {
    /// `role-reap <pgid> <start> <boot>`; see the module documentation.
    ///
    /// # Errors
    /// Why it was refused, or processes of the group survived [`REAP_ROUNDS`] kills.
    pub fn role_reap(&self, pgid: i32, start: u64, boot: &str) -> Result<(), String> {
        if pgid <= 1 {
            return Err(format!(
                "process group {pgid} is not one role-reap will signal"
            ));
        }
        let _ledger = self.lock();
        let role = self.vm_role()?;
        let uid = role.account.uid;
        let current = self
            .host
            .boot_session()
            .map_err(|why| format!("reading the boot session: {why}"))?;
        if current != boot {
            tracing::info!(pgid, "the run is of an earlier boot; nothing to end");
            return Ok(());
        }
        let table = self.table()?;
        let Some(leader) = table.iter().find(|process| process.pid == pgid) else {
            tracing::info!(pgid, "the run's leader is gone; nothing to end");
            return Ok(());
        };
        if leader.start != start {
            tracing::info!(pgid, "pid {pgid} is another process now; nothing to end");
            return Ok(());
        }
        if leader.pgid != pgid {
            return Err(format!("process {pgid} does not lead its process group"));
        }
        if !owned_by(leader, uid) {
            return Err(format!(
                "process {pgid} is not the VM user's (uid {} and real uid {}, not {uid})",
                leader.uid, leader.ruid
            ));
        }
        for round in 1..=REAP_ROUNDS {
            self.host
                .kill_group(pgid)
                .map_err(|why| format!("killing process group {pgid}: {why}"))?;
            self.host.pause();
            let left: Vec<Process> = self
                .table()?
                .into_iter()
                .filter(|process| process.pgid == pgid && !process.zombie)
                .collect();
            if left.is_empty() {
                tracing::info!(pgid, round, "no process of the run's group is left");
                return Ok(());
            }
            if let Some(other) = left.iter().find(|process| !owned_by(process, uid)) {
                return Err(format!(
                    "process {} in group {pgid} is not the VM user's; not signalling again",
                    other.pid
                ));
            }
        }
        Err(format!(
            "processes of group {pgid} remain after {REAP_ROUNDS} kills"
        ))
    }

    fn table(&self) -> Result<Vec<Process>, String> {
        self.host
            .processes()
            .map_err(|why| format!("reading the process table: {why}"))
    }
}

/// Whether both of `process`'s uids are `uid`.
fn owned_by(process: &Process, uid: u32) -> bool {
    process.uid == uid && process.ruid == uid
}

#[cfg(test)]
mod tests;
