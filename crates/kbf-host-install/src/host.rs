//! One invocation: parse the request, check it against the allowlist, run the plan
//! through a [`Runner`], answer, and audit.
//!
//! Order matters. A request outside the grammar is refused before any file is read. An
//! `install` is checked against the allowlist and the source's signing before the audit
//! log is opened, and is not started at all if the log cannot be opened: no install goes
//! unrecorded. A refusal or a `query` is answered even when the log is unavailable (the
//! warning goes to stderr), since neither changes the host.

use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;

use serde_json::Value;

use crate::audit::{self, Audit};
use crate::config::Allowlist;
use crate::debver::DebVersion;
use crate::locks;
use crate::outcome::{Failure, FailureCode, Outcome, QueryReport, Refusal};
use crate::plan;
use crate::request::{BadCommand, Channel, Request, Version};
use crate::runner::{Cmd, Runner};
use crate::trust::Trust;

/// Where the files this program reads as root are, and who must own them.
#[derive(Debug, Clone)]
pub struct Host {
    pub(crate) config: PathBuf,
    pub(crate) audit: PathBuf,
    pub(crate) proc_locks: PathBuf,
    pub(crate) dpkg_lock: PathBuf,
    pub(crate) trust: Trust,
}

impl Host {
    /// The production paths, trusting root. The only constructor outside tests.
    pub fn production() -> Self {
        Self {
            config: PathBuf::from("/etc/kbf/host-install.toml"),
            audit: PathBuf::from("/var/log/kbf-host-install.log"),
            proc_locks: PathBuf::from("/proc/locks"),
            dpkg_lock: PathBuf::from("/var/lib/dpkg/lock-frontend"),
            trust: Trust::root(),
        }
    }

    /// Paths under `dir`, trusting `trust`, for tests.
    #[cfg(test)]
    pub fn for_test(dir: &std::path::Path, trust: Trust) -> Self {
        Self {
            config: dir.join("host-install.toml"),
            audit: dir.join("audit.log"),
            proc_locks: dir.join("proc-locks"),
            dpkg_lock: dir.join("lock-frontend"),
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
    let entry = audit::Entry {
        time: audit::now(),
        verb: request.as_ref().ok().map(Request::verb),
        version,
        channel,
        outcome: outcome_word,
        reason,
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
    if let Some(detail) = outcome.stderr_detail() {
        let _ = writeln!(err, "{detail}");
    }
    let _ = writeln!(out, "{}", outcome.json_line());
    outcome.exit_code()
}

fn fail(code: FailureCode, detail: impl Into<String>) -> Outcome {
    Outcome::Failed(Failure {
        code,
        detail: detail.into(),
    })
}

/// `query`: what is installed, what runs, and whether an install is in progress.
fn query(host: &Host, runner: &dyn Runner) -> Outcome {
    let installed = match read_installed(runner) {
        Ok(installed) => installed,
        Err(outcome) => return outcome,
    };
    let running_commit = installed.as_ref().and_then(|_| running_commit(runner));
    let unit_active = match runner.run(&plan::units_active()) {
        Ok(exec) => exec.success(),
        Err(e) => return fail(FailureCode::QueryFailed, format!("systemctl: {e}")),
    };
    Outcome::Queried(QueryReport {
        installed_version: installed.map(|(text, _)| text),
        running_commit,
        in_progress: unit_active || locks::held(&host.proc_locks, &host.dpkg_lock),
    })
}

/// The commit `kbf --version --json` reports (the full one when it has it), if any.
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

/// The installed version of `kbf` as dpkg printed it and parsed, none when it is not
/// installed.
fn read_installed(runner: &dyn Runner) -> Result<Option<(String, DebVersion)>, Outcome> {
    let exec = runner
        .run(&plan::installed_version())
        .map_err(|e| fail(FailureCode::QueryFailed, format!("dpkg-query: {e}")))?;
    if exec.code == Some(1) {
        // dpkg-query's status for a package it has never heard of.
        return Ok(None);
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
    // `c` config files left), then an error flag.
    let status = words.next().ok_or_else(unreadable)?;
    match status.chars().nth(1) {
        Some('i') => {}
        Some('n' | 'c') => return Ok(None),
        Some(other) => {
            return Err(fail(
                FailureCode::PackageStateUnclean,
                format!("kbf is in package state {other:?}"),
            ));
        }
        None => return Err(unreadable()),
    }
    let version = words.next().ok_or_else(unreadable)?;
    let parsed = DebVersion::parse(version)
        .map_err(|e| fail(FailureCode::InstalledVersionInvalid, e.to_string()))?;
    Ok(Some((version.to_owned(), parsed)))
}

/// `install`: checked, compared with what is installed, run in two units, verified.
fn install(
    host: &Host,
    runner: &dyn Runner,
    version: &Version,
    channel: &Channel,
    audit_ok: bool,
) -> Outcome {
    install_checked(host, runner, version, channel, audit_ok).unwrap_or_else(|outcome| outcome)
}

fn install_checked(
    host: &Host,
    runner: &dyn Runner,
    version: &Version,
    channel: &Channel,
    audit_ok: bool,
) -> Result<Outcome, Outcome> {
    let allowlist = Allowlist::load(&host.trust, &host.config).map_err(Outcome::Refused)?;
    let source = allowlist
        .channel(&host.trust, channel)
        .map_err(Outcome::Refused)?;
    if !audit_ok {
        return Err(fail(
            FailureCode::AuditUnavailable,
            "the audit log cannot be opened; nothing was run",
        ));
    }
    if let Some((text, installed)) = read_installed(runner)? {
        match version.parsed().cmp(&installed) {
            std::cmp::Ordering::Less => return Err(Outcome::Refused(Refusal::OlderThanInstalled)),
            std::cmp::Ordering::Equal => return Ok(Outcome::AlreadyInstalled { version: text }),
            std::cmp::Ordering::Greater => {}
        }
    }
    step(
        runner,
        &plan::apt_update(&source, version),
        FailureCode::UpdateFailed,
    )?;
    step(
        runner,
        &plan::apt_install(&source, version),
        FailureCode::InstallFailed,
    )?;
    // Trust nothing the units said: read the package database again.
    match read_installed(runner) {
        Ok(Some((text, installed))) if installed == *version.parsed() => {
            Ok(Outcome::Installed { version: text })
        }
        _ => Err(fail(
            FailureCode::VerifyFailed,
            "the installed version is not the one asked for",
        )),
    }
}

/// Run one step of the install; a failure to start it or a status but 0 is `code`.
fn step(runner: &dyn Runner, cmd: &Cmd, code: FailureCode) -> Result<(), Outcome> {
    match runner.run(cmd) {
        Ok(exec) if exec.success() => Ok(()),
        Ok(exec) => {
            let output = if exec.stderr.is_empty() {
                exec.stdout_text()
            } else {
                exec.stderr_text()
            };
            Err(fail(code, format!("exit status {:?}: {output}", exec.code)))
        }
        Err(e) => Err(fail(code, format!("{}: {e}", cmd.program))),
    }
}

#[cfg(test)]
mod tests;
