//! C1 of the clean: end the slot's launchd domains. The tool is [`Launchd`] (`launchctl`,
//! run by [`super::macos::host`]); the rules are here, tested on every platform with a
//! fake.
//!
//! - The domains `user/<uid>` and `gui/<uid>` are booted out **whole**, once each. A job
//!   is never booted out by its label: inside a domain that is going down, a per-label
//!   bootout answers 124 ("tearing down"), which is waited out by booting the **domain**
//!   out again. The bootout is the **last** verb the clean runs on either domain.
//! - Nothing prints the domain afterwards, here or in the steps that follow. A root
//!   `launchctl print user/<uid>` of a domain that is gone **makes it again** (measured
//!   on a Mac: the print reports `creator = launchctl[<pid>]` and launchd starts the
//!   per-user agents in it, `distnoted` among them, as the uid, which the next lease
//!   would inherit). That a domain and its jobs are gone is read from the process table
//!   by uid, which makes nothing: C3 kills and C11 looks at it.
//! - The wait is bounded ([`POLLS`] bootouts, [`POLL_PAUSE`] apart), and so is the whole
//!   step: every `launchctl` call is cut to what is left of [`WITHIN`] (the slot clean
//!   runs before the daemon says `Hello`, so a hung launchd must not hold the start for
//!   the sum of its alarms).

use std::time::Duration;

use super::probe;

/// How long one `bootout` may take before it is killed: the domain's teardown runs on in
/// launchd, and booting it out again is what waits for it.
pub(crate) const BOOTOUT_ALARM: Duration = Duration::from_secs(20);

/// The most the whole step takes, all its bootouts together.
pub(crate) const WITHIN: Duration = Duration::from_secs(60);

/// How many times a domain is booted out while it says it is tearing down, and the pause
/// between.
pub(crate) const POLLS: usize = 150;
pub(crate) const POLL_PAUSE: Duration = Duration::from_millis(200);

/// What one `launchctl` run said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Said {
    /// It exited with `code`.
    Exited { code: i32 },
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

    /// A clock: the time since some fixed moment.
    fn now(&self) -> Duration;
}

/// Boots the slot's domains out, `user/<uid>` then `gui/<uid>`. Nothing else is run on
/// them: see the module documentation for why no print follows.
///
/// # Errors
/// A domain was still tearing down after [`POLLS`] bootouts or the step ran past
/// [`WITHIN`], or `launchctl` could not be run.
pub(crate) fn end(launchd: &dyn Launchd, uid: u32) -> Result<(), String> {
    let began = launchd.now();
    // The alarm for a call: `alarm`, or what is left of [`WITHIN`] if less.
    let limit =
        |alarm: Duration| alarm.min(WITHIN.saturating_sub(launchd.now().saturating_sub(began)));
    for domain in [format!("user/{uid}"), format!("gui/{uid}")] {
        boot_out(launchd, &domain, &limit)?;
    }
    Ok(())
}

/// Boots `domain` out until launchd stops saying it is tearing down. Not an error
/// whatever else it says: a domain that does not exist cannot be booted out (113), and
/// the process table is the judge of what is left.
fn boot_out(
    launchd: &dyn Launchd,
    domain: &str,
    limit: &dyn Fn(Duration) -> Duration,
) -> Result<(), String> {
    for look in 0..POLLS {
        if look > 0 {
            launchd.pause(POLL_PAUSE);
        }
        if limit(BOOTOUT_ALARM).is_zero() {
            return Err(format!(
                "launchctl on {domain} did not settle within {WITHIN:?}"
            ));
        }
        match launchd.run(&["bootout", domain], limit(BOOTOUT_ALARM)) {
            Said::Missing(why) => return Err(format!("launchctl bootout {domain}: {why}")),
            Said::TimedOut => {}
            Said::Exited { code } if code == probe::TEARING_DOWN => {}
            Said::Exited { .. } => return Ok(()),
        }
    }
    Err(format!(
        "{domain} still tearing down after {POLLS} bootouts"
    ))
}

#[cfg(test)]
mod tests;
