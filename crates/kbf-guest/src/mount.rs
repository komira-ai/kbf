//! `mount-share`: mounts one virtiofs tag at a mount point, retrying a bounded number of
//! times, and fails loudly when it cannot.
//!
//! The base image's mount daemon runs this from a copy of `kbf-guest` on the guest's own
//! disk (the copy on the share cannot be reached before the share is mounted). It runs as
//! root, like the probes. launchd does not restart it: when the mount still fails after
//! the last attempt the process says so on stderr (launchd writes that to the daemon's
//! log) and exits non-zero, once.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `mount-share`'s arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct MountArgs {
    /// The virtiofs tag to mount. The system mounts the automount tag on its own, so
    /// mounting that one by hand is refused.
    #[arg(long)]
    pub tag: String,
    /// Where to mount it (created when missing).
    #[arg(long)]
    pub mountpoint: PathBuf,
    /// How many times to try before giving up.
    #[arg(long, default_value_t = 40, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub attempts: u32,
    /// Milliseconds to wait between two attempts.
    #[arg(long, default_value_t = 3000)]
    pub delay_ms: u64,
}

/// The tag the system mounts by itself in a guest.
pub const AUTOMOUNT_TAG: &str = "com.apple.virtio-fs.automount";

/// What `mount-share` asks of the guest.
pub trait Mounter {
    /// Mounts `tag` at `mountpoint`; the tool's error text when it fails.
    ///
    /// # Errors
    /// The mount point could not be made or the mount failed.
    fn mount(&self, tag: &str, mountpoint: &Path) -> Result<(), String>;
    /// Waits `d`.
    fn pause(&self, d: Duration);
}

/// Mounts `args.tag`, trying up to `args.attempts` times, `args.delay_ms` apart. Each
/// failed attempt is written to `log`. Returns the attempt that worked.
///
/// # Errors
/// The tag is the automount tag, or every attempt failed (the text names the count and
/// the last error).
pub fn mount_share(
    mounter: &dyn Mounter,
    args: &MountArgs,
    log: &mut dyn Write,
) -> Result<u32, String> {
    if args.tag == AUTOMOUNT_TAG {
        return Err(format!(
            "{AUTOMOUNT_TAG} is mounted by the system itself, at /Volumes/My Shared Files; \
             a second mount of it reports `Resource busy`"
        ));
    }
    let mut last = String::new();
    for attempt in 1..=args.attempts {
        match mounter.mount(&args.tag, &args.mountpoint) {
            Ok(()) => return Ok(attempt),
            Err(e) => {
                let _ = writeln!(
                    log,
                    "kbf-guest mount-share: attempt {attempt} of {}: {e}",
                    args.attempts
                );
                last = e;
            }
        }
        if attempt < args.attempts {
            mounter.pause(Duration::from_millis(args.delay_ms));
        }
    }
    Err(format!(
        "{} did not mount at {} after {} attempts; last error: {last}",
        args.tag,
        args.mountpoint.display(),
        args.attempts
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Fails `fail` times, then works; records every call.
    struct Fake {
        fail: u32,
        calls: RefCell<Vec<String>>,
    }

    impl Mounter for Fake {
        fn mount(&self, tag: &str, mountpoint: &Path) -> Result<(), String> {
            let n = u32::try_from(
                self.calls
                    .borrow()
                    .iter()
                    .filter(|c| c.starts_with("mount"))
                    .count(),
            )
            .expect("n");
            self.calls
                .borrow_mut()
                .push(format!("mount {tag} {}", mountpoint.display()));
            if n < self.fail {
                Err(format!("busy {n}"))
            } else {
                Ok(())
            }
        }
        fn pause(&self, d: Duration) {
            self.calls
                .borrow_mut()
                .push(format!("pause {}", d.as_millis()));
        }
    }

    fn args(tag: &str, attempts: u32) -> MountArgs {
        MountArgs {
            tag: tag.to_owned(),
            mountpoint: "/m".into(),
            attempts,
            delay_ms: 7,
        }
    }

    fn fake(fail: u32) -> Fake {
        Fake {
            fail,
            calls: RefCell::new(Vec::new()),
        }
    }

    /// Catches a first-try success that sleeps or logs, and a tag or mount point that
    /// does not reach the tool.
    #[test]
    fn a_mount_that_works_at_once_waits_for_nothing() {
        let f = fake(0);
        let mut log = Vec::new();
        assert_eq!(mount_share(&f, &args("t", 3), &mut log), Ok(1));
        assert_eq!(*f.calls.borrow(), vec!["mount t /m".to_owned()]);
        assert!(log.is_empty());
    }

    /// Catches a retry that stops early, one that pauses the wrong time or after the
    /// success, and a failed attempt that is not logged.
    #[test]
    fn a_busy_mount_is_retried_with_the_delay_and_each_failure_is_logged() {
        let f = fake(2);
        let mut log = Vec::new();
        assert_eq!(mount_share(&f, &args("t", 5), &mut log), Ok(3));
        assert_eq!(
            *f.calls.borrow(),
            [
                "mount t /m",
                "pause 7",
                "mount t /m",
                "pause 7",
                "mount t /m"
            ]
        );
        let text = String::from_utf8(log).expect("utf8");
        assert!(text.contains("attempt 1 of 5: busy 0"), "{text}");
        assert!(text.contains("attempt 2 of 5: busy 1"), "{text}");
        assert_eq!(text.lines().count(), 2);
    }

    /// Catches a retry without a bound (the loop the first E1 run showed, 101 failures),
    /// a failure that does not say how many attempts or the last error, a pause after the
    /// last attempt, and a bound of zero that mounts nothing and says success.
    #[test]
    fn a_mount_that_never_works_stops_at_the_bound_and_says_why() {
        let f = fake(u32::MAX);
        let mut log = Vec::new();
        let e = mount_share(&f, &args("t", 3), &mut log).unwrap_err();
        assert!(e.contains("after 3 attempts"), "{e}");
        assert!(e.contains("last error: busy 2"), "{e}");
        let calls = f.calls.borrow();
        assert_eq!(calls.iter().filter(|c| c.starts_with("mount")).count(), 3);
        assert_eq!(calls.iter().filter(|c| c.starts_with("pause")).count(), 2);
        assert_eq!(calls.last().expect("call"), "mount t /m");
        let none = mount_share(&fake(0), &args("t", 0), &mut Vec::new()).unwrap_err();
        assert!(none.contains("after 0 attempts"), "{none}");
    }

    /// Catches the automount tag being mounted by hand (the cause of the `Resource busy`
    /// loop): it is refused before any mount is tried.
    #[test]
    fn the_automount_tag_is_refused_without_a_try() {
        let f = fake(0);
        let e = mount_share(&f, &args(AUTOMOUNT_TAG, 3), &mut Vec::new()).unwrap_err();
        assert!(e.contains("mounted by the system itself"), "{e}");
        assert!(f.calls.borrow().is_empty());
    }
}
