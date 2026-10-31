//! One invocation: parse the request, check it against the allowlist, run the plan
//! through a [`Runner`], answer, and audit.
//!
//! Order matters. A request outside the grammar is refused before any file is read. An
//! `install` is checked against the allowlist and the source's signing before the audit
//! log is opened, and is not started at all if the log cannot be opened: no install goes
//! unrecorded. A `restart` is not started either when the log cannot be opened. A refusal
//! or a `query` is answered even when the log is unavailable (the warning goes to stderr),
//! since neither changes the host.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde_json::Value;

use crate::audit::{self, Audit};
use crate::config::Allowlist;
use crate::debver::DebVersion;
use crate::locks;
use crate::node_id;
use crate::outcome::{Failure, FailureCode, Outcome, QueryReport, Refusal};
use crate::plan;
use crate::request::{BadCommand, Channel, Request, Version};
use crate::runner::{Cmd, Exec, Runner};
use crate::trust::{self, Trust};

/// Where the packaging installs this program (the forced command of the key).
pub const SELF_EXE: &str = "/usr/libexec/kbf/kbf-host-install";

/// What the process was started with that decides how it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    /// `SSH_ORIGINAL_COMMAND`: the request.
    pub ssh_original_command: Option<OsString>,
    /// The variable only the install unit has, holding its request.
    pub unit_request: Option<OsString>,
    /// Whether sshd started this process (it sets any of four variables).
    pub ssh_session: bool,
}

impl Environment {
    /// Read it from this process's environment.
    pub fn from_process() -> Self {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// Read it through `get`, which answers for a variable by name.
    pub fn from_vars(get: impl Fn(&str) -> Option<OsString>) -> Self {
        let ssh_session = SSH_VARS.iter().any(|name| get(name).is_some());
        Self {
            ssh_original_command: get("SSH_ORIGINAL_COMMAND"),
            unit_request: get(plan::UNIT_ENV),
            ssh_session,
        }
    }
}

/// The variables sshd sets for a session it runs a command in (and for a login). Any one
/// means the process came in over ssh.
const SSH_VARS: [&str; 4] = [
    "SSH_ORIGINAL_COMMAND",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
];

/// Run as what the environment says. The body of the install unit is for the process
/// `systemd-run` starts: the unit variable alone selects it, and any sign of an ssh session
/// takes that away, since a client may be able to set a variable (the host's `AcceptEnv`) and
/// must not reach the install without the front's checks and its audit line. Every other
/// process is an ssh request handler.
pub fn dispatch(
    host: &Host,
    runner: &dyn Runner,
    env: &Environment,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    match &env.unit_request {
        Some(request) if !env.ssh_session => handle_unit(host, runner, Some(request), out, err),
        _ => handle(host, runner, env.ssh_original_command.as_deref(), out, err),
    }
}

/// The body of the install unit: `request` (the unit variable) must be an install; it is
/// checked against the allowlist again and run under the install lock, which this process
/// holds from before it reads the package database until it exits. The answer is the same
/// line the front would give; there is no audit line (the front's is the record).
pub fn handle_unit(
    host: &Host,
    runner: &dyn Runner,
    request: Option<&OsStr>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let outcome = match request
        .and_then(OsStr::to_str)
        .map_or(Err(BadCommand), Request::parse)
    {
        Ok(Request::Install { version, channel }) => {
            install_body(host, runner, &version, &channel).unwrap_or_else(|outcome| outcome)
        }
        _ => Outcome::Refused(Refusal::BadCommand),
    };
    respond(&outcome, out, err)
}

/// Write the answer: the detail of a failure to `err`, the line to `out`; the exit status.
/// The writes ignore a closed pipe: the client is gone and there is nobody to tell.
pub(crate) fn respond(outcome: &Outcome, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    if let Some(detail) = outcome.stderr_detail() {
        let _ = writeln!(err, "{detail}");
    }
    let _ = writeln!(out, "{}", outcome.json_line());
    outcome.exit_code()
}

/// Where the files this program reads as root are, and who must own them.
#[derive(Debug, Clone)]
pub struct Host {
    pub(crate) config: PathBuf,
    pub(crate) audit: PathBuf,
    pub(crate) install_lock: PathBuf,
    pub(crate) proc_locks: PathBuf,
    pub(crate) dpkg_lock: PathBuf,
    /// The node's configuration, where its id is.
    pub(crate) node_config: PathBuf,
    /// The kernel's host name, the node id when the configuration names none.
    pub(crate) hostname: PathBuf,
    /// This program, as the install unit runs it again.
    pub(crate) self_exe: PathBuf,
    /// How long a restart waits for its unit to be active.
    pub(crate) restart_wait: plan::Wait,
    /// How that wait tells time and pauses: the real clock here, a made-up one in tests.
    pub(crate) pace: std::rc::Rc<crate::pace::Pace>,
    pub(crate) trust: Trust,
}

impl Host {
    /// The production paths, trusting root. The only constructor outside tests.
    pub fn production() -> Self {
        Self {
            config: PathBuf::from("/etc/kbf/host-install.toml"),
            audit: PathBuf::from("/var/log/kbf-host-install.log"),
            install_lock: PathBuf::from("/run/kbf-host-install.lock"),
            proc_locks: PathBuf::from("/proc/locks"),
            dpkg_lock: PathBuf::from("/var/lib/dpkg/lock-frontend"),
            node_config: PathBuf::from("/etc/kbf/kbf.toml"),
            hostname: PathBuf::from("/proc/sys/kernel/hostname"),
            self_exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from(SELF_EXE)),
            restart_wait: plan::ACTIVE_WAIT,
            pace: std::rc::Rc::new(crate::pace::Pace::real()),
            trust: Trust::root(),
        }
    }

    /// Paths under `dir`, trusting `trust`, for tests.
    #[cfg(test)]
    pub fn for_test(dir: &std::path::Path, trust: Trust) -> Self {
        Self {
            config: dir.join("host-install.toml"),
            audit: dir.join("audit.log"),
            install_lock: dir.join("install.lock"),
            proc_locks: dir.join("proc-locks"),
            dpkg_lock: dir.join("lock-frontend"),
            node_config: dir.join("kbf.toml"),
            hostname: dir.join("hostname"),
            self_exe: PathBuf::from(SELF_EXE),
            restart_wait: plan::Wait {
                timeout: std::time::Duration::from_secs(30),
                interval: std::time::Duration::from_millis(1),
            },
            pace: std::rc::Rc::new(crate::pace::fake::Fake::new().pace()),
            trust,
        }
    }
}

/// Handle `command` (the `SSH_ORIGINAL_COMMAND`, none when unset): write the answer line
/// to `out`, detail to `err`, and return the exit status.
pub fn handle(
    host: &Host,
    runner: &dyn Runner,
    command: Option<&OsStr>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let request = command
        .and_then(OsStr::to_str)
        .map_or(Err(BadCommand), Request::parse);
    let mut log = Audit::open(&host.trust, &host.audit);
    let outcome = match &request {
        Err(BadCommand) => Outcome::Refused(Refusal::BadCommand),
        Ok(Request::Query) => query(host, runner),
        Ok(Request::Install { version, channel }) => {
            install(host, runner, version, channel, log.is_ok())
        }
        Ok(Request::Restart { unit }) => restart::run(host, runner, unit, log.is_ok()),
    };

    // The audit line first, then the answer. A failed write is told on stderr; the
    // outcome stands. The writes to `out` and `err` below ignore a closed pipe: the
    // client is gone and there is nobody to tell.
    let (outcome_word, reason) = outcome.audit_fields();
    let (version, channel) = match &request {
        Ok(Request::Install { version, channel }) => (
            Some(version.as_str().to_owned()),
            Some(channel.as_str().to_owned()),
        ),
        _ => (None, None),
    };
    let unit = match &request {
        Ok(Request::Restart { unit }) => Some(unit.as_str().to_owned()),
        _ => None,
    };
    let entry = audit::Entry {
        time: audit::now(),
        verb: request.as_ref().ok().map(Request::verb),
        service: None,
        version,
        channel,
        unit,
        outcome: outcome_word,
        reason,
        result: None,
        command: request
            .is_err()
            .then(|| command.map(|c| audit::truncate_raw(&c.to_string_lossy())))
            .flatten(),
    };
    let logged = log
        .as_mut()
        .map_err(|e| e.to_string())
        .and_then(|log| log.append(&entry).map_err(|e| e.to_string()));
    if let Err(why) = logged {
        let _ = writeln!(err, "kbf-host-install: audit log unavailable: {why}");
    }
    respond(&outcome, out, err)
}

pub(crate) fn fail(code: FailureCode, detail: impl Into<String>) -> Outcome {
    Outcome::Failed(Failure {
        code,
        detail: detail.into(),
    })
}

/// What `dpkg` says about `kbf`.
enum Installed {
    /// Not installed (never, or removed, or only its configuration files left).
    Absent,
    /// Installed at `text`/`parsed`. `clean` is false when dpkg flags an error on the
    /// package (the third character of its status, `R` for reinstall required): the files
    /// are not known to be whole.
    Present {
        text: String,
        parsed: DebVersion,
        clean: bool,
    },
}

/// `query`: what is installed, what runs, and whether an install is in progress.
///
/// An install is in progress when ANY of three things holds: the install lock is held (by
/// the process that does the work, for the whole install, so there is no moment between the
/// steps at which none of these is true), an install unit exists that is not over (listed
/// `active` or `activating`, not only the former), or some process holds the dpkg frontend
/// lock. A package in an error state has no installed version here: it is not cleanly
/// installed, and the updater's install repairs it.
fn query(host: &Host, runner: &dyn Runner) -> Outcome {
    let installed = match read_installed(runner) {
        Ok(Installed::Present {
            text, clean: true, ..
        }) => Some(text),
        Ok(_) => None,
        Err(outcome) => return outcome,
    };
    let running_commit = installed.as_ref().and_then(|_| running_commit(runner));
    let listing = match runner.run(&plan::units_running()) {
        Ok(exec) if exec.success() => exec.stdout_text(),
        Ok(exec) => {
            return fail(
                FailureCode::QueryFailed,
                format!(
                    "systemctl: exit status {:?}: {}",
                    exec.code,
                    exec.stderr_text()
                ),
            );
        }
        Err(e) => return fail(FailureCode::QueryFailed, format!("systemctl: {e}")),
    };
    let unit_listed = !listing.trim().is_empty();
    Outcome::Queried(QueryReport {
        installed_version: installed,
        running_commit,
        node_id: node_id::read(&host.trust, &host.node_config, &host.hostname),
        in_progress: unit_listed
            || locks::flock_held(&host.install_lock)
            || locks::held(&host.proc_locks, &host.dpkg_lock),
    })
}

/// The commit `kbf --version --json` reports (the full one when it has it), if any. A
/// binary that fails, hangs (the runner kills it at the plan's limit) or prints something
/// else has none.
fn running_commit(runner: &dyn Runner) -> Option<String> {
    let exec = runner
        .run(&plan::running_build())
        .ok()
        .filter(crate::runner::Exec::success)?;
    let build: Value = serde_json::from_str(&exec.stdout_text()).ok()?;
    ["commit_full", "commit"]
        .iter()
        .filter_map(|key| build.get(key)?.as_str())
        .find(|commit| {
            (1..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .map(str::to_owned)
}

/// What dpkg says about `kbf`, read and parsed.
fn read_installed(runner: &dyn Runner) -> Result<Installed, Outcome> {
    let exec = runner
        .run(&plan::installed_version())
        .map_err(|e| fail(FailureCode::QueryFailed, format!("dpkg-query: {e}")))?;
    if exec.code == Some(1) {
        // dpkg-query's status for a package it has never heard of.
        return Ok(Installed::Absent);
    }
    if !exec.success() {
        return Err(fail(
            FailureCode::QueryFailed,
            format!("dpkg-query: {}", exec.stderr_text()),
        ));
    }
    let text = exec.stdout_text();
    let mut words = text.split_whitespace();
    let unreadable = || {
        fail(
            FailureCode::QueryFailed,
            "dpkg-query printed something unexpected",
        )
    };
    // The status abbreviation: wanted, then current state (`i` installed, `n` not,
    // `c` config files left), then an error flag (a space, or `R` for reinstall required).
    let status = words.next().ok_or_else(unreadable)?;
    match status.chars().nth(1) {
        Some('i') => {}
        Some('n' | 'c') => return Ok(Installed::Absent),
        Some(other) => {
            return Err(fail(
                FailureCode::PackageStateUnclean,
                format!("kbf is in package state {other:?}"),
            ));
        }
        None => return Err(unreadable()),
    }
    let clean = status.chars().nth(2).is_none();
    let version = words.next().ok_or_else(unreadable)?;
    let parsed = DebVersion::parse(version)
        .map_err(|e| fail(FailureCode::InstalledVersionInvalid, e.to_string()))?;
    Ok(Installed::Present {
        text: version.to_owned(),
        parsed,
        clean,
    })
}

/// `install`, as the process sshd starts it: checked against the allowlist and the source's
/// signing, then handed to ONE transient unit, whose answer is relayed. This process takes
/// no lock: it ends with the ssh connection, and the work must not be unlocked when it does.
fn install(
    host: &Host,
    runner: &dyn Runner,
    version: &Version,
    channel: &Channel,
    audit_ok: bool,
) -> Outcome {
    install_front(host, runner, version, channel, audit_ok).unwrap_or_else(|outcome| outcome)
}

fn install_front(
    host: &Host,
    runner: &dyn Runner,
    version: &Version,
    channel: &Channel,
    audit_ok: bool,
) -> Result<Outcome, Outcome> {
    let allowlist = Allowlist::load(&host.trust, &host.config).map_err(Outcome::Refused)?;
    allowlist
        .channel(&host.trust, channel)
        .map_err(Outcome::Refused)?;
    if !audit_ok {
        return Err(fail(
            FailureCode::AuditUnavailable,
            "the audit log cannot be opened; nothing was run",
        ));
    }
    // An install is running (or the lock cannot be asked about): say so, rather than start
    // a unit that would find the lock held, or that systemd refuses by name.
    if locks::flock_held(&host.install_lock) {
        return Err(fail(FailureCode::Busy, BUSY_DETAIL));
    }
    let started = runner.run(&plan::unit_install(&host.self_exe, version, channel));
    Ok(relay(started, version))
}

/// The outcome of the unit as it ended: the last non-blank line of its stdout is its
/// answer, and a unit that gave none (it could not start, was killed, or printed something
/// else) is an install failure whatever its exit status. A success is believed only for the
/// version asked for.
fn relay(started: io::Result<Exec>, version: &Version) -> Outcome {
    let exec = match started {
        Ok(exec) => exec,
        Err(e) => {
            return fail(
                FailureCode::InstallFailed,
                format!("{}: {e}", plan::SYSTEMD_RUN),
            );
        }
    };
    let stdout = exec.stdout_text();
    let answer = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .and_then(Outcome::from_unit_line);
    let outcome = match answer {
        None => {
            return fail(
                FailureCode::InstallFailed,
                format!(
                    "the install unit gave no answer (exit status {:?}): {}",
                    exec.code,
                    detail_of(&exec)
                ),
            );
        }
        // The line carries the code; the detail is what the unit wrote to stderr.
        Some(Outcome::Failed(failure)) => {
            return Outcome::Failed(Failure {
                detail: exec.stderr_text(),
                ..failure
            });
        }
        Some(outcome) => outcome,
    };
    let (Outcome::Installed { version: claimed } | Outcome::AlreadyInstalled { version: claimed }) =
        &outcome
    else {
        return outcome;
    };
    if DebVersion::parse(claimed).is_ok_and(|parsed| parsed == *version.parsed()) {
        outcome
    } else {
        fail(
            FailureCode::VerifyFailed,
            "the unit reports a version other than the one asked for",
        )
    }
}

/// The body of the install unit: allowlist again, the lock, the same-version check, the two
/// apt steps, and the package database read back. An older version installs like a newer
/// one: a rollback is an install. The lock is held by this process from
/// before the first read until the function returns, which is until the process exits.
fn install_body(
    host: &Host,
    runner: &dyn Runner,
    version: &Version,
    channel: &Channel,
) -> Result<Outcome, Outcome> {
    let allowlist = Allowlist::load(&host.trust, &host.config).map_err(Outcome::Refused)?;
    let source = allowlist
        .channel(&host.trust, channel)
        .map_err(Outcome::Refused)?;
    // One install at a time: the same-version check and the install are one critical
    // section. The lock is dropped (and so released) when this function returns.
    let _lock = take_install_lock(host)?;
    if let Installed::Present {
        text,
        parsed,
        clean,
    } = read_installed(runner)?
    {
        // The same version is done, unless dpkg flags the package: then apt installs it
        // again. Any other version, older or newer, is installed.
        if clean && *version.parsed() == parsed {
            return Ok(Outcome::AlreadyInstalled { version: text });
        }
    }
    step(
        runner,
        &plan::apt_update(&source),
        FailureCode::UpdateFailed,
    )?;
    step(
        runner,
        &plan::apt_install(&source, version),
        FailureCode::InstallFailed,
    )?;
    // Trust nothing the steps said: read the package database again.
    match read_installed(runner) {
        Ok(Installed::Present {
            text,
            parsed,
            clean: true,
        }) if parsed == *version.parsed() => Ok(Outcome::Installed { version: text }),
        _ => Err(fail(
            FailureCode::VerifyFailed,
            "the installed version is not the one asked for",
        )),
    }
}

/// What a `busy` answer says on stderr: an install or a restart holds the install lock.
const BUSY_DETAIL: &str = "another install or restart is running";

/// Take the install lock without waiting: an advisory `flock` on a root-owned file under
/// `/run`, opened without following a symbolic link. Held by the returned file.
fn take_install_lock(host: &Host) -> Result<Lock, Outcome> {
    take_lock(&host.install_lock)
}

/// A held advisory lock. Dropping it lets the lock go with an explicit unlock, not with the
/// close of the descriptor: a `flock` belongs to the open file description, which a process
/// forked meanwhile shares until it execs, so a close alone can leave the lock held (and the
/// next request refused as busy) for as long as such a child lives.
#[derive(Debug)]
pub(crate) struct Lock(File);

impl Lock {
    /// The locked file, for a test that copies the descriptor as a fork does.
    #[cfg(test)]
    pub(crate) fn file(&self) -> &File {
        &self.0
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Nothing to report: the lock goes with the descriptor at the latest.
        let _ = self.0.unlock();
    }
}

/// Take the advisory lock on the file at `path` without waiting, creating it root-only.
pub(crate) fn take_lock(path: &std::path::Path) -> Result<Lock, Outcome> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(trust::nofollow())
        .open(path)
        .map_err(|e| {
            fail(
                FailureCode::LockUnavailable,
                format!("{}: {e}", path.display()),
            )
        })?;
    // Anything but a lock taken means another install or restart runs (or the lock cannot be
    // held): either way nothing may start.
    file.try_lock()
        .map_err(|_| fail(FailureCode::Busy, BUSY_DETAIL))?;
    Ok(Lock(file))
}

/// What a command printed to explain itself: its stderr, else its stdout.
fn detail_of(exec: &Exec) -> String {
    if exec.stderr.is_empty() {
        exec.stdout_text()
    } else {
        exec.stderr_text()
    }
}

/// Run one step of the install; a failure to start it or a status but 0 is `code`.
fn step(runner: &dyn Runner, cmd: &Cmd, code: FailureCode) -> Result<(), Outcome> {
    match runner.run(cmd) {
        Ok(exec) if exec.success() => Ok(()),
        Ok(exec) => Err(fail(
            code,
            format!("exit status {:?}: {}", exec.code, detail_of(&exec)),
        )),
        Err(e) => Err(fail(code, format!("{}: {e}", cmd.program))),
    }
}

mod restart;
#[cfg(test)]
mod restart_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod unit_tests;
