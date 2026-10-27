//! `kbf-guest sim`: the iOS simulator probe (E3), run inside a macOS guest.
//!
//! A root launch daemon runs this once per boot. It runs a fixed list of steps, one command
//! each, and sends the host one `result` frame per step as the step ends
//! ([`crate::frame`], port 5000, like every probe), then one summary frame:
//!
//! 1. as root: `xcode-select -s`, `xcodebuild -license accept`, `xcodebuild -runFirstLaunch`,
//!    and for each runtime the host staged `xcrun simctl runtime add <dmg>` (a `.dmg`) or
//!    `xcodebuild -importPlatform <bundle>` (a `.exportedBundle`, what
//!    `xcodebuild -downloadPlatform -exportPath` writes);
//!    except those the base's priming boot already did ([`sim_primed`]): a step the record
//!    covers is reported `ok` with the note `skipped: done at prime` and not run;
//! 2. in the session of the guest's auto-login user (`launchctl asuser <uid> sudo -u <user>`):
//!    `simctl list runtimes -j` and `list devicetypes -j` (the runtimes output picks the
//!    runtime and, from that runtime's `supportedDeviceTypes`, the device type; the
//!    devicetypes output is the fallback when the runtime lists none), `simctl create`, `boot`, `bootstatus -b`, an optional app
//!    (`install`, `launch`), `simctl io screenshot`, optionally the UI tests staged by
//!    `image setup --sim-tests` (`xctest`, `xctest-summary`: [`crate::sim_xctest`]), and
//!    `simctl shutdown`.
//!
//! # Whose session
//! See [`sim_user`](crate::sim_user): the recorded user, or the console owner.
//!
//! # No hang, no guess
//! Every command has a deadline from [`Config`] and is killed (with its process group) when
//! it passes; the step is then `timeout`. A step runs only when the steps it needs
//! succeeded; otherwise it is reported `skipped` with the step to blame, and its command is
//! not run. A step that cannot even be built (the earlier output did not name a runtime) is
//! `error`. Nothing waits for anything unbounded: the wait for the console user counts a
//! fixed number of pauses.
//!
//! # What the frames hold
//! `sim-step-NN-<step>.json` is a [`StepFrame`]: the step, its argv, the exit code (or the
//! signal, or `null`), the milliseconds the command took, and the tail of its output
//! (at most [`Config::tail_bytes`] each, cut at a character boundary). `sim-screenshot.json`
//! carries the screenshot as base64 within [`SCREENSHOT_MAX_BYTES`], so the frame fits the
//! codec's body limit. `sim-summary.json` ([`SummaryFrame`]) ends the run.
//!
//! This module runs no process and opens no socket: the commands, the console user, the
//! files and the vsock send come from a [`SimHost`], so the whole flow runs in tests on any
//! OS with a fake. The real host is `sim_host` (macOS) over [`crate::sim_run`].

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::frame::{Frame, Kind, MAX_BODY_BYTES, SendError};
use crate::probe::send_with_retries;
pub use crate::sim_choose::{
    RuntimeDeviceType, choose_device_type, choose_device_type_for_runtime, choose_runtime,
};
use crate::sim_choose::{take_fallback_device_type, take_runtime};
pub use crate::sim_parse::{
    CUT_MARK, MAX_BUNDLE_ID_BYTES, MAX_ECHO_BYTES, name_ok, parse_bundle_id, parse_udid, tail_text,
};
use crate::sim_primed::{self, Primed};
pub use crate::sim_user::Wanted;
use crate::sim_user::{unmet_wish, wait_console_user};
use crate::sim_xctest;
use crate::simpaths;

/// The prefix of a step frame's name; the host collects the frames by it.
pub const STEP_PREFIX: &str = "sim-step-";
/// The name of the frame that ends the run.
pub const SUMMARY_NAME: &str = "sim-summary.json";
/// The name of the screenshot frame.
pub const SCREENSHOT_NAME: &str = "sim-screenshot.json";
/// The name the simulator device is created with.
pub const DEVICE_NAME: &str = "kbf-sim";
/// Where the screenshot is written (a directory the console user may write; the guest side
/// makes it).
pub const OUT_DIR: &str = "/private/var/kbf-sim/out";
pub const SCREENSHOT_PATH: &str = "/private/var/kbf-sim/out/screenshot.jpg";
/// The largest screenshot sent, in bytes before base64: 4/3 of it plus the frame's JSON
/// wrapper stays under [`MAX_BODY_BYTES`].
pub const SCREENSHOT_MAX_BYTES: usize = 700_000;
/// The longest name of a staged runtime image or app, in bytes: it becomes part of a result
/// frame's name, which the host limits to 100.
pub const MAX_STAGED_NAME_BYTES: usize = 64;
/// The suffix of a staged runtime that is an exported platform bundle (a directory), not a
/// disk image (`.dmg`).
pub const BUNDLE_SUFFIX: &str = ".exportedBundle";
/// The most output a command's capture keeps (more is dropped, and the step says so).
pub const MAX_CAPTURE_BYTES: usize = 4 << 20;

const XCODE_SELECT: &str = "/usr/bin/xcode-select";
const XCODEBUILD: &str = "/usr/bin/xcodebuild";
const XCRUN: &str = "/usr/bin/xcrun";
const PLUTIL: &str = "/usr/bin/plutil";
const LAUNCHCTL: &str = "/bin/launchctl";
const SUDO: &str = "/usr/bin/sudo";

/// Who runs a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    Root,
    /// The auto-login user, in that user's session.
    User,
}

/// How long a step may take: the class picks the deadline from [`Config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Quick,
    FirstLaunch,
    RuntimeAdd,
    Boot,
    /// The UI tests.
    XcTest,
}

/// A step's argument list, some of which only earlier output can give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Argv {
    Fixed(Vec<String>),
    /// Built from the [`State`] when the step is due.
    Late(Late),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Late {
    Create,
    Boot,
    BootStatus,
    Install,
    Launch,
    Screenshot,
    Shutdown,
    /// `xcodebuild test-without-building` of the staged `.xctestrun`.
    XcTest,
}

/// One command of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    pub who: Who,
    pub class: Class,
    /// The steps that must have succeeded.
    pub needs: Vec<String>,
    /// The steps that must have run to an end (succeeded or failed; not skipped, timed out
    /// or unbuildable): a step that reads what a failing step leaves.
    pub ran: Vec<String>,
    pub argv: Argv,
}

/// What the run needs to know about the guest to plan: the runtime images and the app the
/// host staged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inputs {
    /// File names in [`simpaths::RUNTIME_DIR`].
    pub runtimes: Vec<String>,
    /// The `.app` directory name staged in [`simpaths::APP_DIR`], if any.
    pub app: Option<String>,
    /// The `.xctestrun` file name staged in [`simpaths::TESTS_DIR`], if any.
    pub tests: Option<String>,
}

fn fixed(who: Who, class: Class, id: &str, needs: &[&str], argv: &[&str]) -> Step {
    Step {
        id: id.to_owned(),
        who,
        class,
        needs: needs.iter().map(|s| (*s).to_owned()).collect(),
        ran: Vec::new(),
        argv: Argv::Fixed(argv.iter().map(|s| (*s).to_owned()).collect()),
    }
}

fn late(class: Class, id: &str, needs: &[&str], which: Late) -> Step {
    Step {
        id: id.to_owned(),
        who: Who::User,
        class,
        needs: needs.iter().map(|s| (*s).to_owned()).collect(),
        ran: Vec::new(),
        argv: Argv::Late(which),
    }
}

/// The path of a staged runtime image in the guest.
#[must_use]
pub fn runtime_path(name: &str) -> String {
    format!("{}/{name}", simpaths::RUNTIME_DIR)
}

/// The path of the staged app in the guest.
#[must_use]
pub fn app_path(name: &str) -> String {
    format!("{}/{name}", simpaths::APP_DIR)
}

/// The steps, in order. A step needs only steps before it.
#[must_use]
pub fn plan(inputs: &Inputs) -> Vec<Step> {
    use Class::{Boot, FirstLaunch, Quick, RuntimeAdd};
    use Who::{Root, User};
    let mut steps = vec![
        fixed(
            Root,
            Quick,
            "xcode-select",
            &[],
            &[XCODE_SELECT, "-s", simpaths::XCODE_APP],
        ),
        fixed(
            Root,
            Quick,
            "license",
            &["xcode-select"],
            &[XCODEBUILD, "-license", "accept"],
        ),
        fixed(
            Root,
            FirstLaunch,
            "first-launch",
            &["license"],
            &[XCODEBUILD, "-runFirstLaunch"],
        ),
    ];
    let mut adds = Vec::new();
    for name in &inputs.runtimes {
        // An image is added, a bundle is imported; both are slow and share one deadline.
        let path = runtime_path(name);
        let (id, argv) = if name.ends_with(BUNDLE_SUFFIX) {
            (
                format!("runtime-import-{name}"),
                vec![XCODEBUILD, "-importPlatform", &path],
            )
        } else {
            (
                format!("runtime-add-{name}"),
                vec![XCRUN, "simctl", "runtime", "add", &path],
            )
        };
        steps.push(fixed(Root, RuntimeAdd, &id, &["first-launch"], &argv));
        adds.push(id);
    }
    // The runtimes are listed once every staged one has been added or imported.
    let after_adds: Vec<&str> = if adds.is_empty() {
        vec!["first-launch"]
    } else {
        adds.iter().map(String::as_str).collect()
    };
    steps.push(fixed(
        User,
        Quick,
        "list-runtimes",
        &after_adds,
        &[XCRUN, "simctl", "list", "runtimes", "-j"],
    ));
    steps.push(fixed(
        User,
        Quick,
        "list-devicetypes",
        &["first-launch"],
        &[XCRUN, "simctl", "list", "devicetypes", "-j"],
    ));
    steps.push(late(
        Quick,
        "create",
        &["list-runtimes", "list-devicetypes"],
        Late::Create,
    ));
    steps.push(late(Boot, "boot", &["create"], Late::Boot));
    steps.push(late(Boot, "bootstatus", &["boot"], Late::BootStatus));
    if let Some(app) = &inputs.app {
        steps.push(fixed(
            User,
            Quick,
            "app-id",
            &["bootstatus"],
            &[
                PLUTIL,
                "-extract",
                "CFBundleIdentifier",
                "raw",
                "-o",
                "-",
                &format!("{}/Info.plist", app_path(app)),
            ],
        ));
        steps.push(late(
            Boot,
            "install",
            &["bootstatus", "app-id"],
            Late::Install,
        ));
        steps.push(late(Boot, "launch", &["install"], Late::Launch));
    }
    // A failed install or launch still leaves a booted device worth a picture.
    steps.push(late(Quick, "screenshot", &["bootstatus"], Late::Screenshot));
    if inputs.tests.is_some() {
        let after = if inputs.app.is_some() {
            "launch"
        } else {
            "bootstatus"
        };
        steps.extend(sim_xctest::steps(after));
    }
    steps.push(late(Quick, "shutdown", &["boot"], Late::Shutdown));
    steps
}

/// What the run has learnt from earlier steps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub runtime: Option<String>,
    pub device_type: Option<String>,
    pub udid: Option<String>,
    pub bundle_id: Option<String>,
    pub app: Option<String>,
    /// The `.xctestrun` to run.
    pub xctestrun: Option<String>,
}

impl State {
    fn need<'a>(value: &'a Option<String>, what: &str) -> Result<&'a str, String> {
        value
            .as_deref()
            .ok_or_else(|| format!("no {what} from an earlier step"))
    }

    /// The argument list of a late step.
    ///
    /// # Errors
    /// An earlier step did not give what this one needs.
    pub fn resolve(&self, which: Late) -> Result<Vec<String>, String> {
        let io = |rest: &[&str]| {
            let mut v = vec![XCRUN.to_owned(), "simctl".to_owned()];
            v.extend(rest.iter().map(|s| (*s).to_owned()));
            v
        };
        Ok(match which {
            Late::Create => io(&[
                "create",
                DEVICE_NAME,
                Self::need(&self.device_type, "device type")?,
                Self::need(&self.runtime, "runtime")?,
            ]),
            Late::Boot => io(&["boot", Self::need(&self.udid, "device id")?]),
            Late::BootStatus => io(&["bootstatus", Self::need(&self.udid, "device id")?, "-b"]),
            Late::Install => {
                let app = Self::need(&self.app, "app")?;
                io(&[
                    "install",
                    Self::need(&self.udid, "device id")?,
                    &app_path(app),
                ])
            }
            Late::Launch => io(&[
                "launch",
                Self::need(&self.udid, "device id")?,
                Self::need(&self.bundle_id, "bundle identifier")?,
            ]),
            Late::Screenshot => io(&[
                "io",
                Self::need(&self.udid, "device id")?,
                "screenshot",
                "--type=jpeg",
                SCREENSHOT_PATH,
            ]),
            Late::Shutdown => io(&["shutdown", Self::need(&self.udid, "device id")?]),
            Late::XcTest => sim_xctest::test_argv(
                Self::need(&self.xctestrun, "staged .xctestrun")?,
                Self::need(&self.udid, "device id")?,
            ),
        })
    }
}

/// The deadlines and caps of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub quick: Duration,
    pub first_launch: Duration,
    pub runtime_add: Duration,
    pub boot: Duration,
    pub xctest: Duration,
    /// How long the run waits for the auto-login user, counted in [`CONSOLE_POLL`] pauses.
    pub console_wait: Duration,
    /// How much of a command's stdout and stderr each a step frame keeps.
    pub tail_bytes: usize,
    /// The vsock port the host listens on.
    pub port: u32,
}

/// One pause while waiting for the console user.
pub const CONSOLE_POLL: Duration = Duration::from_secs(1);

impl Config {
    #[must_use]
    pub fn deadline(&self, class: Class) -> Duration {
        match class {
            Class::Quick => self.quick,
            Class::FirstLaunch => self.first_launch,
            Class::RuntimeAdd => self.runtime_add,
            Class::Boot => self.boot,
            Class::XcTest => self.xctest,
        }
    }
}

/// `sim`'s arguments (the flags of the launch daemon).
#[derive(Debug, Clone, clap::Args)]
pub struct SimArgs {
    /// The vsock port the host listens on for results.
    #[arg(long, default_value_t = 5000)]
    pub vsock_port: u32,
    /// The deadline of a quick command (select, license, list, create, screenshot, shutdown).
    #[arg(long, default_value_t = 120)]
    pub quick_secs: u64,
    /// The deadline of `xcodebuild -runFirstLaunch`.
    #[arg(long, default_value_t = 1800)]
    pub first_launch_secs: u64,
    /// The deadline of each `simctl runtime add`.
    #[arg(long, default_value_t = 1800)]
    pub runtime_add_secs: u64,
    /// The deadline of boot, bootstatus, install and launch.
    #[arg(long, default_value_t = 600)]
    pub boot_secs: u64,
    /// The deadline of the UI tests (`xcodebuild test-without-building`).
    #[arg(long, default_value_t = 1800)]
    pub xctest_secs: u64,
    /// How long to wait for the auto-login user's session.
    #[arg(long, default_value_t = 300)]
    pub console_wait_secs: u64,
    /// How much output of each command a step frame keeps, per stream.
    #[arg(long, default_value_t = 8192)]
    pub tail_bytes: usize,
}

impl From<&SimArgs> for Config {
    fn from(a: &SimArgs) -> Self {
        Self {
            quick: Duration::from_secs(a.quick_secs),
            first_launch: Duration::from_secs(a.first_launch_secs),
            runtime_add: Duration::from_secs(a.runtime_add_secs),
            boot: Duration::from_secs(a.boot_secs),
            xctest: Duration::from_secs(a.xctest_secs),
            console_wait: Duration::from_secs(a.console_wait_secs),
            tail_bytes: a.tail_bytes,
            port: a.vsock_port,
        }
    }
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    Exited(i32),
    Signaled(i32),
    /// Still running at its deadline; it was killed.
    TimedOut,
    SpawnFailed(String),
}

/// What running a command gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub ended: Ended,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Whether more output than [`MAX_CAPTURE_BYTES`] came and was dropped.
    pub cut: bool,
    pub ms: u64,
}

/// How a command whose stdout was streamed to a sink ended (nothing is kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Streamed {
    pub ended: Ended,
    pub ms: u64,
}

/// The console user: whose session the user steps run in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleUser {
    pub uid: u32,
    pub name: String,
}

/// What the run asks of the guest; a fake in tests.
pub trait SimHost {
    fn is_root(&self) -> bool;
    /// The names of the files in [`simpaths::RUNTIME_DIR`].
    fn runtime_images(&self) -> Vec<String>;
    /// The names of the entries in [`simpaths::APP_DIR`].
    fn app_bundles(&self) -> Vec<String>;
    /// The names of the entries in [`simpaths::TESTS_DIR`].
    fn xctest_entries(&self) -> Vec<String>;
    /// Removes the result bundle of the tests ([`sim_xctest::RESULT_PATH`]) without following
    /// a link; one that is not there is fine.
    ///
    /// # Errors
    /// It is there and could not be removed.
    fn clear_result_bundle(&self) -> Result<(), String>;
    /// Whether the result bundle is there, as a real directory.
    fn result_bundle_present(&self) -> bool;
    /// The user logged in at the console, if there is one.
    fn console_user(&self) -> Option<ConsoleUser>;
    /// The user `kbf-guest mkuser` recorded as ready ([`simpaths::USER_READY`]), if any.
    fn ready_user(&self) -> Option<String>;
    /// The user `image setup --guest-user` asked for ([`simpaths::USER_WANTED`]), if any.
    fn wanted_user(&self) -> Wanted;
    /// Makes [`OUT_DIR`] writable by the console user.
    ///
    /// # Errors
    /// The directory could not be made.
    fn prepare_out_dir(&self) -> Result<(), String>;
    /// The screenshot file, if it is a regular file of at most `cap` bytes.
    ///
    /// # Errors
    /// It is missing, not a regular file, larger than `cap`, or unreadable.
    fn read_screenshot(&self, cap: usize) -> Result<Vec<u8>, String>;
    /// Runs `argv` (no shell) and waits for it, at most `deadline`.
    fn run(&self, argv: &[String], deadline: Duration) -> Outcome;
    /// One attempt to send `frame` to the host on `port` and read the reply.
    ///
    /// # Errors
    /// No connection or reply, or the host refused the frame.
    fn send(&self, port: u32, frame: &Frame) -> Result<(), SendError>;
    fn pause(&self, d: Duration);
    /// The regular file `path` of at most `cap` bytes (`staged.json` and the prime's record);
    /// a file that cannot be read means nothing counts as done at prime.
    ///
    /// # Errors
    /// It is missing, not a regular file, larger than `cap`, or unreadable.
    fn read_file(&self, path: &str, cap: usize) -> Result<Vec<u8>, String>;
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    /// The command ran and exited non-zero or by a signal.
    Failed,
    Timeout,
    /// The command could not be built or started, or its output was unusable.
    Error,
    /// Not run: a step it needs did not succeed.
    Skipped,
}

/// A step frame's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFrame {
    pub step: String,
    pub index: usize,
    pub who: Who,
    pub status: Status,
    /// The argument list as the step means it (without the wrapper that enters the user's
    /// session). Empty when it could not be built.
    pub argv: Vec<String>,
    pub exit: Option<i32>,
    pub signal: Option<i32>,
    pub ms: u64,
    pub deadline_secs: u64,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// What the run took from the output, or why the step is an error.
    pub note: Option<String>,
    /// For a skipped step, the step that did not succeed.
    pub skipped_because: Option<String>,
}

/// The frame that ends the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryFrame {
    pub complete: bool,
    /// Every step with its status, in order.
    pub steps: Vec<(String, Status)>,
    pub ok: bool,
    pub runtime: Option<String>,
    pub device_type: Option<String>,
    pub udid: Option<String>,
    /// The screenshot's size in bytes when it was sent.
    pub screenshot_bytes: Option<usize>,
    /// Names of the frames that never reached the host.
    pub undelivered: Vec<String>,
    pub note: Option<String>,
}

/// The name of the frame for step number `index` (zero-based).
#[must_use]
pub fn step_frame_name(index: usize, id: &str) -> String {
    format!("{STEP_PREFIX}{:02}-{id}.json", index + 1)
}

/// The frames a plan of `n_runtimes` runtimes sends at most: every step, the screenshot,
/// the summary.
#[must_use]
pub fn max_frames(n_runtimes: usize) -> usize {
    let inputs = Inputs {
        runtimes: (0..n_runtimes).map(|i| format!("r{i}.dmg")).collect(),
        app: Some("a.app".to_owned()),
        tests: Some("t.xctestrun".to_owned()),
    };
    // The screenshot, the UI tests' counts and the summary have frames of their own.
    plan(&inputs).len() + 3
}

struct Run<'a> {
    host: &'a dyn SimHost,
    cfg: &'a Config,
    log: &'a mut dyn Write,
    undelivered: Vec<String>,
    /// The user whose session the user steps need (the one `mkuser` recorded); `None`: the
    /// console owner, whoever it is.
    user_name: Option<String>,
    /// What the base's priming boot did ([`sim_primed`]); nothing unless [`run`] read it.
    primed: Primed,
}

impl<'a> Run<'a> {
    fn new(host: &'a dyn SimHost, cfg: &'a Config, log: &'a mut dyn Write) -> Self {
        Self {
            host,
            cfg,
            log,
            undelivered: Vec::new(),
            user_name: None,
            primed: Primed::default(),
        }
    }

    /// Sends `frame` with retries; a frame that never arrives is remembered, not fatal.
    fn deliver(&mut self, frame: &Frame) -> bool {
        let host = self.host;
        let port = self.cfg.port;
        let mut send = |f: &Frame| host.send(port, f);
        let ok = send_with_retries(&mut send, &|d| host.pause(d), port, frame, self.log).is_some();
        if !ok {
            self.undelivered.push(frame.name.clone());
        }
        ok
    }

    /// Sends `body` as the JSON text of a result frame named `name`. The frame types hold
    /// strings, numbers and enums only, which always serialize.
    fn deliver_json(&mut self, name: &str, body: &impl Serialize) -> bool {
        let text = format!("{:#}", serde_json::to_value(body).unwrap_or(Value::Null));
        self.deliver(&Frame::new(Kind::Result, name, &text))
    }
}

/// The command that runs `argv` as `user` in the user's session.
#[must_use]
pub fn as_user(user: &ConsoleUser, argv: &[String]) -> Vec<String> {
    let mut v: Vec<String> = [
        LAUNCHCTL,
        "asuser",
        &user.uid.to_string(),
        SUDO,
        "-u",
        &user.name,
        "-H",
        "--",
    ]
    .map(str::to_owned)
    .to_vec();
    v.extend_from_slice(argv);
    v
}

/// Runs the plan against `host`: see the module documentation. Never blocks beyond the
/// deadlines in `cfg`.
pub fn run(host: &dyn SimHost, cfg: &Config, log: &mut dyn Write) -> SummaryFrame {
    let mut r = Run::new(host, cfg, log);
    if !host.is_root() {
        return finish(
            &mut r,
            &[],
            &State::default(),
            None,
            Some("sim runs as root".into()),
        );
    }
    // A guest that is to have a user but has none yet has no session to run the user steps
    // in: running the root ones would only import the runtimes into the base for nothing.
    let ready = host.ready_user();
    // Fails closed: a wish that cannot be read stops the run as a wish that is not met does.
    if let Some(why) = ready.as_ref().map_or_else(|| unmet_wish(host), |_| None) {
        return finish(&mut r, &[], &State::default(), None, Some(why));
    }
    r.user_name = ready;
    r.primed = Primed::load(&|path, cap| host.read_file(path, cap));
    let inputs = Inputs {
        runtimes: host
            .runtime_images()
            .into_iter()
            .filter(|n| name_ok(n, ".dmg") || name_ok(n, BUNDLE_SUFFIX))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
        app: host
            .app_bundles()
            .into_iter()
            .filter(|n| name_ok(n, ".app"))
            .min(),
        tests: host
            .xctest_entries()
            .into_iter()
            .filter(|n| name_ok(n, sim_xctest::XCTESTRUN_SUFFIX))
            .min(),
    };
    let state = State {
        app: inputs.app.clone(),
        xctestrun: inputs.tests.clone(),
        ..State::default()
    };
    run_steps(&mut r, &plan(&inputs), state)
}

/// Runs `steps`, a plan the caller built, as [`run`] runs its own: a frame for each, then
/// the summary. It does not check for root.
pub fn run_plan(
    host: &dyn SimHost,
    cfg: &Config,
    steps: &[Step],
    log: &mut dyn Write,
) -> SummaryFrame {
    run_steps(&mut Run::new(host, cfg, log), steps, State::default())
}

/// Runs `steps` in order from `state`, sending a frame for each, then the summary.
fn run_steps(r: &mut Run<'_>, steps: &[Step], mut state: State) -> SummaryFrame {
    let (host, cfg) = (r.host, r.cfg);
    let mut status: BTreeMap<String, Status> = BTreeMap::new();
    let mut order: Vec<(String, Status)> = Vec::new();
    let mut user: Option<Option<ConsoleUser>> = None;
    let mut screenshot_bytes = None;
    let note = host
        .prepare_out_dir()
        .err()
        .map(|e| format!("prepare {OUT_DIR}: {e}"));
    for (index, step) in steps.iter().enumerate() {
        let blame = step
            .needs
            .iter()
            .find(|n| status.get(*n) != Some(&Status::Ok))
            .or_else(|| {
                step.ran
                    .iter()
                    .find(|n| !matches!(status.get(*n), Some(Status::Ok | Status::Failed)))
            });
        let mut f = StepFrame {
            step: step.id.clone(),
            index: index + 1,
            who: step.who,
            status: Status::Skipped,
            argv: Vec::new(),
            exit: None,
            signal: None,
            ms: 0,
            deadline_secs: cfg.deadline(step.class).as_secs(),
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            note: None,
            skipped_because: blame.cloned(),
        };
        if blame.is_none() && step.who == Who::Root && r.primed.covers(&step.id) {
            sim_primed::mark_done(&mut f, step);
        } else if blame.is_none() {
            execute(
                host,
                cfg,
                step,
                &mut state,
                &mut user,
                &mut f,
                &mut screenshot_bytes,
                r,
            );
        }
        status.insert(step.id.clone(), f.status);
        order.push((step.id.clone(), f.status));
        r.deliver_json(&step_frame_name(index, &step.id), &f);
    }
    finish(r, &order, &state, screenshot_bytes, note)
}

#[allow(clippy::too_many_arguments)]
fn execute(
    host: &dyn SimHost,
    cfg: &Config,
    step: &Step,
    state: &mut State,
    user: &mut Option<Option<ConsoleUser>>,
    f: &mut StepFrame,
    screenshot_bytes: &mut Option<usize>,
    r: &mut Run<'_>,
) {
    let argv = match &step.argv {
        Argv::Fixed(v) => Ok(v.clone()),
        Argv::Late(which) => state.resolve(*which),
    };
    let argv = match argv {
        Ok(a) => a,
        Err(e) => {
            f.status = Status::Error;
            f.note = Some(e);
            return;
        }
    };
    f.argv.clone_from(&argv);
    if let Err(e) = sim_xctest::check_before(host, &step.id) {
        f.status = Status::Error;
        f.note = Some(e);
        return;
    }
    let command = if step.who == Who::User {
        let expected = r.user_name.as_deref();
        let found = user.get_or_insert_with(|| wait_console_user(host, cfg.console_wait, expected));
        match found {
            Some(u) => as_user(u, &argv),
            None => {
                f.status = Status::Error;
                f.note = Some(match expected {
                    None => format!(
                        "no user was logged in at the console within {} s",
                        cfg.console_wait.as_secs()
                    ),
                    Some(name) => format!(
                        "the user {name} (recorded by mkuser) was not logged in at the console within {} s; the console owner is {}",
                        cfg.console_wait.as_secs(),
                        host.console_user().map_or("nobody".to_owned(), |u| u.name)
                    ),
                });
                return;
            }
        }
    } else {
        argv
    };
    let out = host.run(&command, cfg.deadline(step.class));
    f.ms = out.ms;
    let (stdout, cut_out) = tail_text(&out.stdout, cfg.tail_bytes);
    let (stderr, cut_err) = tail_text(&out.stderr, cfg.tail_bytes);
    // The summary of the UI tests goes in a frame of its own, in few fields; the step frame
    // does not repeat what the result bundle printed.
    f.stdout = if step.id == sim_xctest::SUMMARY_STEP {
        String::new()
    } else {
        stdout
    };
    f.stderr = stderr;
    f.stdout_truncated = cut_out || out.cut;
    f.stderr_truncated = cut_err || out.cut;
    f.status = match &out.ended {
        Ended::Exited(0) => Status::Ok,
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
            f.note = Some(format!("could not start: {e}"));
            Status::Error
        }
    };
    if matches!(out.ended, Ended::Exited(0)) {
        f.exit = Some(0);
        absorb(host, step, &out, state, f, screenshot_bytes, r);
    }
}

/// Takes what a successful step's output gives the later steps.
#[allow(clippy::too_many_arguments)]
fn absorb(
    host: &dyn SimHost,
    step: &Step,
    out: &Outcome,
    state: &mut State,
    f: &mut StepFrame,
    screenshot_bytes: &mut Option<usize>,
    r: &mut Run<'_>,
) {
    let text = String::from_utf8_lossy(&out.stdout);
    let taken: Option<Result<String, String>> = match step.id.as_str() {
        "list-runtimes" => Some(take_runtime(&text, state)),
        "list-devicetypes" => Some(take_fallback_device_type(&text, state)),
        "create" => Some(parse_udid(&text).inspect(|v| state.udid = Some(v.clone()))),
        "app-id" => Some(parse_bundle_id(&text).inspect(|v| state.bundle_id = Some(v.clone()))),
        sim_xctest::SUMMARY_STEP => Some(send_xctest(out, r)),
        "screenshot" => Some(
            send_screenshot(host, r)
                .inspect(|n| *screenshot_bytes = Some(*n))
                .map(|n| format!("{n} bytes sent as {SCREENSHOT_NAME}")),
        ),
        _ => None,
    };
    match taken {
        Some(Ok(what)) => f.note = Some(format!("took {what}")),
        Some(Err(e)) => {
            f.status = Status::Error;
            f.note = Some(e);
        }
        None => {}
    }
}

/// Reads the summary the step printed and sends the counts as a frame of their own.
fn send_xctest(out: &Outcome, r: &mut Run<'_>) -> Result<String, String> {
    let frame = sim_xctest::fit(
        sim_xctest::parse_summary(&String::from_utf8_lossy(&out.stdout), out.cut)?,
        sim_xctest::FRAME_LIMIT,
    );
    let note = format!(
        "{} passed, {} failed, {} skipped, sent as {}",
        frame.passed,
        frame.failed,
        frame.skipped,
        sim_xctest::XCTEST_NAME
    );
    if r.deliver_json(sim_xctest::XCTEST_NAME, &frame) {
        Ok(note)
    } else {
        Err("the UI tests' frame never reached the host".to_owned())
    }
}

/// Reads the screenshot and sends it as a frame of its own.
fn send_screenshot(host: &dyn SimHost, r: &mut Run<'_>) -> Result<usize, String> {
    let bytes = host.read_screenshot(SCREENSHOT_MAX_BYTES)?;
    let body = json!({
        "format": "jpeg",
        "bytes": bytes.len(),
        "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
    });
    let text = serde_json::to_string(&body).map_err(|e| e.to_string())?;
    if text.len() > MAX_BODY_BYTES {
        return Err(format!(
            "the screenshot's frame would be {} bytes, over the {MAX_BODY_BYTES} byte limit",
            text.len()
        ));
    }
    if r.deliver(&Frame::new(Kind::Result, SCREENSHOT_NAME, &text)) {
        Ok(bytes.len())
    } else {
        Err("the screenshot frame never reached the host".to_owned())
    }
}

fn finish(
    r: &mut Run<'_>,
    order: &[(String, Status)],
    state: &State,
    screenshot_bytes: Option<usize>,
    note: Option<String>,
) -> SummaryFrame {
    let ok = !order.is_empty() && order.iter().all(|(_, s)| *s == Status::Ok);
    let mut summary = SummaryFrame {
        complete: true,
        steps: order.to_vec(),
        ok,
        runtime: state.runtime.clone(),
        device_type: state.device_type.clone(),
        udid: state.udid.clone(),
        screenshot_bytes,
        undelivered: r.undelivered.clone(),
        note,
    };
    if !r.deliver_json(SUMMARY_NAME, &summary) {
        summary.undelivered.push(SUMMARY_NAME.to_owned());
    }
    summary
}
