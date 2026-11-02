//! The Mac side: `kbf-host-install` on a Mac, which installs the release tarball into the
//! kit layout and restarts the daemon's launchd job on it.
//!
//! Only the platform of the process (`cfg!(target_os = "macos")` in [`crate::main`]) picks
//! this over the Linux side. Everything here is plain Rust over the same ports as the Linux
//! side ([`crate::runner::Runner`], files under a [`layout::MacHost`]), so Linux CI runs it
//! with a fake `launchctl`, `codesign` and the rest.
//!
//! # The three entries
//!
//! ```text
//! kbf-host-install                 sshd's forced command: the request is SSH_ORIGINAL_COMMAND
//! sudo -n kbf-host-install --as-root   the same program as root, the request on stdin
//! kbf-host-install --apply <stage> the apply job launchd runs as root (labelled
//!                                  com.komira.kbf-host-install-apply)
//! ```
//!
//! The ssh login of the updater's key is an ordinary account, not root. Its forced command
//! (the first line) passes the request to itself as root through sudo, which has one rule
//! for the one command line `--as-root`. A process that already is root (the key of a
//! login that is root) handles the request itself. The root process checks the request
//! against the grammar again, audits it, and answers; the first line only relays.
//!
//! # The requests
//!
//! The same two as on Linux ([`crate::request`]); the third, `restart frontend=<name>`, is
//! refused as a bad command, since a Mac has no frontend unit to restart. `<version>` is the
//! version text of a build (`0.1.<seq>+g<12 hex>-<run>`); its 12 digits name the release.
//!
//! - `install kbf=<version> from <channel>`: look up the channel ([`channel`]), checks the
//!   release and stages it ([`stage`]), then launches the apply job ([`apply`]) and
//!   answers `apply_started`. The same version already installed and running is
//!   `already_installed`. An older version installs like a newer one: a rollback is an
//!   install.
//! - `query`: what is installed (the version `current` was installed as), the commit the
//!   RUNNING daemon executes, the node's id (`NODE_ID` of `av2_mac.conf`, [`conf`]), whether
//!   an install is in progress, and how the last apply stands ([`status`]).

pub mod apply;
pub mod channel;
pub mod conf;
pub mod fetch;
pub mod kit;
pub mod layout;
pub mod release;
pub(crate) mod retain;
pub mod stage;
pub mod status;
pub mod tools;

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;

use crate::audit::{self, Audit};
use crate::config::refusal;
use crate::host::{fail, respond, take_lock};
use crate::locks;
use crate::outcome::{FailureCode, Outcome, QueryReport, Refusal};
use crate::request::{BadCommand, Channel, Request, Version};
use crate::runner::{Runner, SystemRunner};
use channel::{ChannelError, MacAllowlist};
use layout::MacHost;
use status::ApplyState;

/// The longest request, in bytes, that is passed on to the root process.
pub const MAX_REQUEST_LEN: usize = 1024;

/// What the process was started with.
#[derive(Debug, Clone, Default)]
pub struct Invocation {
    /// The arguments after the program name.
    pub args: Vec<OsString>,
    /// Whether the process runs as root.
    pub root: bool,
    /// `SSH_ORIGINAL_COMMAND`.
    pub ssh_original_command: Option<OsString>,
}

impl Invocation {
    /// This process's.
    pub fn from_process() -> Self {
        Self {
            args: std::env::args_os().skip(1).collect(),
            root: rustix::process::geteuid().is_root(),
            ssh_original_command: std::env::var_os("SSH_ORIGINAL_COMMAND"),
        }
    }
}

/// Run as what the invocation says; the exit status.
pub fn run(
    host: &MacHost,
    runner: &dyn Runner,
    invocation: &Invocation,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let args: Vec<&str> = invocation
        .args
        .iter()
        .map(|a| a.to_str().unwrap_or(""))
        .collect();
    match (args.as_slice(), invocation.root) {
        ([], _) => {
            let command = invocation
                .ssh_original_command
                .as_deref()
                .and_then(|c| c.to_str());
            if invocation.root {
                handle_root(host, runner, command, out, err)
            } else {
                relay(host, runner, command, out, err)
            }
        }
        (["--as-root"], true) => {
            let mut bytes = Vec::new();
            // One byte over the limit tells the root process the request is too long.
            let read = stdin
                .take(MAX_REQUEST_LEN as u64 + 1)
                .read_to_end(&mut bytes);
            let command = read
                .ok()
                .and_then(|_| String::from_utf8(bytes).ok())
                .filter(|text| text.len() <= MAX_REQUEST_LEN);
            handle_root(host, runner, command.as_deref(), out, err)
        }
        (["--apply", stage], true) if std::path::Path::new(stage) == host.stage() => {
            apply::run(host, runner)
        }
        _ => respond(&Outcome::Refused(Refusal::BadCommand), out, err),
    }
}

/// The unprivileged process: pass the request to the root process through sudo and print its
/// answer and status as they are. A request that is not text of a bounded length is refused
/// here, since it could not be written down for the root process to audit.
fn relay(
    host: &MacHost,
    runner: &dyn Runner,
    command: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let Some(command) = command.filter(|c| c.len() <= MAX_REQUEST_LEN) else {
        return respond(&Outcome::Refused(Refusal::BadCommand), out, err);
    };
    let code = if command == "query" {
        FailureCode::QueryFailed
    } else {
        FailureCode::InstallFailed
    };
    let exec = match runner.run(&tools::sudo_root(&host.self_exe, command)) {
        Ok(exec) => exec,
        Err(e) => return respond(&fail(code, format!("sudo: {e}")), out, err),
    };
    // The root process answers with exit status 0, 2 or 3 and one line. Anything else is
    // sudo's own refusal (no rule, a password asked for) or a kill.
    match exec.code {
        Some(status @ (0 | 2 | 3)) if !exec.stdout.is_empty() => {
            let _ = err.write_all(&exec.stderr);
            let _ = out.write_all(&exec.stdout);
            u8::try_from(status).unwrap_or(3)
        }
        other => {
            let detail = format!("sudo: exit status {other:?}: {}", exec.stderr_text());
            respond(&fail(code, detail), out, err)
        }
    }
}

/// The root process: parse, answer, audit.
fn handle_root(
    host: &MacHost,
    runner: &dyn Runner,
    command: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let request = command.map_or(Err(BadCommand), Request::parse);
    let mut log = Audit::open(&host.trust, &host.audit);
    let outcome = match &request {
        Err(BadCommand) => Outcome::Refused(Refusal::BadCommand),
        Ok(Request::Query) => query(host, runner),
        Ok(Request::Install { version, channel }) => {
            install(host, runner, version, channel, log.is_ok())
        }
        // A frontend unit is a Linux systemd unit: a Mac has none to restart.
        Ok(Request::Restart { .. }) => Outcome::Refused(Refusal::BadCommand),
    };
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
        unit: None,
        outcome: outcome_word,
        reason,
        result: None,
        command: request
            .is_err()
            .then(|| command.map(audit::truncate_raw))
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

/// `query`: nothing it does changes the host.
fn query(host: &MacHost, runner: &dyn Runner) -> Outcome {
    let installed_version = kit::links(host)
        .ok()
        .and_then(|links| links.current)
        .and_then(|commit| kit::installed_version(host, &commit));
    // An install that has written `running` and not yet loaded the job holds the lock.
    let locked = locks::flock_held(&host.lock_file());
    let apply = match status::effective(host, runner) {
        Some(ApplyState::Failed(why)) if locked && why == status::LOST => Some(ApplyState::Running),
        other => other,
    };
    let in_progress = locked || apply.as_ref() == Some(&ApplyState::Running);
    Outcome::MacQueried {
        report: QueryReport {
            installed_version,
            running_commit: kit::running_commit(host, runner),
            node_id: conf::node_id(&host.kit_trust, &host.node_conf),
            in_progress,
        },
        apply: apply.map(|state| state.line()),
    }
}

/// `install`: checked end to end before the apply job is launched.
fn install(
    host: &MacHost,
    runner: &dyn Runner,
    version: &Version,
    channel: &Channel,
    audit_ok: bool,
) -> Outcome {
    install_front(host, runner, version, channel, audit_ok).unwrap_or_else(|outcome| outcome)
}

fn install_front(
    host: &MacHost,
    runner: &dyn Runner,
    version: &Version,
    channel: &Channel,
    audit_ok: bool,
) -> Result<Outcome, Outcome> {
    // The program the sudoers rule and the apply job name runs as root: it and every
    // directory above it must be root's and closed to others.
    host.trust
        .check_file(&host.self_exe)
        .map_err(|e| Outcome::Refused(refusal(e)))?;
    let allowlist = MacAllowlist::load(&host.trust, &host.allowlist).map_err(Outcome::Refused)?;
    let channel = allowlist
        .channel(&host.trust, channel)
        .map_err(|e| match e {
            ChannelError::Refused(refusal) => Outcome::Refused(refusal),
            other => Outcome::RefusedBecause(other.refusal(), other.to_string()),
        })?;
    if !audit_ok {
        return Err(fail(
            FailureCode::AuditUnavailable,
            "the audit log cannot be opened; nothing was run",
        ));
    }
    ensure_work(host)?;
    // The checks, the stage and the launch are one critical section.
    let _lock = take_lock(&host.lock_file())?;
    if status::effective(host, runner) == Some(ApplyState::Running) {
        return Err(fail(FailureCode::Busy, "an apply is running"));
    }
    let links = kit::links(host).map_err(|why| fail(FailureCode::PackageStateUnclean, why))?;
    check_kit(host)?;
    if let Some(current) = &links.current {
        let installed = kit::installed_version(host, current);
        // The same version is done only when the daemon runs it: a release that was laid out
        // and did not come up is installed again.
        if installed.as_deref() == Some(version.as_str())
            && kit::running_commit(host, runner).as_ref() == Some(current)
        {
            return Ok(Outcome::AlreadyInstalled {
                version: version.as_str().to_owned(),
            });
        }
    }
    let staged = stage::stage(host, runner, &channel, version)?;
    apply::launch(host, runner, &staged, version, &links)?;
    Ok(Outcome::ApplyStarted {
        version: version.as_str().to_owned(),
        commit: staged.commit,
    })
}

/// Root's private directory: made 0711 (others may pass through to the stage when it is
/// opened for the checks that run as another user, and may list nothing) and trusted.
pub(crate) fn ensure_work(host: &MacHost) -> Result<(), Outcome> {
    let made = fs::create_dir(&host.work)
        .and_then(|()| fs::set_permissions(&host.work, fs::Permissions::from_mode(0o711)));
    match made {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(fail(
                FailureCode::LockUnavailable,
                format!("work directory: {e}"),
            ));
        }
        _ => {}
    }
    host.trust
        .check_dir(&host.work)
        .map_err(|e| Outcome::Refused(refusal(e)))
}

/// The kit is there and is root's: the tree, `releases`, and the daemon's job.
pub(crate) fn check_kit(host: &MacHost) -> Result<(), Outcome> {
    host.kit_trust
        .check_dir(&host.releases())
        .map_err(|e| fail(FailureCode::PackageStateUnclean, format!("releases: {e}")))?;
    host.trust
        .check_file(&host.daemon_plist)
        .map_err(|e| fail(FailureCode::PackageStateUnclean, format!("daemon job: {e}")))
}

/// Run against the process's own standard streams.
pub fn run_std(host: &MacHost, runner: &dyn Runner, invocation: &Invocation) -> u8 {
    let (mut stdin, mut stdout, mut stderr) = (io::stdin(), io::stdout(), io::stderr());
    run(
        host,
        runner,
        invocation,
        &mut stdin,
        &mut stdout,
        &mut stderr,
    )
}

/// The program on a Mac: this process's invocation against the production paths.
#[must_use]
pub fn main_process() -> u8 {
    run_std(
        &MacHost::production(),
        &SystemRunner,
        &Invocation::from_process(),
    )
}

#[cfg(test)]
pub(crate) mod tests;
