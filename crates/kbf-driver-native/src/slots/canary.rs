//! Profile canaries: before a lease is handed a pool user, the daemon runs each door the
//! sandbox profile is meant to shut as that user, under the exact profile generated for
//! the lease, and the door must be refused (`EPERM` or `EACCES`). A slot with a canary not
//! refused is set aside, not offered and not counted among the node's action slots until
//! its next offer ([`first_passing`]); the lease takes the next slot, or waits as when
//! none is free. The profile is asserted, not trusted.
//!
//! A canary is a row of [`CANARIES`]: its name, the profile rule it stands for
//! (`deny`) and its [`Op`]. The probe is this binary's `__canary <name> <lease>` mode
//! ([`dispatch`]): it makes the syscall and prints one line, `canary <name> refused
//! errno=<n>` or `canary <name> allowed`, exiting [`REFUSED`], [`ALLOWED`], [`FAILED`]
//! (the call failed for another reason) or [`USAGE`]. [`judge`] reads that back and
//! [`check`] runs the whole table and reports every canary that is not refused. A new
//! deny gets its canary by adding a row and an [`Op`]; the macOS side
//! ([`macos`](super::macos)) runs a probe as the slot's user under the lease's profile.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The argument after argv0 that selects this binary's canary probe mode.
pub const ARG: &str = "__canary";

/// How long one canary may take, launch included (a run took about 50 to 125 ms on the macOS runner).
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

/// What [`check`] found: the canaries that were refused and the ones that were not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub passed: Vec<Passed>,
    pub failed: Vec<Failure>,
}

impl Report {
    /// Whether every canary was refused.
    #[must_use]
    pub fn all_refused(&self) -> bool {
        self.failed.is_empty()
    }

    /// The failures, one line each, for a log: `name (deny): result; ...`.
    #[must_use]
    pub fn failures(&self) -> String {
        let lines: Vec<String> = self.failed.iter().map(ToString::to_string).collect();
        lines.join("; ")
    }
}

/// Runs every canary of `canaries` with `run` (which starts the probe as the slot's user
/// under the lease's profile), none skipped after a failure, so each can be seen red on
/// its own. A canary that could not be started or timed out is a failure too. Every
/// result is logged with the canary's name and how long it took.
pub fn check(canaries: &[Canary], run: &mut dyn FnMut(&Canary) -> io::Result<Ran>) -> Report {
    check_until(canaries, &|| false, run)
}

/// [`check`] that starts no canary once `cancelled` says so: the report holds what ran.
pub fn check_until(
    canaries: &[Canary],
    cancelled: &dyn Fn() -> bool,
    run: &mut dyn FnMut(&Canary) -> io::Result<Ran>,
) -> Report {
    let mut report = Report::default();
    for canary in canaries {
        if cancelled() {
            break;
        }
        let verdict = match run(canary) {
            Ok(ran) => match judge(canary, &ran) {
                Verdict::Refused(errno) => {
                    tracing::info!(canary = canary.name, errno, took = ?ran.took, "profile canary refused");
                    report.passed.push(Passed {
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
        tracing::error!(canary = canary.name, deny = canary.deny, %verdict, "profile canary failed: this slot is set aside");
        report.failed.push(Failure {
            name: canary.name,
            deny: canary.deny,
            verdict,
        });
    }
    report
}

/// What probing a slot decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The slot may run the lease.
    Pass,
    /// The slot is not to run it; the text says which canaries failed.
    Aside(String),
}

/// The slots a lease is offered, for [`first_passing`]. The offer holds the slot it has
/// taken until it is set aside or handed out ([`acquire`]), so that a failure of any
/// step leaves the slot with the caller, which removes the lease directory before it
/// lets the slot go.
pub trait Offer {
    type Slot: Send;
    type Err;

    /// Takes the next free slot (waiting when none is) and holds it; or says why the
    /// lease ends instead (a kill while waiting, or while the lease was prepared with
    /// the slot held).
    fn next(&mut self) -> impl Future<Output = Result<(), Self::Err>> + Send;

    /// Hands the lease's directory to the held slot and runs its canaries.
    fn probe(&mut self) -> impl Future<Output = Result<Probe, Self::Err>> + Send;

    /// Takes the lease's directory back from the held slot and sets the slot aside;
    /// `why` says which canaries failed. No slot is held afterwards. If the directory
    /// cannot be taken back the slot stays held and the lease ends with the error, so
    /// that the directory is removed before the slot is released.
    fn aside(&mut self, why: &str) -> impl Future<Output = Result<(), Self::Err>> + Send;

    /// The slot held now, if any.
    fn take_held(&mut self) -> Option<Self::Slot>;
}

/// Takes slots until one passes its probe, setting each one that does not aside (it is
/// not offered again until its next offer). A canary failure never fails the action: the
/// next slot is taken, and with none free [`Offer::next`] waits, as it does when every
/// slot is held. The slot that passes is held by the offer.
///
/// # Errors
/// What [`Offer::next`] or [`Offer::probe`] returns (a kill, a hand-over that failed).
pub async fn first_passing<O: Offer + Send>(offer: &mut O) -> Result<(), O::Err> {
    loop {
        offer.next().await?;
        match offer.probe().await? {
            Probe::Pass => return Ok(()),
            Probe::Aside(why) => offer.aside(&why).await?,
        }
    }
}

/// [`first_passing`], then moves the slot the offer holds into `hold`, whether it passed
/// or a step failed: the caller owns the slot from the moment it was taken, so on every
/// error and kill it removes the lease directory first and releases the slot after.
///
/// # Errors
/// As [`first_passing`].
pub async fn acquire<O: Offer + Send>(
    offer: &mut O,
    hold: &mut Option<O::Slot>,
) -> Result<(), O::Err> {
    let result = first_passing(offer).await;
    *hold = offer.take_held();
    result
}

/// How `work` ended when it was raced against a kill.
#[derive(Debug, PartialEq, Eq)]
pub enum Raced<T, K> {
    /// It finished first, with this.
    Done(T),
    /// The kill came first; `work` has stopped (it saw `cancel` and returned).
    Killed(K),
}

/// Runs `work`, which does blocking work that cannot be cancelled from outside and
/// checks `cancel` between its steps, until it finishes or `killed` resolves. On a
/// kill it sets `cancel` and **waits for `work` to return**, so that nothing it does
/// (a canary launched as the slot's user, an owner change) happens after the caller
/// has been told the lease stopped and goes on to remove the directory and release the
/// slot.
pub async fn until_killed<T, K>(
    work: impl Future<Output = T>,
    cancel: &std::sync::atomic::AtomicBool,
    killed: impl Future<Output = Option<K>>,
) -> Raced<T, K> {
    tokio::pin!(work);
    tokio::pin!(killed);
    tokio::select! {
        done = &mut work => Raced::Done(done),
        Some(by) = &mut killed => {
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = work.await;
            Raced::Killed(by)
        }
    }
}

/// What a canary can leave in the lease directory: the link the link canary made. The
/// daemon removes it itself after the canaries, without following it, so that a link to
/// a file of the daemon's is never left in a tree that is handed to the slot's user.
pub fn remove_leftovers(fs: &dyn Fs, lease: &Path) {
    fs.remove(&lease.join(LINK_NAME));
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

    /// A log sink the tests read back.
    #[derive(Clone, Default)]
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl io::Write for Log {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Log {
        /// Runs `f` with this log as the subscriber (a `tracing` macro evaluates its
        /// fields only with one), and returns the text logged.
        fn during<T>(&self, f: impl FnOnce() -> T) -> (T, String) {
            let writer = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .finish();
            let out = tracing::subscriber::with_default(subscriber, f);
            io::Write::flush(&mut self.clone()).expect("flush");
            let text = String::from_utf8(self.0.lock().expect("log").clone()).expect("utf8");
            (out, text)
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
        assert_eq!(
            classify(errno(libc::ENOENT)),
            Seen::Failed(io::Error::from_raw_os_error(libc::ENOENT).to_string())
        );
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
        assert_eq!([linked.exists(), file.exists()], [false, false]);
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

    /// Catches a check that stops at the first failure (a second broken door is never
    /// seen, so a profile that breaks only the later canary cannot be shown red), that
    /// passes when one canary is allowed (the "not offered" branch) or cannot run (the
    /// "cannot run" branch), or that does not log the canary, its deny and the result.
    #[test]
    fn check_runs_every_canary_and_reports_every_failure() {
        let refused = |c: &Canary| Ok(ran(REFUSED, &format!("canary {} refused errno=1", c.name)));
        let (report, log) = Log::default().during(|| check(CANARIES, &mut |c| refused(c)));
        assert!(report.all_refused());
        assert_eq!(report.passed.len(), CANARIES.len());
        assert_eq!(report.passed[0].name, "write-outside");
        assert_eq!(report.passed[0].took, Duration::from_millis(80));
        assert!(log.contains("profile canary refused"), "{log}");
        assert!(log.contains("link-outside"), "{log}");

        // Only one canary is broken, in turn: each is seen alone, the other refused.
        for broken in CANARIES {
            let (report, log) = Log::default().during(|| {
                check(CANARIES, &mut |c| {
                    if c.name == broken.name {
                        Ok(ran(ALLOWED, &format!("canary {} allowed", c.name)))
                    } else {
                        refused(c)
                    }
                })
            });
            assert!(!report.all_refused());
            assert_eq!(report.failed.len(), 1, "{}", report.failures());
            assert_eq!(report.passed.len(), CANARIES.len() - 1);
            let failed = &report.failed[0];
            assert_eq!(
                (failed.name, &failed.verdict),
                (broken.name, &Verdict::Allowed)
            );
            assert_eq!(
                report.failures(),
                format!(
                    "canary {} ({}): NOT refused: the call succeeded",
                    broken.name, broken.deny
                )
            );
            for want in ["ERROR", "profile canary failed", broken.name, "NOT refused"] {
                assert!(log.contains(want), "{want} not in {log}");
            }
        }

        let mut seen = 0;
        let (report, log) = Log::default().during(|| {
            check(CANARIES, &mut |_| {
                seen += 1;
                Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"))
            })
        });
        assert_eq!(seen, CANARIES.len(), "every canary runs");
        assert_eq!(report.failed.len(), CANARIES.len());
        assert_eq!(
            report.failed[0].verdict,
            Verdict::CannotRun("timed out".to_owned())
        );
        assert!(log.contains("could not run: timed out"), "{log}");
        assert_eq!(check(&[], &mut |_| unreachable!()), Report::default());
    }

    /// A slot that records when it is released (dropped), unless it was set aside.
    struct TestSlot {
        id: u32,
        events: Events,
        asided: bool,
    }

    type Events = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    impl Drop for TestSlot {
        fn drop(&mut self) {
            if !self.asided {
                let event = format!("released {}", self.id);
                self.events.lock().expect("events").push(event);
            }
        }
    }

    /// Where the offer fails, for the slot taken first.
    #[derive(Clone, Copy, PartialEq)]
    enum Fail {
        Never,
        /// A kill while the lease is prepared, with the slot held.
        Prepare,
        /// The hand-over of the lease directory fails.
        HandOver,
        /// A canary could not be started.
        Canary,
        /// The lease directory cannot be taken back from a slot that failed its canaries.
        Reclaim,
    }

    /// Slots from a queue (taken from the back); the probe passes the slots in `good`.
    struct TestOffer {
        events: Events,
        queue: Vec<u32>,
        good: Vec<u32>,
        fail: Fail,
        current: Option<TestSlot>,
    }

    impl TestOffer {
        fn new(queue: &[u32], good: &[u32], fail: Fail) -> Self {
            Self {
                events: Events::default(),
                queue: queue.to_vec(),
                good: good.to_vec(),
                fail,
                current: None,
            }
        }

        fn push(&self, event: &str) {
            self.events.lock().expect("events").push(event.to_owned());
        }
    }

    impl Offer for TestOffer {
        type Slot = TestSlot;
        type Err = String;

        async fn next(&mut self) -> Result<(), String> {
            let Some(id) = self.queue.pop() else {
                return Err("killed while waiting".to_owned());
            };
            self.current = Some(TestSlot {
                id,
                events: Events::clone(&self.events),
                asided: false,
            });
            if self.fail == Fail::Prepare {
                return Err("killed during prepare".to_owned());
            }
            Ok(())
        }

        async fn probe(&mut self) -> Result<Probe, String> {
            let id = self.current.as_ref().expect("held").id;
            match self.fail {
                Fail::HandOver => return Err("hand over failed".to_owned()),
                Fail::Canary => return Err("canary could not start".to_owned()),
                Fail::Never | Fail::Prepare | Fail::Reclaim => {}
            }
            Ok(if self.good.contains(&id) {
                Probe::Pass
            } else {
                Probe::Aside(format!("slot {id} bad"))
            })
        }

        async fn aside(&mut self, why: &str) -> Result<(), String> {
            if self.fail == Fail::Reclaim {
                return Err("reclaim failed".to_owned());
            }
            self.push("reclaim the lease directory");
            let mut slot = self.current.take().expect("held");
            slot.asided = true;
            self.push(&format!("aside {}: {why}", slot.id));
            Ok(())
        }

        fn take_held(&mut self) -> Option<TestSlot> {
            self.current.take()
        }
    }

    /// The lease as `run` does it: acquire, then remove the lease directory, then release
    /// the slot. What happened, in order.
    fn lease(mut offer: TestOffer) -> (Result<(), String>, Vec<String>, bool) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let mut hold = None;
        let result = rt.block_on(acquire(&mut offer, &mut hold));
        offer.push("lease directory removed");
        let held = hold.is_some();
        drop(hold);
        let events = offer.events.lock().expect("events").clone();
        (result, events, held)
    }

    /// Catches a lease whose slot is freed before its directory is removed (another
    /// lease could then take the uid while the failed lease's half-written inputs, owned
    /// by that uid, are still there): on a kill during prepare, a failed hand-over and a
    /// canary that could not start, the slot stays with the caller, which removes the
    /// directory and only then releases it; and after a kill while waiting for the next
    /// slot, with one set aside, no slot is held and the directory is removed. Seen red
    /// with `acquire` not moving the held slot out (the slot is released inside it,
    /// before the directory event).
    #[test]
    fn on_every_failure_the_directory_goes_before_the_slot_is_released() {
        for fail in [Fail::Prepare, Fail::HandOver, Fail::Canary] {
            let (result, events, held) = lease(TestOffer::new(&[1], &[1], fail));
            assert!(result.is_err());
            assert!(held, "the caller holds the slot");
            assert_eq!(events, ["lease directory removed", "released 1"]);
        }
        // The directory cannot be taken back from a slot that failed its canaries: it is
        // not set aside, the lease fails with the slot held, and the directory goes first.
        let (result, events, held) = lease(TestOffer::new(&[1], &[], Fail::Reclaim));
        assert_eq!(result, Err("reclaim failed".to_owned()));
        assert!(held);
        assert_eq!(events, ["lease directory removed", "released 1"]);
        // Slot 1 fails its canaries and is set aside; the wait for the next ends in a kill.
        let (result, events, held) = lease(TestOffer::new(&[1], &[], Fail::Never));
        assert_eq!(result, Err("killed while waiting".to_owned()));
        assert!(!held);
        assert_eq!(
            events,
            [
                "reclaim the lease directory",
                "aside 1: slot 1 bad",
                "lease directory removed"
            ]
        );
    }

    /// Catches a lease that fails, or runs on a slot that failed its canaries, when a
    /// later slot would pass: the bad slots are set aside, each after the directory is
    /// taken back from it, and the lease runs on the good one, which the caller holds.
    #[test]
    fn a_failed_canary_sets_the_slot_aside_and_the_lease_takes_the_next() {
        let (result, events, held) = lease(TestOffer::new(&[3, 2, 1], &[3], Fail::Never));
        assert_eq!(result, Ok(()));
        assert!(held);
        assert_eq!(
            events,
            [
                "reclaim the lease directory",
                "aside 1: slot 1 bad",
                "reclaim the lease directory",
                "aside 2: slot 2 bad",
                "lease directory removed",
                "released 3"
            ]
        );
    }

    /// Catches a canary loop that starts another canary after it was told to stop (a
    /// canary launched as the slot's user after the slot was released): with the flag set
    /// after the first, the second is not run and the report holds the first.
    #[test]
    fn no_canary_starts_once_cancelled() {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut runs = 0;
        let report = check_until(
            CANARIES,
            &|| cancel.load(std::sync::atomic::Ordering::SeqCst),
            &mut |c| {
                runs += 1;
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(ran(REFUSED, &format!("canary {} refused errno=1", c.name)))
            },
        );
        assert_eq!(runs, 1);
        assert_eq!(report.passed.len(), 1);
        assert!(report.failed.is_empty());
    }

    /// Catches a kill that is answered while blocking work of the slot goes on (a canary
    /// launched as the slot's user, or an owner change, after the lease was reported
    /// killed and its slot released): the work records a step every 20 ms and stops at
    /// its next step once the flag is set; after `until_killed` returns, no step is
    /// recorded, and fewer than all of them ran. A run that finishes first is `Done`.
    /// Seen red with `until_killed` returning on the kill without awaiting the work.
    #[test]
    fn a_kill_waits_for_the_blocking_work_to_stop() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let steps = Arc::new(Mutex::new(0_u32));
        let cancel = Arc::new(AtomicBool::new(false));
        let (work_steps, work_cancel) = (Arc::clone(&steps), Arc::clone(&cancel));
        let work = async move {
            tokio::task::spawn_blocking(move || {
                while !work_cancel.load(Ordering::SeqCst) && *work_steps.lock().expect("steps") < 50
                {
                    std::thread::sleep(Duration::from_millis(20));
                    *work_steps.lock().expect("steps") += 1;
                }
            })
            .await
            .expect("work")
        };
        let killed = async {
            tokio::time::sleep(Duration::from_millis(70)).await;
            Some("kill")
        };
        let raced = rt.block_on(until_killed(work, &cancel, killed));
        assert_eq!(raced, Raced::Killed("kill"));
        let at_ack = *steps.lock().expect("steps");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            *steps.lock().expect("steps"),
            at_ack,
            "work after the kill was answered"
        );
        assert!(at_ack < 50, "the work was not stopped");

        let cancel = AtomicBool::new(false);
        let done = rt.block_on(until_killed(
            async { 7 },
            &cancel,
            std::future::pending::<Option<()>>(),
        ));
        assert_eq!(done, Raced::Done(7));
        assert!(!cancel.load(Ordering::SeqCst));
    }

    /// Catches a removal of the link canary's leftovers that follows the link, or misses
    /// it: a hard link to a file outside is removed from the lease directory and the
    /// file outside is untouched.
    #[test]
    fn the_daemon_removes_the_link_a_canary_left() {
        let scratch = crate::unit_dir("canary-leftovers");
        let outside = scratch.path().join("outside");
        std::fs::write(&outside, b"x").expect("outside");
        let lease = scratch.path().join("lease");
        std::fs::create_dir(&lease).expect("lease");
        let link = lease.join(LINK_NAME);
        std::fs::hard_link(&outside, &link).expect("link");
        remove_leftovers(&RealFs, &lease);
        assert!(!link.exists(), "the link stayed");
        assert!(outside.exists(), "the file outside was removed");
        remove_leftovers(&RealFs, &lease);
    }

    /// Catches a failed canary that does not make the slot dirty in the book (it would
    /// stay offered and counted), a lease that fails or runs on that slot instead of the
    /// next one, a recovery that offers the slot again without running its canaries at
    /// the next hand-out, and a slot that stays dirty after its canaries pass. Changes
    /// seen to turn it red: `Book::set_aside` giving the permit back instead of making
    /// the slot dirty (the dirty-set and count assertions); `first_passing` returning a
    /// slot whose probe said aside (the lease runs on slot 0); `first_passing` skipping
    /// the probe (slot 0 is handed out without its canaries).
    #[test]
    fn a_failed_canary_makes_the_slot_dirty_and_recovery_offers_it_with_its_canaries_run_again() {
        use std::collections::BTreeSet;
        use std::sync::{Arc, Mutex};

        use super::super::book::{Backoff, Book, Cleaner, Slotted};
        use super::super::clean::Outcome;

        struct Stub(u32);
        impl Slotted for Stub {
            fn uid(&self) -> u32 {
                700 + self.0
            }
        }

        type Held = (usize, tokio::sync::OwnedSemaphorePermit);

        /// The book's slots offered to one lease; the canaries of `failing` fail.
        struct BookOffer {
            book: Arc<Book<Stub>>,
            failing: Arc<Mutex<BTreeSet<usize>>>,
            probed: Vec<usize>,
            current: Option<Held>,
        }

        impl Offer for BookOffer {
            type Slot = Held;
            type Err = String;

            async fn next(&mut self) -> Result<(), String> {
                self.current = Some(self.book.take().await?);
                Ok(())
            }

            async fn probe(&mut self) -> Result<Probe, String> {
                let index = self.current.as_ref().expect("held").0;
                self.probed.push(index);
                let bad = self.failing.lock().expect("failing").contains(&index);
                Ok(if bad {
                    Probe::Aside(
                        "canary write-outside ((deny file-write*)): NOT refused".to_owned(),
                    )
                } else {
                    Probe::Pass
                })
            }

            async fn aside(&mut self, why: &str) -> Result<(), String> {
                let (index, permit) = self.current.take().expect("held");
                self.book.set_aside(index, permit, why);
                Ok(())
            }

            fn take_held(&mut self) -> Option<Held> {
                self.current.take()
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let cleaner: Cleaner<Stub> = Arc::new(|_: &Stub| Outcome::Clean);
        let never = Backoff {
            first: Duration::from_secs(3600),
            max: Duration::from_secs(3600),
        };
        let book = Book::start(vec![Stub(0), Stub(1)], cleaner, never);
        let failing = Arc::new(Mutex::new(BTreeSet::from([0_usize])));
        let mut offer = BookOffer {
            book: Arc::clone(&book),
            failing: Arc::clone(&failing),
            probed: Vec::new(),
            current: None,
        };
        let mut hold = None;

        // Slot 0 fails its canaries: dirty, uncounted; the lease runs on slot 1.
        rt.block_on(acquire(&mut offer, &mut hold)).expect("a slot");
        let (index, permit) = hold.take().expect("held");
        assert_eq!(index, 1, "the lease takes the next slot");
        assert_eq!(offer.probed, [0, 1]);
        assert_eq!(book.dirty(), BTreeSet::from([0]));
        assert_eq!(book.count(), 1, "a dirty slot is not counted");
        book.held(index, permit).give();

        // The recovery pass cleans slot 0 and frees it; its canaries run again at the next
        // hand-out and fail again: dirty again, the lease runs on slot 1.
        assert_eq!(book.retry(), 1);
        assert_eq!(book.count(), 2);
        offer.probed.clear();
        rt.block_on(acquire(&mut offer, &mut hold)).expect("a slot");
        let (index, permit) = hold.take().expect("held");
        assert_eq!((index, offer.probed.clone()), (1, vec![0, 1]));
        assert_eq!(book.dirty(), BTreeSet::from([0]));
        book.held(index, permit).give();

        // Its canaries pass now: recovered, probed once, handed out.
        failing.lock().expect("failing").clear();
        assert_eq!(book.retry(), 1);
        offer.probed.clear();
        rt.block_on(acquire(&mut offer, &mut hold)).expect("a slot");
        let (index, permit) = hold.take().expect("held");
        assert_eq!((index, offer.probed.clone()), (0, vec![0]));
        assert!(book.dirty().is_empty());
        book.held(index, permit).give();
    }
}
