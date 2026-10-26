//! `kbf-guest mkuser`: gives a guest the one user the iOS simulator probe runs as, once.
//!
//! A guest built by `image base` and `image setup` has no user account: its processes run
//! as `nobody` in the system session, and the simulator's user steps have no session to run
//! in. `image setup --guest-user <name>` stages a launch daemon (`com.kbf.mkuser`) that
//! runs this once as root in the first boot, which `image prime` drives:
//!
//! 1. if the user does not exist, create a standard (non-admin) user with `sysadminctl`,
//!    with a freshly generated random password; if it exists (an earlier run stopped part
//!    way), set that password with `dscl`;
//! 2. create the home directory (`createhomedir`);
//! 3. write `/etc/kcpassword` (the obfuscated password, [`kcpassword`]), mode 0600, owned
//!    by root, and set `autoLoginUser` in the login window's preferences;
//! 4. turn off sleep, the screen saver and its lock, and the update checks;
//! 5. write [`simpaths::USER_READY`] with the user's name.
//!
//! Then the guest is powered off (`/sbin/shutdown -h now`), so that the base's disk is
//! flushed and every clone made from it boots into the user's Aqua session.
//!
//! # Idempotent
//! When [`simpaths::USER_READY`] names the user, nothing is done and the guest is not
//! powered off: the summary says `already_ready`, and `image prime` stops the guest itself.
//!
//! # The password
//! The password is generated here, from the guest's random source, and is used in exactly
//! three places: the argument list of `sysadminctl` (or `dscl`) **inside the guest**, where
//! it is visible to other processes of the guest for the moment the command runs; the
//! content of `/etc/kcpassword` (obfuscated, root-only); and memory. It is not sent to the
//! host, not logged and not written anywhere else. Each step's frame holds the argument
//! list with the password replaced by [`WITHHELD`], and the output tails with any occurrence
//! of it replaced by [`REDACTED`]. The account is local to a throwaway guest: it holds no
//! credential of the farm.
//!
//! # Frames
//! One `result` frame per step (`mkuser-step-NN-<step>.json`, a [`StepFrame`] like the
//! simulator's), then `mkuser-summary.json` ([`MkUserSummary`]); the guest then powers off,
//! and a power-off command that fails is reported as `mkuser-shutdown.json`. A step runs
//! only when the steps it needs succeeded; the power-off happens whatever the steps did (a
//! failed run leaves no marker, so the next one starts again).
//!
//! This module runs no process and opens no socket: everything comes from a
//! [`MkUserHost`], so the whole flow runs in tests on any OS. The real host is `mkuser_host`.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::frame::{Frame, Kind, SendError};
use crate::probe::send_with_retries;
use crate::sim::{Ended, Outcome, Status, StepFrame, Who, tail_text};
use crate::simpaths;

/// The prefix of every frame this sends; the host collects the frames by it.
pub const FRAME_PREFIX: &str = "mkuser-";
/// The prefix of a step frame's name.
pub const STEP_PREFIX: &str = "mkuser-step-";
/// The name of the frame that ends the run.
pub const SUMMARY_NAME: &str = "mkuser-summary.json";
/// The name of the frame sent when the power-off command did not work.
pub const SHUTDOWN_NAME: &str = "mkuser-shutdown.json";
/// Stands for the password in a frame's argument list.
pub const WITHHELD: &str = "<withheld>";
/// Stands for the password in a frame's output tail.
pub const REDACTED: &str = "[redacted]";
/// Where the obfuscated password is written.
pub const KCPASSWORD_PATH: &str = "/etc/kcpassword";
/// The longest user name accepted, in bytes.
pub const MAX_NAME_BYTES: usize = 31;
/// The random bytes the password is made of (it is their lowercase hex: 22 characters, so
/// that [`kcpassword`] always has padding after them).
pub const PASSWORD_BYTES: usize = 11;
/// The command that powers the guest off.
pub const SHUTDOWN: [&str; 3] = ["/sbin/shutdown", "-h", "now"];

const SYSADMINCTL: &str = "/usr/sbin/sysadminctl";
const DSCL: &str = "/usr/bin/dscl";
const CREATEHOMEDIR: &str = "/usr/sbin/createhomedir";
const DEFAULTS: &str = "/usr/bin/defaults";
const PMSET: &str = "/usr/bin/pmset";
const SUDO: &str = "/usr/bin/sudo";
const LOGINWINDOW: &str = "/Library/Preferences/com.apple.loginwindow";
const SOFTWAREUPDATE: &str = "/Library/Preferences/com.apple.SoftwareUpdate";
/// The key `kcpassword` is obfuscated with (the documented, fixed one).
const KC_KEY: [u8; 11] = [
    0x7D, 0x89, 0x52, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F,
];
/// Names that are system accounts, which a guest user must not be.
const RESERVED: [&str; 6] = ["root", "nobody", "daemon", "admin", "guest", "wheel"];

/// Whether `name` can be the guest user: a short name that starts with a lowercase letter
/// and holds lowercase letters, digits, `-` and `_`, and is not a system account.
///
/// # Errors
/// Why it cannot.
pub fn check_name(name: &str) -> Result<(), String> {
    let mut bytes = name.bytes();
    if !bytes.next().is_some_and(|b| b.is_ascii_lowercase()) {
        return Err(format!(
            "{name:?}: a user name starts with a lowercase letter"
        ));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!("{name:?}: longer than {MAX_NAME_BYTES} bytes"));
    }
    if !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_')) {
        return Err(format!(
            "{name:?}: a user name holds only lowercase letters, digits, `-` and `_`"
        ));
    }
    if RESERVED.contains(&name) {
        return Err(format!("{name:?} is a system account"));
    }
    Ok(())
}

/// The content of `/etc/kcpassword` for `password`: each byte XORed with the fixed key (used
/// round and round), after zero bytes pad the password to a multiple of 12 bytes (to 12 when
/// it is shorter).
#[must_use]
pub fn kcpassword(password: &[u8]) -> Vec<u8> {
    let padded = password.len().div_ceil(12).max(1) * 12;
    (0..padded)
        .map(|i| password.get(i).copied().unwrap_or(0) ^ KC_KEY[i % KC_KEY.len()])
        .collect()
}

/// The password made of `random` (lowercase hex).
#[must_use]
pub fn password_from(random: &[u8]) -> String {
    use std::fmt::Write as _;
    random.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// One argument of a command: text, or the password (which a frame never shows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Text(String),
    Password,
}

/// What a step does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Run a command.
    Run(Vec<Arg>),
    /// Write `/etc/kcpassword`.
    WriteKcpassword,
    /// Write [`simpaths::USER_READY`].
    WriteReady,
}

/// One step of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    /// The steps that must have succeeded.
    pub needs: Vec<String>,
    pub action: Action,
}

fn text(parts: &[&str]) -> Vec<Arg> {
    parts.iter().map(|p| Arg::Text((*p).to_owned())).collect()
}

fn step(id: &str, needs: &[&str], action: Action) -> Step {
    Step {
        id: id.to_owned(),
        needs: needs.iter().map(|n| (*n).to_owned()).collect(),
        action,
    }
}

/// The steps, in order, for `name`; `exists` is whether the user is already there. A step
/// needs only steps before it.
#[must_use]
pub fn plan(name: &str, exists: bool) -> Vec<Step> {
    let home = format!("/Users/{name}");
    let user_step = if exists { "set-password" } else { "add-user" };
    let make_user = if exists {
        let mut argv = text(&[DSCL, ".", "-passwd", &home]);
        argv.push(Arg::Password);
        step(user_step, &[], Action::Run(argv))
    } else {
        let mut argv = text(&[
            SYSADMINCTL,
            "-addUser",
            name,
            "-fullName",
            name,
            "-password",
        ]);
        argv.push(Arg::Password);
        argv.extend(text(&["-home", &home]));
        step(user_step, &[], Action::Run(argv))
    };
    let as_user = |extra: &[&str]| {
        let mut v = vec![SUDO, "-u", name, "-H", "--", DEFAULTS];
        v.extend_from_slice(extra);
        Action::Run(text(&v))
    };
    let system = |domain: &str, key: &str| {
        Action::Run(text(&[DEFAULTS, "write", domain, key, "-bool", "false"]))
    };
    vec![
        make_user,
        step(
            "home",
            &[user_step],
            Action::Run(text(&[CREATEHOMEDIR, "-c", "-u", name])),
        ),
        step("kcpassword", &[user_step], Action::WriteKcpassword),
        step(
            "autologin",
            &["kcpassword"],
            Action::Run(text(&[
                DEFAULTS,
                "write",
                LOGINWINDOW,
                "autoLoginUser",
                "-string",
                name,
            ])),
        ),
        step(
            "pmset",
            &[],
            Action::Run(text(&[
                PMSET,
                "-a",
                "sleep",
                "0",
                "displaysleep",
                "0",
                "disksleep",
                "0",
            ])),
        ),
        step(
            "screensaver-idle",
            &["home"],
            as_user(&[
                "-currentHost",
                "write",
                "com.apple.screensaver",
                "idleTime",
                "-int",
                "0",
            ]),
        ),
        step(
            "screensaver-lock",
            &["home"],
            as_user(&[
                "write",
                "com.apple.screensaver",
                "askForPassword",
                "-int",
                "0",
            ]),
        ),
        step(
            "updates-check",
            &[],
            system(SOFTWAREUPDATE, "AutomaticCheckEnabled"),
        ),
        step(
            "updates-download",
            &[],
            system(SOFTWAREUPDATE, "AutomaticDownload"),
        ),
        step(
            "updates-os",
            &[],
            system(SOFTWAREUPDATE, "AutomaticallyInstallMacOSUpdates"),
        ),
        step(
            "updates-critical",
            &[],
            system(SOFTWAREUPDATE, "CriticalUpdateInstall"),
        ),
        step(
            "ready-marker",
            &[
                "home",
                "autologin",
                "pmset",
                "screensaver-idle",
                "screensaver-lock",
                "updates-check",
                "updates-download",
                "updates-os",
                "updates-critical",
            ],
            Action::WriteReady,
        ),
    ]
}

/// The deadlines and caps of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The deadline of each command.
    pub quick: Duration,
    /// How much of a command's stdout and stderr each a step frame keeps.
    pub tail_bytes: usize,
    /// The vsock port the host listens on.
    pub port: u32,
}

/// `mkuser`'s arguments (the flags of the launch daemon).
#[derive(Debug, Clone, clap::Args)]
pub struct MkUserArgs {
    /// The short name of the user to create.
    #[arg(long)]
    pub name: String,
    /// The vsock port the host listens on for results.
    #[arg(long, default_value_t = 5000)]
    pub vsock_port: u32,
    /// The deadline of each command.
    #[arg(long, default_value_t = 120)]
    pub quick_secs: u64,
    /// How much output of each command a step frame keeps, per stream.
    #[arg(long, default_value_t = 8192)]
    pub tail_bytes: usize,
}

impl From<&MkUserArgs> for Config {
    fn from(a: &MkUserArgs) -> Self {
        Self {
            quick: Duration::from_secs(a.quick_secs),
            tail_bytes: a.tail_bytes,
            port: a.vsock_port,
        }
    }
}

/// What the run asks of the guest; a fake in tests.
pub trait MkUserHost {
    fn is_root(&self) -> bool;
    /// Whether a user of that name exists.
    fn user_exists(&self, name: &str) -> bool;
    /// The user named in [`simpaths::USER_READY`], if the file is there.
    fn ready_user(&self) -> Option<String>;
    /// `n` random bytes.
    ///
    /// # Errors
    /// The random source could not be read.
    fn random_bytes(&self, n: usize) -> Result<Vec<u8>, String>;
    /// Writes `bytes` to the file `path` (its directory is made), replacing what is there,
    /// owned by root with exactly `mode`.
    ///
    /// # Errors
    /// The file could not be written.
    fn write_file(&self, path: &str, bytes: &[u8], mode: u32) -> Result<(), String>;
    /// Runs `argv` (no shell) and waits for it, at most `deadline`.
    fn run(&self, argv: &[String], deadline: Duration) -> Outcome;
    /// One attempt to send `frame` to the host on `port` and read the reply.
    ///
    /// # Errors
    /// No connection or reply, or the host refused the frame.
    fn send(&self, port: u32, frame: &Frame) -> Result<(), SendError>;
    fn pause(&self, d: Duration);
}

/// The frame that ends the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MkUserSummary {
    pub complete: bool,
    pub user: String,
    /// Every step with its status, in order.
    pub steps: Vec<(String, Status)>,
    /// All steps succeeded (or the user was ready already).
    pub ok: bool,
    /// The marker named the user already: nothing was done and the guest is not powered off.
    pub already_ready: bool,
    /// The run created the account (it did not exist).
    pub created: bool,
    /// The guest is being powered off after this frame.
    pub powering_off: bool,
    /// Names of the frames that never reached the host.
    pub undelivered: Vec<String>,
    pub note: Option<String>,
}

/// How the run ended, for the exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub summary: MkUserSummary,
    /// The power-off command, when one was run: whether it started.
    pub power_off_started: Option<bool>,
}

/// The name of the frame for step number `index` (zero-based).
#[must_use]
pub fn step_frame_name(index: usize, id: &str) -> String {
    format!("{STEP_PREFIX}{:02}-{id}.json", index + 1)
}

/// The most frames a run sends: every step, the summary and the power-off failure.
#[must_use]
pub fn max_frames() -> usize {
    plan("u", false).len() + 2
}

fn mode_text(mode: u32) -> String {
    format!("{mode:04o}")
}

struct Run<'a> {
    host: &'a dyn MkUserHost,
    cfg: &'a Config,
    log: &'a mut dyn Write,
    undelivered: Vec<String>,
}

impl Run<'_> {
    /// Sends `frame` with retries; a frame that never arrives is remembered, not fatal.
    fn deliver(&mut self, name: &str, body: &impl Serialize) -> bool {
        let text = format!("{:#}", serde_json::to_value(body).unwrap_or(Value::Null));
        let frame = Frame::new(Kind::Result, name, &text);
        let host = self.host;
        let port = self.cfg.port;
        let mut send = |f: &Frame| host.send(port, f);
        let ok = send_with_retries(&mut send, &|d| host.pause(d), port, &frame, self.log).is_some();
        if !ok {
            self.undelivered.push(name.to_owned());
        }
        ok
    }
}

fn blank_frame(index: usize, step: &Step, blame: Option<&String>, cfg: &Config) -> StepFrame {
    StepFrame {
        step: step.id.clone(),
        index: index + 1,
        who: Who::Root,
        status: Status::Skipped,
        argv: Vec::new(),
        exit: None,
        signal: None,
        ms: 0,
        deadline_secs: cfg.quick.as_secs(),
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        note: None,
        skipped_because: blame.cloned(),
    }
}

/// Fills `f` from how the command `out` ended; the password never gets in.
fn record(f: &mut StepFrame, out: &Outcome, cfg: &Config, password: &str) {
    f.ms = out.ms;
    // An empty password stands for "none": replacing it would put the mark between letters.
    let redact = |s: String| {
        if password.is_empty() {
            s
        } else {
            s.replace(password, REDACTED)
        }
    };
    let (stdout, cut_out) = tail_text(&out.stdout, cfg.tail_bytes);
    let (stderr, cut_err) = tail_text(&out.stderr, cfg.tail_bytes);
    // The password is cut out of the whole capture's tail: one that the cut would split is
    // not whole in the tail, so it cannot be matched, and a part of it is shorter than 22.
    f.stdout = redact(stdout);
    f.stderr = redact(stderr);
    f.stdout_truncated = cut_out || out.cut;
    f.stderr_truncated = cut_err || out.cut;
    f.status = match &out.ended {
        Ended::Exited(0) => {
            f.exit = Some(0);
            Status::Ok
        }
        Ended::Exited(code) => {
            f.exit = Some(*code);
            Status::Failed
        }
        Ended::Signaled(sig) => {
            f.signal = Some(*sig);
            Status::Failed
        }
        Ended::TimedOut => Status::Timeout,
        Ended::SpawnFailed(e) => {
            f.note = Some(redact(format!("could not start: {e}")));
            Status::Error
        }
    };
}

/// Runs one step into `f`.
fn execute(
    host: &dyn MkUserHost,
    cfg: &Config,
    name: &str,
    password: &str,
    s: &Step,
    f: &mut StepFrame,
) {
    match &s.action {
        Action::Run(args) => {
            f.argv = args
                .iter()
                .map(|a| match a {
                    Arg::Text(t) => t.clone(),
                    Arg::Password => WITHHELD.to_owned(),
                })
                .collect();
            let argv: Vec<String> = args
                .iter()
                .map(|a| match a {
                    Arg::Text(t) => t.clone(),
                    Arg::Password => password.to_owned(),
                })
                .collect();
            let out = host.run(&argv, cfg.quick);
            record(f, &out, cfg, password);
        }
        Action::WriteKcpassword => {
            f.argv = vec![
                "write".to_owned(),
                KCPASSWORD_PATH.to_owned(),
                mode_text(0o600),
                "root".to_owned(),
            ];
            write(
                host,
                f,
                KCPASSWORD_PATH,
                &kcpassword(password.as_bytes()),
                0o600,
            );
        }
        Action::WriteReady => {
            f.argv = vec![
                "write".to_owned(),
                simpaths::USER_READY.to_owned(),
                mode_text(0o644),
                "root".to_owned(),
            ];
            write(
                host,
                f,
                simpaths::USER_READY,
                format!("{name}\n").as_bytes(),
                0o644,
            );
        }
    }
}

fn write(host: &dyn MkUserHost, f: &mut StepFrame, path: &str, bytes: &[u8], mode: u32) {
    match host.write_file(path, bytes, mode) {
        Ok(()) => {
            f.status = Status::Ok;
            f.exit = Some(0);
        }
        Err(e) => {
            f.status = Status::Error;
            f.note = Some(format!("write {path}: {e}"));
        }
    }
}

/// Runs the plan against `host`: see the module documentation. Never blocks beyond the
/// deadline of each command.
pub fn run(host: &dyn MkUserHost, cfg: &Config, name: &str, log: &mut dyn Write) -> Finished {
    let mut r = Run {
        host,
        cfg,
        log,
        undelivered: Vec::new(),
    };
    let mut summary = MkUserSummary {
        complete: true,
        user: name.to_owned(),
        steps: Vec::new(),
        ok: false,
        already_ready: false,
        created: false,
        powering_off: false,
        undelivered: Vec::new(),
        note: None,
    };
    if let Err(e) = check_name(name) {
        summary.note = Some(e);
        return end(&mut r, summary, None);
    }
    if !host.is_root() {
        summary.note = Some("mkuser runs as root".to_owned());
        return end(&mut r, summary, None);
    }
    if host.ready_user().as_deref() == Some(name) {
        summary.ok = true;
        summary.already_ready = true;
        summary.note = Some(format!(
            "{} names {name}: nothing to do",
            simpaths::USER_READY
        ));
        return end(&mut r, summary, None);
    }
    let password = match host.random_bytes(PASSWORD_BYTES) {
        Ok(b) if b.len() == PASSWORD_BYTES => password_from(&b),
        Ok(b) => {
            summary.note = Some(format!(
                "the random source gave {} bytes, not {PASSWORD_BYTES}",
                b.len()
            ));
            return end(&mut r, summary, Some(true));
        }
        Err(e) => {
            summary.note = Some(format!("no random bytes: {e}"));
            return end(&mut r, summary, Some(true));
        }
    };
    let exists = host.user_exists(name);
    summary.created = !exists;
    let steps = plan(name, exists);
    let mut status: BTreeMap<String, Status> = BTreeMap::new();
    for (index, s) in steps.iter().enumerate() {
        let blame = s.needs.iter().find(|n| status.get(*n) != Some(&Status::Ok));
        let mut f = blank_frame(index, s, blame, cfg);
        if blame.is_none() {
            execute(host, cfg, name, &password, s, &mut f);
        }
        status.insert(s.id.clone(), f.status);
        summary.steps.push((s.id.clone(), f.status));
        r.deliver(&step_frame_name(index, &s.id), &f);
    }
    summary.ok = summary.steps.iter().all(|(_, s)| *s == Status::Ok);
    end(&mut r, summary, Some(true))
}

/// Sends the summary; then, when `power_off` is asked for, powers the guest off.
fn end(r: &mut Run<'_>, mut summary: MkUserSummary, power_off: Option<bool>) -> Finished {
    summary.powering_off = power_off.is_some();
    summary.undelivered.clone_from(&r.undelivered);
    // `deliver` notes a frame that never arrived in `r.undelivered`, which this copy of the
    // summary (the one the caller reads) takes; the frame itself held the earlier ones.
    if !r.deliver(SUMMARY_NAME, &summary) {
        summary.undelivered.push(SUMMARY_NAME.to_owned());
    }
    let power_off_started = power_off.map(|_| {
        let argv: Vec<String> = SHUTDOWN.iter().map(|s| (*s).to_owned()).collect();
        let out = r.host.run(&argv, r.cfg.quick);
        let started = matches!(out.ended, Ended::Exited(0));
        if !started {
            let mut f = StepFrame {
                step: "shutdown".to_owned(),
                index: summary.steps.len() + 1,
                who: Who::Root,
                status: Status::Failed,
                argv: argv.clone(),
                exit: None,
                signal: None,
                ms: 0,
                deadline_secs: r.cfg.quick.as_secs(),
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                note: None,
                skipped_because: None,
            };
            record(&mut f, &out, r.cfg, "");
            r.deliver(SHUTDOWN_NAME, &f);
        }
        started
    });
    Finished {
        summary,
        power_off_started,
    }
}
