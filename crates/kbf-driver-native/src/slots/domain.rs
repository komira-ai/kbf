//! C1 of the clean: end the slot's launchd domains and wait until the domain lists
//! nothing of the lease's. The tool is [`Launchd`] (`launchctl`, run by
//! [`super::macos::host`]); the rules are here, tested on every platform with a fake.
//!
//! - The domains `user/<uid>` and `gui/<uid>` are booted out **whole**, once each. A job
//!   is never booted out by its label: inside a domain that is going down, a per-label
//!   bootout answers 124 ("tearing down") and was read as a failed step, which the old
//!   retry then hid.
//! - Then `launchctl print user/<uid>` is polled until the domain lists no job but
//!   launchd's own (judged by where a job's plist lies, [`probe::is_own_plist`], never by
//!   its label, which a lease chooses), or is gone. Exit 124 means "keep waiting". The
//!   poll is bounded ([`POLLS`] looks, [`POLL_PAUSE`] apart); a domain that still lists a
//!   job at the bound fails the step, naming the jobs.

use std::time::Duration;

use super::probe;

/// How long one `bootout` may take before it is killed: the domain's teardown runs on in
/// launchd, and the poll below is what waits for it.
pub(crate) const BOOTOUT_ALARM: Duration = Duration::from_secs(20);

/// How long one `print` may take.
pub(crate) const PRINT_ALARM: Duration = Duration::from_secs(20);

/// How many times the poll looks at the domain, and the pause between looks.
pub(crate) const POLLS: usize = 150;
pub(crate) const POLL_PAUSE: Duration = Duration::from_millis(200);

/// What one `launchctl` run said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Said {
    /// It exited with `code`, printing `stdout`.
    Exited { code: i32, stdout: String },
    /// It did not end within its alarm and was killed.
    TimedOut,
    /// It could not be run.
    Missing(String),
}

/// `launchctl`.
pub(crate) trait Launchd {
    /// Runs `launchctl <args>`, for at most `alarm`.
    fn run(&self, args: &[&str], alarm: Duration) -> Said;

    /// Waits `time` between two looks.
    fn pause(&self, time: Duration);
}

/// Boots the slot's domains out and waits until `user/<uid>` lists none of the lease's
/// jobs.
///
/// # Errors
/// The domain still listed jobs after [`POLLS`] looks (their labels), or `launchctl`
/// could not be run.
pub(crate) fn end(launchd: &dyn Launchd, uid: u32) -> Result<(), String> {
    for domain in [format!("user/{uid}"), format!("gui/{uid}")] {
        // Not an error whatever it says: a domain that does not exist cannot be booted
        // out, and one that is going down says 124. The poll below is the judge.
        if let Said::Missing(why) = launchd.run(&["bootout", &domain], BOOTOUT_ALARM) {
            return Err(format!("launchctl bootout {domain}: {why}"));
        }
    }
    let domain = format!("user/{uid}");
    let mut foreign = Vec::new();
    for look in 0..POLLS {
        if look > 0 {
            launchd.pause(POLL_PAUSE);
        }
        match launchd.run(&["print", &domain], PRINT_ALARM) {
            Said::Missing(why) => return Err(format!("launchctl print {domain}: {why}")),
            Said::TimedOut => {}
            Said::Exited { code: 0, stdout } => {
                foreign = foreign_jobs(launchd, &domain, &stdout);
                if foreign.is_empty() {
                    return Ok(());
                }
            }
            Said::Exited { code, .. } if code == probe::TEARING_DOWN => {}
            // The domain is not there (or cannot be printed, which the clean's check
            // stands in for): nothing of the lease's is in it.
            Said::Exited { .. } => return Ok(()),
        }
    }
    Err(format!(
        "{domain} still lists jobs after {POLLS} looks: {foreign:?}"
    ))
}

/// The labels in the printed `domain` that are not launchd's own: each is printed on its
/// own for its plist's path.
fn foreign_jobs(launchd: &dyn Launchd, domain: &str, printed: &str) -> Vec<String> {
    probe::listed_services(printed)
        .into_iter()
        .filter(|(_, label)| {
            let target = format!("{domain}/{label}");
            let own = match launchd.run(&["print", &target], PRINT_ALARM) {
                Said::Exited { code: 0, stdout } => {
                    probe::plist_path(&stdout).is_some_and(probe::is_own_plist)
                }
                _ => false,
            };
            !own
        })
        .map(|(_, label)| label)
        .collect()
}

#[cfg(test)]
mod tests;
