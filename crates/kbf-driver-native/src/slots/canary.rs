//! Profile canaries: before a lease is handed a pool user, the daemon runs each door the
//! sandbox profile is meant to shut as that user, under the exact profile generated for
//! the lease, and the door must be refused (`EPERM` or `EACCES`). No canary refused, no
//! lease: the profile is asserted, not trusted
//! (`~/workplace/mac-pool-experiments-scratch/08_revised_design.md`, section 1.6).
//!
//! A canary is a row of [`CANARIES`]: its name, the profile rule it stands for
//! (`deny`) and its [`Op`]. The probe is this binary's `__canary <name> <lease>` mode
//! ([`dispatch`]): it makes the syscall and prints one line, `canary <name> refused
//! errno=<n>` or `canary <name> allowed`, exiting [`REFUSED`], [`ALLOWED`], [`FAILED`]
//! (the call failed for another reason) or [`USAGE`]. [`judge`] reads that back and
//! [`check`] runs the table, stopping at the first canary that is not refused. A new
//! deny gets its canary by adding a row and an [`Op`]; the macOS side
//! ([`macos`](super::macos)) runs a probe as the slot's user under the lease's profile.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The argument after argv0 that selects this binary's canary probe mode.
pub const ARG: &str = "__canary";

/// How long one canary may take, launch included (the design measured 64 to 125 ms).
pub const WITHIN: Duration = Duration::from_secs(10);

/// The probe's exit code when the door was refused.
pub const REFUSED: i32 = 0;
/// The probe's exit code when the door was open.
pub const ALLOWED: i32 = 10;
/// The probe's exit code when the call failed for a reason that is not a refusal.
pub const FAILED: i32 = 11;
/// The probe's exit code for arguments it does not know.
pub const USAGE: i32 = 12;

/// Where the write canary creates its file: outside the four paths a profile allows.
const OUTSIDE_DIR: &str = "/private/tmp";
/// The root-owned file the link canary tries to link into the lease directory.
const LINK_SOURCE: &str = "/etc/hosts";
/// The name the link canary uses in the lease directory.
const LINK_NAME: &str = ".kbf-canary-link";

/// What a canary does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Create a file in [`OUTSIDE_DIR`].
    WriteOutside,
    /// `link(2)` [`LINK_SOURCE`] into the lease directory.
    LinkOutside,
}

/// One door the profile shuts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Canary {
    /// Its name in the probe's arguments and in the log.
    pub name: &'static str,
    /// The line of the generated profile that shuts the door.
    pub deny: &'static str,
    pub op: Op,
}

/// Every canary, in the order they run. A rule of the profile gets its row here.
pub const CANARIES: &[Canary] = &[
    Canary {
        name: "write-outside",
        deny: "(deny file-write*)",
        op: Op::WriteOutside,
    },
    Canary {
        name: "link-outside",
        deny: "(deny file-write*)",
        op: Op::LinkOutside,
    },
];

/// The calls a probe makes, so a test can stand in for the kernel.
pub trait Fs {
    /// Creates `path`, which must not exist.
    ///
    /// # Errors
    /// The call's error.
    fn create(&self, path: &Path) -> io::Result<()>;
    /// Hard-links `from` as `to`.
    ///
    /// # Errors
    /// The call's error.
    fn link(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Removes `path`, if it is there.
    fn remove(&self, path: &Path);
}

/// The real calls.
#[derive(Debug)]
pub struct RealFs;

impl Fs for RealFs {
    fn create(&self, path: &Path) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(drop)
    }

    fn link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)
    }

    fn remove(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

/// What a probe saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    /// Refused with this errno (`EPERM` or `EACCES`).
    Refused(i32),
    /// The call succeeded: the door is open.
    Allowed,
    /// The call failed, but not with a refusal (say, the directory is missing): the
    /// canary proved nothing.
    Failed(String),
}

/// `result` as what the probe saw.
fn classify(result: io::Result<()>) -> Seen {
    match result {
        Ok(()) => Seen::Allowed,
        Err(e) => match e.raw_os_error() {
            Some(n) if n == libc::EPERM || n == libc::EACCES => Seen::Refused(n),
            _ => Seen::Failed(e.to_string()),
        },
    }
}

/// Makes `op`'s call with `lease` as the lease directory, and removes what an open door
/// let it create.
pub fn probe(fs: &dyn Fs, op: Op, lease: &Path) -> Seen {
    let (made, path) = match op {
        Op::WriteOutside => {
            let path = Path::new(OUTSIDE_DIR).join(format!(".kbf-canary-{}", std::process::id()));
            (fs.create(&path), path)
        }
        Op::LinkOutside => {
            let path = lease.join(LINK_NAME);
            (fs.link(Path::new(LINK_SOURCE), &path), path)
        }
    };
    let seen = classify(made);
    if seen == Seen::Allowed {
        fs.remove(&path);
    }
    seen
}

/// The probe mode: when `args` (argv, with argv0) select it, runs the canary they name
/// against the lease directory they give and returns the exit code; `None` otherwise.
pub fn dispatch(args: &[OsString], fs: &dyn Fs, out: &mut dyn io::Write) -> Option<i32> {
    if args.get(1).is_none_or(|a| a != ARG) {
        return None;
    }
    let found = args.get(2).and_then(|name| {
        CANARIES
            .iter()
            .find(|c| name.to_str() == Some(c.name))
            .zip(args.get(3))
    });
    let Some((canary, lease)) = found else {
        let _ = writeln!(out, "canary: usage: {ARG} <name> <lease dir>");
        return Some(USAGE);
    };
    let seen = probe(fs, canary.op, &PathBuf::from(lease));
    let name = canary.name;
    let (line, code) = match seen {
        Seen::Refused(errno) => (format!("canary {name} refused errno={errno}"), REFUSED),
        Seen::Allowed => (format!("canary {name} allowed"), ALLOWED),
        Seen::Failed(why) => (format!("canary {name} failed {why}"), FAILED),
    };
    let _ = writeln!(out, "{line}");
    Some(code)
}

/// What running a probe gave the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    /// The exit code; `None` if a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    /// Launch to exit, as the daemon measured it.
    pub took: Duration,
}

/// What a canary showed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Verdict {
    #[error("refused (errno {0})")]
    Refused(i32),
    #[error("NOT refused: the call succeeded")]
    Allowed,
    #[error("could not run: {0}")]
    CannotRun(String),
}

/// What `ran` shows for `canary`: refused only when the probe exited [`REFUSED`] and
/// said so with a refusing errno; anything unrecognised is a canary that could not run.
#[must_use]
pub fn judge(canary: &Canary, ran: &Ran) -> Verdict {
    let said = ran.stdout.trim();
    let name = canary.name;
    let refused = said
        .strip_prefix(&format!("canary {name} refused errno="))
        .and_then(|n| n.parse::<i32>().ok())
        .filter(|n| *n == libc::EPERM || *n == libc::EACCES);
    match (ran.code, refused) {
        (Some(REFUSED), Some(errno)) => Verdict::Refused(errno),
        (Some(ALLOWED), _) if said == format!("canary {name} allowed") => Verdict::Allowed,
        _ => Verdict::CannotRun(format!("exit {:?}, output {said:?}", ran.code)),
    }
}

/// A canary that was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passed {
    pub name: &'static str,
    pub errno: i32,
    pub took: Duration,
}

/// The first canary that was not refused, and why.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("canary {name} ({deny}): {verdict}")]
pub struct Failure {
    pub name: &'static str,
    pub deny: &'static str,
    pub verdict: Verdict,
}

/// Runs every canary of `canaries` with `run` (which starts the probe as the slot's user
/// under the lease's profile) and returns what each showed, or the first one that was
/// not refused. A canary that could not be started or timed out is a failure too. Every
/// result is logged with the canary's name and how long it took.
///
/// # Errors
/// The first canary that was allowed or could not run.
pub fn check(
    canaries: &[Canary],
    run: &mut dyn FnMut(&Canary) -> io::Result<Ran>,
) -> Result<Vec<Passed>, Failure> {
    let mut passed = Vec::new();
    for canary in canaries {
        let verdict = match run(canary) {
            Ok(ran) => match judge(canary, &ran) {
                Verdict::Refused(errno) => {
                    tracing::info!(canary = canary.name, errno, took = ?ran.took, "profile canary refused");
                    passed.push(Passed {
                        name: canary.name,
                        errno,
                        took: ran.took,
                    });
                    continue;
                }
                other => other,
            },
            Err(e) => Verdict::CannotRun(e.to_string()),
        };
        tracing::error!(canary = canary.name, deny = canary.deny, %verdict, "profile canary failed: no lease for this user");
        return Err(Failure {
            name: canary.name,
            deny: canary.deny,
            verdict,
        });
    }
    Ok(passed)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    type Answer = fn() -> io::Result<()>;

    /// A kernel that answers every call with `answer` and records them.
    struct Fake {
        answer: Answer,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(answer: Answer) -> Self {
            Self {
                answer,
                calls: RefCell::default(),
            }
        }
    }

    impl Fs for Fake {
        fn create(&self, path: &Path) -> io::Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("create {}", path.display()));
            (self.answer)()
        }
        fn link(&self, from: &Path, to: &Path) -> io::Result<()> {
            let call = format!("link {} {}", from.display(), to.display());
            self.calls.borrow_mut().push(call);
            (self.answer)()
        }
        fn remove(&self, path: &Path) {
            self.calls
                .borrow_mut()
                .push(format!("remove {}", path.display()));
        }
    }

    fn errno(n: i32) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(n))
    }

    fn row(name: &str) -> &'static Canary {
        CANARIES.iter().find(|c| c.name == name).expect("row")
    }

    fn ran(code: i32, stdout: &str) -> Ran {
        Ran {
            code: Some(code),
            stdout: stdout.to_owned(),
            took: Duration::from_millis(80),
        }
    }

    fn args(rest: &[&str]) -> Vec<OsString> {
        let mut all = vec![OsString::from("kbf")];
        all.extend(rest.iter().map(OsString::from));
        all
    }

    /// Catches a probe that reads `EPERM` or `EACCES` as anything but a refusal, or
    /// takes another error (a missing directory) or a success for one: only a refusal
    /// may pass a canary.
    #[test]
    fn only_eperm_and_eacces_are_refusals() {
        assert_eq!(classify(errno(libc::EPERM)), Seen::Refused(libc::EPERM));
        assert_eq!(classify(errno(libc::EACCES)), Seen::Refused(libc::EACCES));
        assert_eq!(classify(Ok(())), Seen::Allowed);
        let Seen::Failed(why) = classify(errno(libc::ENOENT)) else {
            panic!("ENOENT is not a refusal");
        };
        assert!(why.contains("No such file"), "{why}");
        assert!(matches!(
            classify(Err(io::Error::other("no errno"))),
            Seen::Failed(_)
        ));
    }

    /// Catches a write canary that does not write outside the allowed paths, a link
    /// canary that does not link a root-owned file into the lease directory, an open
    /// door that is left open (the file stays), and a refused one that is "cleaned up".
    #[test]
    fn each_op_makes_its_call_and_removes_what_an_open_door_made() {
        let lease = Path::new("/lease");
        let open = Fake::new(|| Ok(()));
        assert_eq!(probe(&open, Op::WriteOutside, lease), Seen::Allowed);
        assert_eq!(probe(&open, Op::LinkOutside, lease), Seen::Allowed);
        let calls = open.calls.borrow();
        let pid = std::process::id();
        assert_eq!(
            *calls,
            [
                format!("create /private/tmp/.kbf-canary-{pid}"),
                format!("remove /private/tmp/.kbf-canary-{pid}"),
                "link /etc/hosts /lease/.kbf-canary-link".to_owned(),
                "remove /lease/.kbf-canary-link".to_owned(),
            ]
        );
        let shut = Fake::new(|| errno(libc::EPERM));
        assert_eq!(
            probe(&shut, Op::LinkOutside, lease),
            Seen::Refused(libc::EPERM)
        );
        assert_eq!(shut.calls.borrow().len(), 1, "nothing to remove");
    }

    /// Catches real calls that do not create, link and remove (a create that
    /// overwrites would let a stale file hide an open door).
    #[test]
    fn the_real_calls_create_link_and_remove() {
        let scratch = crate::unit_dir("canary-real-calls");
        let dir = scratch.path();
        let file = dir.join("f");
        RealFs.create(&file).expect("create");
        assert!(
            matches!(classify(RealFs.create(&file)), Seen::Failed(_)),
            "creating twice is not a refusal"
        );
        let linked = dir.join("l");
        RealFs.link(&file, &linked).expect("link");
        RealFs.remove(&linked);
        RealFs.remove(&file);
        RealFs.remove(&file);
        assert!(!linked.exists() && !file.exists());
    }

    /// Catches a probe mode that answers a process that did not ask for it, that runs a
    /// canary it does not know, or that prints a line the daemon's [`judge`] cannot
    /// read back (the line and the exit code are the whole protocol).
    #[test]
    fn the_probe_mode_prints_what_judge_reads() {
        let mut out = Vec::new();
        assert_eq!(dispatch(&args(&[]), &RealFs, &mut out), None);
        assert_eq!(dispatch(&args(&["other"]), &RealFs, &mut out), None);
        assert!(out.is_empty());
        for bad in [&[ARG][..], &[ARG, "nope", "/l"], &[ARG, "write-outside"]] {
            assert_eq!(
                dispatch(&args(bad), &RealFs, &mut out),
                Some(USAGE),
                "{bad:?}"
            );
        }
        let cases: [(Answer, i32); 3] = [
            (|| errno(libc::EACCES), REFUSED),
            (|| Ok(()), ALLOWED),
            (|| errno(libc::ENOENT), FAILED),
        ];
        for canary in CANARIES {
            for (answer, code) in cases {
                let mut out = Vec::new();
                let fake = Fake::new(answer);
                let got = dispatch(&args(&[ARG, canary.name, "/lease"]), &fake, &mut out);
                assert_eq!(got, Some(code), "{}", canary.name);
                let text = String::from_utf8(out).expect("utf8");
                let verdict = judge(canary, &ran(code, &text));
                match code {
                    REFUSED => assert_eq!(verdict, Verdict::Refused(libc::EACCES)),
                    ALLOWED => assert_eq!(verdict, Verdict::Allowed),
                    _ => assert!(matches!(verdict, Verdict::CannotRun(_)), "{verdict:?}"),
                }
            }
        }
    }

    /// Catches a judge that passes a canary on the exit code alone (a probe that died
    /// early exits 0 on some path), on a line for another canary, on a refusal with an
    /// errno that is not one, or on a signal; and one that reports an open door as
    /// anything but open.
    #[test]
    fn judge_passes_only_the_exact_refusal() {
        let c = row("write-outside");
        let good = "canary write-outside refused errno=1\n";
        assert_eq!(judge(c, &ran(REFUSED, good)), Verdict::Refused(1));
        for bad in [
            ran(REFUSED, ""),
            ran(REFUSED, "canary link-outside refused errno=1"),
            ran(REFUSED, "canary write-outside refused errno=2"),
            ran(REFUSED, "canary write-outside refused errno=x"),
            ran(FAILED, good),
            ran(127, ""),
            Ran {
                code: None,
                stdout: good.to_owned(),
                took: Duration::ZERO,
            },
        ] {
            assert!(matches!(judge(c, &bad), Verdict::CannotRun(_)), "{bad:?}");
        }
        assert_eq!(
            judge(c, &ran(ALLOWED, "canary write-outside allowed")),
            Verdict::Allowed
        );
        assert!(matches!(
            judge(c, &ran(ALLOWED, "something else")),
            Verdict::CannotRun(_)
        ));
    }

    /// Catches a check that gives a lease a user when one canary is allowed (the
    /// "not offered" branch), when one cannot be started or times out (the "cannot
    /// run" branch), or that skips the later canaries after a pass, and one that does
    /// not name the canary, its deny and the result.
    #[test]
    fn check_passes_only_when_every_canary_is_refused() {
        let refused = |c: &Canary| Ok(ran(REFUSED, &format!("canary {} refused errno=1", c.name)));
        let passed = check(CANARIES, &mut |c| refused(c)).expect("all refused");
        assert_eq!(passed.len(), CANARIES.len());
        assert_eq!(passed[0].name, "write-outside");
        assert_eq!(passed[0].took, Duration::from_millis(80));

        let open = check(CANARIES, &mut |c| {
            if c.name == "link-outside" {
                Ok(ran(ALLOWED, "canary link-outside allowed"))
            } else {
                refused(c)
            }
        })
        .expect_err("a door is open");
        assert_eq!(open.name, "link-outside");
        assert_eq!(open.verdict, Verdict::Allowed);
        assert_eq!(
            open.to_string(),
            "canary link-outside ((deny file-write*)): NOT refused: the call succeeded"
        );

        let mut seen = 0;
        let cannot = check(CANARIES, &mut |_| {
            seen += 1;
            Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"))
        })
        .expect_err("cannot run");
        assert_eq!(seen, 1, "stops at the first failure");
        assert_eq!(cannot.verdict, Verdict::CannotRun("timed out".to_owned()));
        assert_eq!(check(&[], &mut |_| unreachable!()), Ok(vec![]));
    }
}
