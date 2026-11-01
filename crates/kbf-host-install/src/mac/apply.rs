//! Putting a staged release in and restarting the daemon on it, as its own launchd job.
//!
//! The install verb returns once it has launched the job ([`launch`]), so a dropped ssh
//! session cannot end the work between `bootout` and `bootstrap`. The job is this program
//! again (`--apply`, [`run`]) under the one-shot label [`APPLY_LABEL`], started as root by
//! launchd. Its steps:
//!
//! 1. lay the release out as `releases/<commit>/kbf` (owned by root, 0755, no quarantine
//!    attribute) and `VERSION`; a release that is already there must be the same bytes;
//! 2. move `previous` to the release that ran and `current` to the new one, each by an
//!    atomic rename of a link made aside;
//! 3. `launchctl bootout` the daemon, wait (bounded) until `launchctl print` no longer
//!    finds it, `launchctl bootstrap` it, wait (bounded) for its `welcomed` line in the
//!    error log, counted from where the log ended before the bootstrap, and require that
//!    the daemon now runs the release that was started (the same reading `query` reports as
//!    `running_commit`);
//! 4. if step 3 fails, move the links back and do step 3 again on the old release, which
//!    is checked to be root's first.
//!
//! The links as they were before step 2 are in the plan, so that the apply (and whoever
//! has to repair a job that was killed halfway) works from what the install saw, not from
//! what a half-moved flip left.
//!
//! The stage is removed, then the result goes to the status file ([`status`]): `done`, or
//! `failed:<reason>` with what became of the old release. The same line is appended to the
//! audit log. The drain that keeps builds from being cut is the updater's,
//! done before it asks for the install; nothing here waits for leases.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::kit::{self, Links};
use super::layout::{APPLY_LABEL, BINARY, DAEMON_LABEL, MacHost, VERSION_FILE};
use super::stage::{Staged, run_ok, sha256_of, strip_quarantine};
use super::status::{self, ApplyState};
use super::tools;
use crate::audit::{self, Audit};
use crate::host::fail;
use crate::outcome::{FailureCode, Outcome};
use crate::request::Version;
use crate::runner::Runner;
use crate::trust::nofollow;

/// What the apply job reads from the stage.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub commit: String,
    pub version: String,
    pub binary_sha256: String,
    /// The release `current` named when the install was accepted.
    pub before_current: Option<String>,
    /// The release `previous` named then.
    pub before_previous: Option<String>,
}

const PLAN_FILE: &str = "plan.json";

/// The most of the daemon's error log read after a restart.
const LOG_WINDOW: u64 = 1024 * 1024;

fn launch_failed(detail: impl Into<String>) -> Outcome {
    fail(FailureCode::InstallFailed, detail)
}

/// Write the plan and the job's plist, mark the apply `running` and start the job.
///
/// # Errors
/// An install failure when the job cannot be started; the status then says
/// `failed:launch_failed`.
pub fn launch(
    host: &MacHost,
    runner: &dyn Runner,
    staged: &Staged,
    version: &Version,
    before: &Links,
) -> Result<(), Outcome> {
    let plan = Plan {
        commit: staged.commit.clone(),
        version: version.as_str().to_owned(),
        binary_sha256: staged.binary_sha256.clone(),
        before_current: before.current.clone(),
        before_previous: before.previous.clone(),
    };
    let started = write_plan(host, &plan)
        .and_then(|()| status::write(host, &ApplyState::Running))
        .map_err(|e| launch_failed(format!("cannot record the plan: {e}")))
        .and_then(|()| start_job(host, runner));
    if let Err(outcome) = started {
        let _ = status::write(host, &ApplyState::Failed("launch_failed".to_owned()));
        return Err(outcome);
    }
    Ok(())
}

fn write_plan(host: &MacHost, plan: &Plan) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(host.stage().join(PLAN_FILE))?;
    file.write_all(
        serde_json::to_string(plan)
            .map_err(io::Error::other)?
            .as_bytes(),
    )
}

/// The plist of the one-shot job: it runs once when loaded and is not kept alive. It lives
/// in the work directory, not in `/Library/LaunchDaemons`, so a reboot does not run it again.
pub fn plist_xml(host: &MacHost) -> String {
    let escape = |text: &str| {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let log = host.work.join("apply.log");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{APPLY_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\
         \t\t<string>{exe}</string>\n\t\t<string>--apply</string>\n\t\t<string>{stage}</string>\n\t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<false/>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
         \t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
         </dict>\n</plist>\n",
        exe = escape(&host.self_exe.display().to_string()),
        stage = escape(&host.stage().display().to_string()),
        log = escape(&log.display().to_string()),
    )
}

fn start_job(host: &MacHost, runner: &dyn Runner) -> Result<(), Outcome> {
    let limit = host.tuning.tool;
    let plist = host.apply_plist();
    fs::write(&plist, plist_xml(host))
        .and_then(|()| fs::set_permissions(&plist, fs::Permissions::from_mode(0o644)))
        .map_err(|e| launch_failed(format!("cannot write the job: {e}")))?;
    // A job that has finished stays loaded; it has to be gone before the next one is.
    if loaded(host, runner, APPLY_LABEL) {
        let _ = runner.run(&tools::bootout(APPLY_LABEL, limit));
        if !wait_gone(host, runner, APPLY_LABEL) {
            return Err(launch_failed("the last apply job cannot be unloaded"));
        }
    }
    run_ok(
        runner,
        &tools::bootstrap(&plist, limit),
        "launchctl bootstrap",
    )
    .map(drop)
    .map_err(launch_failed)
}

/// Whether launchd has the service. One that cannot be asked about counts as loaded.
fn loaded(host: &MacHost, runner: &dyn Runner, label: &str) -> bool {
    runner
        .run(&tools::print(label, host.tuning.tool))
        .map_or(true, |exec| exec.success())
}

/// Wait, for at most the tuning's `gone`, until launchd no longer has the service:
/// `launchctl bootout` returns before the service is gone, and a `bootstrap` in that gap
/// fails with an input/output error.
fn wait_gone(host: &MacHost, runner: &dyn Runner, label: &str) -> bool {
    let deadline = (host.pace.now)() + host.tuning.gone;
    loop {
        if !loaded(host, runner, label) {
            return true;
        }
        if (host.pace.now)() >= deadline {
            return false;
        }
        (host.pace.sleep)(host.tuning.poll);
    }
}

/// The apply job's body: apply, clear the stage, record the result. The exit status is 0 for
/// done and 1 for a failure.
///
/// The stage goes first: the status leaves `running` only when nothing of this apply is
/// left, so an install that sees the end cannot make a stage this job then removes.
pub fn run(host: &MacHost, runner: &dyn Runner) -> u8 {
    let plan = read_plan(host);
    let result = plan
        .as_ref()
        .ok_or_else(|| "plan_invalid".to_owned())
        .and_then(|plan| apply(host, runner, plan));
    let state = match &result {
        Ok(()) => ApplyState::Done,
        Err(why) => ApplyState::Failed(why.clone()),
    };
    let _ = fs::remove_dir_all(host.stage());
    if result.is_ok() {
        // The release is applied: the ones the Mac no longer keeps go, before the status says
        // done, so no install lays out a release meanwhile. A prune that fails removes less;
        // the apply is done all the same.
        let _ = super::retain::prune(host, kbf_installer::retain::DEFAULT_KEEP, None);
    }
    // The result stays in the status file; an unwritable file is all there is to tell.
    let written = status::write(host, &state);
    record(host, plan.as_ref(), &state);
    u8::from(result.is_err() || written.is_err())
}

/// Append the end of the apply to the audit log; a log that cannot be opened loses the line
/// (the status file has the result).
fn record(host: &MacHost, plan: Option<&Plan>, state: &ApplyState) {
    let Ok(mut log) = Audit::open(&host.trust, &host.audit) else {
        return;
    };
    let entry = audit::Entry {
        time: audit::now(),
        verb: Some("apply"),
        version: plan.map(|plan| plan.version.clone()),
        channel: None,
        unit: None,
        outcome: if *state == ApplyState::Done {
            "applied"
        } else {
            "failed"
        },
        reason: None,
        result: Some(state.line()),
        command: None,
    };
    let _ = log.append(&entry);
}

fn read_plan(host: &MacHost) -> Option<Plan> {
    let text = host.trust.read(&host.stage().join(PLAN_FILE)).ok()?;
    let plan: Plan = serde_json::from_str(&text).ok()?;
    let sound = super::release::is_commit(&plan.commit)
        && plan.binary_sha256.len() == 64
        && Version::new(&plan.version).is_ok()
        && [&plan.before_current, &plan.before_previous]
            .into_iter()
            .flatten()
            .all(|commit| super::release::is_commit(commit));
    sound.then_some(plan)
}

/// The steps; an error is the status text.
fn apply(host: &MacHost, runner: &dyn Runner, plan: &Plan) -> Result<(), String> {
    // The staged file is what was checked.
    let staged = host.stage().join(BINARY);
    host.trust
        .check_file(&staged)
        .map_err(|_| "stage_untrusted")?;
    if sha256_of(&staged).ok().as_deref() != Some(plan.binary_sha256.as_str()) {
        return Err("stage_changed".to_owned());
    }
    let before = Links {
        current: plan.before_current.clone(),
        previous: plan.before_previous.clone(),
    };
    lay_out(host, runner, plan, &staged)?;
    switch(host, runner, &plan.commit, &before)
}

/// Move the links to the laid-out release `commit` and restart the daemon on it. When the
/// daemon does not come up, the links go back to `before` and the old release is started
/// again; the error says which step failed and what became of the old release.
pub(crate) fn switch(
    host: &MacHost,
    runner: &dyn Runner,
    commit: &str,
    before: &Links,
) -> Result<(), String> {
    if flip(host, before, commit).is_err() {
        // A flip that stopped halfway must not leave `previous` naming the release that ran.
        let _ = restore(host, before);
        return Err("links_failed".to_owned());
    }
    let Err(why) = restart(host, runner, commit) else {
        return Ok(());
    };
    // The new release did not come up: the links go back, and the old release must.
    let tail = match switch_back(host, runner, before) {
        Ok(()) => "rolled_back",
        Err(failed) => failed,
    };
    Err(format!("{why}:{tail}"))
}

/// Put both links back as `before` had them and start the release `before.current` names.
///
/// # Errors
/// What became of the old release: `rollback_failed`, or `no_previous` when there was none.
pub(crate) fn switch_back(
    host: &MacHost,
    runner: &dyn Runner,
    before: &Links,
) -> Result<(), &'static str> {
    let restored = restore(host, before).is_ok();
    let old = before.current.as_deref().ok_or("no_previous")?;
    // The daemon is not restarted on a file that is not root's to run, nor one that is gone.
    let back = restored
        && host
            .kit_trust
            .check_file(&host.release(old).join(BINARY))
            .is_ok()
        && restart(host, runner, old).is_ok();
    if back { Ok(()) } else { Err("rollback_failed") }
}

/// `releases/<commit>/kbf` and its `VERSION`.
pub(crate) fn lay_out(
    host: &MacHost,
    runner: &dyn Runner,
    plan: &Plan,
    staged: &Path,
) -> Result<(), String> {
    let dest = host.release(&plan.commit);
    let binary = dest.join(BINARY);
    if fs::symlink_metadata(&dest).is_err() {
        let aside = host.releases().join(format!(".{}.new", plan.commit));
        let _ = fs::remove_dir_all(&aside);
        fs::create_dir(&aside)
            .and_then(|()| fs::copy(staged, aside.join(BINARY)))
            .and_then(|_| {
                fs::set_permissions(aside.join(BINARY), fs::Permissions::from_mode(0o755))?;
                fs::set_permissions(&aside, fs::Permissions::from_mode(0o755))?;
                fs::rename(&aside, &dest)
            })
            .map_err(|_| "layout_failed")?;
    }
    // A release is written once: what is there, laid out now or before, is the staged bytes.
    if sha256_of(&binary).ok().as_deref() != Some(plan.binary_sha256.as_str()) {
        return Err("release_differs".to_owned());
    }
    // The file that will run, and every directory above it up to the top of the kit, are
    // root's and closed to others; nothing is written into the release before that is so.
    host.kit_trust
        .check_file(&binary)
        .map_err(|_| "release_untrusted")?;
    strip_quarantine(host, runner, &binary).map_err(|_| "quarantine")?;
    write_version(&dest, &plan.version).map_err(|_| "layout_failed")?;
    Ok(())
}

fn write_version(release: &Path, version: &str) -> io::Result<()> {
    let aside = release.join(format!(".{VERSION_FILE}.new"));
    let _ = fs::remove_file(&aside);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&aside)?;
    writeln!(file, "{version}")?;
    file.sync_all()?;
    fs::rename(&aside, release.join(VERSION_FILE))
}

/// `previous` becomes the release that ran (unless that is the new one), then `current` the
/// new one.
fn flip(host: &MacHost, before: &Links, commit: &str) -> io::Result<()> {
    if let Some(ran) = before.current.as_deref().filter(|ran| *ran != commit) {
        kit::set_link(&host.previous(), Some(ran))?;
    }
    kit::set_link(&host.current(), Some(commit))
}

/// Both links as they were.
fn restore(host: &MacHost, before: &Links) -> io::Result<()> {
    let current = kit::set_link(&host.current(), before.current.as_deref());
    let previous = kit::set_link(&host.previous(), before.previous.as_deref());
    current.and(previous)
}

/// Restart the daemon: out, gone, in, welcomed, and running `commit`. The error is the
/// reason.
fn restart(host: &MacHost, runner: &dyn Runner, commit: &str) -> Result<(), &'static str> {
    let limit = host.tuning.tool;
    if loaded(host, runner, DAEMON_LABEL) {
        // The service may not be loaded by the time it is asked to leave; the wait decides.
        let _ = runner.run(&tools::bootout(DAEMON_LABEL, limit));
        if !wait_gone(host, runner, DAEMON_LABEL) {
            return Err("bootout_timeout");
        }
    }
    let offset = fs::metadata(&host.err_log).map_or(0, |meta| meta.len());
    run_ok(
        runner,
        &tools::bootstrap(&host.daemon_plist, limit),
        "launchctl bootstrap",
    )
    .map_err(|_| "bootstrap_failed")?;
    if !wait_welcomed(host, offset) {
        return Err("not_welcomed");
    }
    // A `welcomed` line is the log's word; the process is the proof.
    if wait_running(host, runner, commit) {
        Ok(())
    } else {
        Err("wrong_release")
    }
}

/// Wait, for at most the tuning's `gone`, until the daemon runs the release `commit`.
fn wait_running(host: &MacHost, runner: &dyn Runner, commit: &str) -> bool {
    let deadline = (host.pace.now)() + host.tuning.gone;
    loop {
        if kit::running_commit(host, runner).as_deref() == Some(commit) {
            return true;
        }
        if (host.pace.now)() >= deadline {
            return false;
        }
        (host.pace.sleep)(host.tuning.poll);
    }
}

/// Wait, for at most the tuning's `welcome`, for the `welcomed` line the new daemon writes
/// once the frontend has taken it in. Only what the log gained since `offset` counts.
fn wait_welcomed(host: &MacHost, offset: u64) -> bool {
    let deadline = (host.pace.now)() + host.tuning.welcome;
    loop {
        if is_welcomed(&log_since(&host.err_log, offset)) {
            return true;
        }
        if (host.pace.now)() >= deadline {
            return false;
        }
        (host.pace.sleep)(host.tuning.poll);
    }
}

/// What the file at `path` gained after its first `offset` bytes, as text (a file that
/// shrank is read whole; one that cannot be read gained nothing).
fn log_since(path: &Path, offset: u64) -> String {
    let read = || -> io::Result<String> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(nofollow())
            .open(path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(if len < offset { 0 } else { offset }))?;
        let mut bytes = Vec::new();
        file.take(LOG_WINDOW).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    read().unwrap_or_default()
}

/// Whether `log` has the line the daemon writes when the frontend welcomed it:
/// `... kbf_daemon: welcomed server=...`.
pub fn is_welcomed(log: &str) -> bool {
    log.lines()
        .any(|line| line.contains("kbf_daemon") && line.contains(": welcomed"))
}
