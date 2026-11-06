//! `kbf-host-install activate`: the emergency command, root's, on the host itself.
//!
//! ```text
//! kbf-host-install activate kbf=<commit|version> [--pin] [--force-lock] [--restart-unit UNIT]...
//! ```
//!
//! It makes a release the host keeps the one that runs, for good, without a fetch, a trial or
//! the agent ([`super::core::activate`]). It is run by root (ssh as root, or `sudo`) from this
//! program's fixed path, which no kbf release moves, so the swap runs nothing of the release it
//! leaves. On Linux `--restart-unit` names the units that run the release, as `--serve` takes
//! them (at least one); a Mac restarts the daemon's job and takes none. `--pin` writes the hold
//! file in the agent's state directory ([`LINUX_HOLD`], [`MAC_HOLD`]) and pins the release;
//! `--force-lock` takes the install lock from a holder that is gone or stuck.
//!
//! The answer is the socket's JSON line on stdout (`activated`, or a refusal that lists the kept
//! releases); the detail of a refusal or a failure, and the kept releases, go to stderr. The exit
//! status is 0 (done), 2 (refused: nothing changed) or 3 (failed). The command is audited like
//! every other (verb `activate`), and is not run when the audit log cannot be opened.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kbf_retain::CommitId;

use super::answer::{Answer, Code};
use super::backend::Backend;
use super::core::Ab;
use super::core::activate::How;
use super::request::Target;
use crate::audit::{self, Audit};
use crate::mac::layout::MacHost;
use crate::request::{BadCommand, Unit};
use crate::runner::Runner;
use crate::trust::Trust;

/// The hold file on Linux: in the packaged agent's state directory.
// Read by the Linux build only; the Mac build compiles the Linux side and has no use for it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const LINUX_HOLD: &str = "/var/lib/kbf-updater/pin.json";

/// The hold file on a Mac: in the packaged agent's state directory.
// Read by the Mac build only; the Linux build compiles the Mac side and has no use for it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MAC_HOLD: &str = "/var/db/kbf-updater/pin.json";

pub const USAGE: &str = "usage: kbf-host-install activate kbf=<commit|version> [--pin] \
[--force-lock] [--restart-unit UNIT]...\n";

/// What the command line asks for.
#[derive(Debug, Clone)]
pub struct Command {
    pub target: Target,
    pub how: How,
    /// The units that run the release (Linux).
    pub units: Vec<Unit>,
}

impl Command {
    /// Read the arguments after `activate`: `kbf=<commit|version>` once, the flags in any order,
    /// each but `--restart-unit` at most once.
    ///
    /// # Errors
    /// What is wrong with them, in a line.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut target = None;
        let mut how = How::default();
        let mut units = Vec::new();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            let flag = |slot: &mut bool| {
                if std::mem::replace(slot, true) {
                    Err(format!("{arg} is given twice"))
                } else {
                    Ok(())
                }
            };
            match arg.as_str() {
                "--pin" => flag(&mut how.pin)?,
                "--force-lock" => flag(&mut how.force_lock)?,
                "--restart-unit" => {
                    let value = rest.next().ok_or("--restart-unit needs a value")?;
                    units.push(Unit::service(value).map_err(|BadCommand| {
                        format!("--restart-unit {value:?} is not a kbf unit")
                    })?);
                }
                _ => {
                    let text = arg
                        .strip_prefix("kbf=")
                        .ok_or_else(|| format!("unknown argument {arg:?}"))?;
                    let parsed = Target::parse(text).map_err(|BadCommand| {
                        format!("{text:?} is neither a commit (12 to 40 hex digits) nor a version")
                    })?;
                    if target.replace(parsed).is_some() {
                        return Err("kbf= is given twice".to_owned());
                    }
                }
            }
        }
        let target = target.ok_or("kbf=<commit|version> is required")?;
        Ok(Self { target, how, units })
    }
}

/// The program: `activate` with the arguments after it, as root, on this platform.
pub fn main(args: &[String]) -> ExitCode {
    main_with(args, rustix::process::geteuid().as_raw(), &production)
}

/// [`main`] with the effective user and the platform's run put in by the caller.
pub(super) fn main_with(args: &[String], euid: u32, run: &dyn Fn(&Command) -> u8) -> ExitCode {
    let command = match Command::parse(args) {
        Ok(command) => command,
        Err(why) => {
            eprintln!("kbf-host-install: {why}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if euid != 0 {
        eprintln!("kbf-host-install: activate is root's: run it as root (sudo)");
        return ExitCode::from(2);
    }
    ExitCode::from(run(&command))
}

/// Where the audit log is, and who is trusted to own it.
pub(super) struct Place<'a> {
    pub(super) trust: &'a Trust,
    pub(super) audit: &'a Path,
}

/// Answer `command` with `activate` (an [`Ab::activate`]), audit it, and print the answer: the
/// exit status. One code path whatever the backend.
pub(super) fn run(
    activate: &dyn Fn(&Target, How) -> Answer,
    command: &Command,
    place: &Place<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let target = &command.target;
    let act = || activate(target, command.how);
    let asked = Asked {
        verb: "activate",
        version: Some(target.as_str().to_owned()),
    };
    answer(&asked, &act, place, out, err)
}

/// The verb of a root command and the release it names, for the audit line.
pub(super) struct Asked {
    pub(super) verb: &'static str,
    pub(super) version: Option<String>,
}

/// Answer with `act` once the audit log is open, audit it as `asked`, and print the answer: the
/// exit status (0 done, 2 refused, 3 failed).
pub(super) fn answer(
    asked: &Asked,
    act: &dyn Fn() -> Answer,
    place: &Place<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let verb = asked.verb;
    let log = Audit::open(place.trust, place.audit);
    let answer = match &log {
        Err(e) => Answer::Failed(
            Code::AuditUnavailable,
            format!(
                "the audit log {} cannot be opened ({e}); nothing was run",
                place.audit.display()
            ),
        ),
        Ok(_) => act(),
    };
    let (outcome, reason) = answer.audit_fields();
    let entry = audit::Entry {
        time: audit::now(),
        verb: Some(verb),
        version: asked.version.clone(),
        channel: None,
        unit: None,
        outcome,
        reason,
        result: result_of(&answer),
        command: None,
    };
    let logged = log
        .map_err(|e| e.to_string())
        .and_then(|mut log| log.append(&entry).map_err(|e| e.to_string()));
    if let Err(why) = logged {
        let _ = writeln!(err, "kbf-host-install: audit log unavailable: {why}");
    }
    let _ = writeln!(out, "{}", answer.json_line());
    if let Some(detail) = answer.log_detail() {
        let _ = writeln!(err, "kbf-host-install: {verb}: {detail}");
    }
    if let Answer::Unresolved { releases, .. } = &answer {
        for k in releases {
            let why = if k.why.is_empty() {
                String::new()
            } else {
                format!(" ({})", k.why.join(", "))
            };
            let _ = writeln!(
                err,
                "  kept: {} {}{why}",
                k.commit,
                k.version.as_deref().unwrap_or("-")
            );
        }
    }
    match answer {
        Answer::Activated { .. } | Answer::Unpinned { .. } => 0,
        Answer::Failed(..) => 3,
        _ => 2,
    }
}

/// What the audit line says an activate did: the release, the one left, the pin and the lock;
/// and what an unpin did: the commit unpinned, or that nothing was.
fn result_of(answer: &Answer) -> Option<String> {
    if let Answer::Unpinned { commit } = answer {
        return Some(commit.as_ref().map_or_else(
            || "nothing unpinned".to_owned(),
            |c| format!("unpinned {c}"),
        ));
    }
    let Answer::Activated {
        commit,
        from,
        pinned,
        lock_taken,
        ..
    } = answer
    else {
        return None;
    };
    let mut line = format!(
        "{commit} from {}",
        from.as_ref().map_or("none", CommitId::as_str)
    );
    if *pinned {
        line.push_str("; pinned");
    }
    if *lock_taken {
        line.push_str("; install lock taken");
    }
    Some(line)
}

/// A Linux host: `command` on the `kbf-installer` layout that `backend` makes (after the
/// command is checked: nothing of the host is read before), the state in `state_dir` (made
/// private if it is not there), the hold file at `hold`.
// Called by the Linux build only; the Mac build compiles it and its tests run it everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn on_linux<B: Backend>(
    command: &Command,
    backend: impl FnOnce() -> B,
    env: &super::production::LinuxEnv,
    trust: &Trust,
    hold: PathBuf,
) -> u8 {
    if command.units.is_empty() {
        eprintln!("kbf-host-install: --restart-unit is required on a Linux host\n{USAGE}");
        return 2;
    }
    if let Err(why) = super::production::private_dir(trust, &env.state_dir) {
        eprintln!("kbf-host-install: {why}");
        return 3;
    }
    let ab = new_ab(backend(), env.state_dir.join(&env.state_name), trust, hold);
    let place = Place {
        trust,
        audit: &env.audit,
    };
    run(
        &|target, how| ab.activate(target, how),
        command,
        &place,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

/// A Mac: `command` on the kit of `host`, whose tools `runner` runs, the state in its work
/// directory, the hold file at `hold`.
// Called by the Mac build only; the Linux build compiles it and its tests run it everywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn on_mac(
    command: &Command,
    host: MacHost,
    runner: Box<dyn Runner>,
    hold: PathBuf,
) -> u8 {
    if !command.units.is_empty() {
        eprintln!("kbf-host-install: a Mac restarts the daemon's job and takes no --restart-unit");
        return 2;
    }
    if crate::mac::ensure_work(&host).is_err() {
        eprintln!("kbf-host-install: the work directory is not usable");
        return 3;
    }
    let (trust, state, audit) = (
        host.trust.clone(),
        host.work.join("state.json"),
        host.audit.clone(),
    );
    let backend = super::mac_backend::MacBackend::new(host, runner);
    let ab = new_ab(backend, state, &trust, hold);
    let place = Place {
        trust: &trust,
        audit: &audit,
    };
    run(
        &|target, how| ab.activate(target, how),
        command,
        &place,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

pub(super) fn new_ab<B: Backend>(
    backend: B,
    state_file: PathBuf,
    trust: &Trust,
    hold: PathBuf,
) -> Ab<B> {
    Ab::new(
        backend,
        state_file,
        trust.clone(),
        super::serve::DEFAULT_WINDOW_SECS,
        Box::new(super::production::unix_now),
    )
    .keeping(kbf_retain::DEFAULT_KEEP, Box::new(super::production::log))
    .holding(hold)
}

#[cfg(target_os = "linux")]
pub(super) fn production(command: &Command) -> u8 {
    on_linux(
        command,
        || super::linux::production(command.units.clone()),
        &super::production::LinuxEnv::production(),
        &Trust::root(),
        PathBuf::from(LINUX_HOLD),
    )
}

#[cfg(target_os = "macos")]
fn production(command: &Command) -> u8 {
    on_mac(
        command,
        MacHost::production(),
        Box::new(crate::runner::SystemRunner),
        PathBuf::from(MAC_HOLD),
    )
}
