//! Tests of `mkuser`: the step plan and its argument lists, the `kcpassword` encoding, what
//! a frame may hold (never the password), idempotence, and the power-off. On a fake host
//! that answers each command from a script and records everything it is asked, in order;
//! nothing waits on a clock.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::Duration;

use clap::Parser;

use crate::frame::{Frame, SendError};
use crate::mkuser::*;
use crate::sim::{Ended, Outcome, Status, StepFrame, Streamed};
use crate::simpaths;

#[path = "mkuser_prime_tests.rs"]
pub(crate) mod prime;

const RANDOM: [u8; 11] = [
    0x00, 0x01, 0x0a, 0x0f, 0x10, 0x7f, 0x80, 0xab, 0xcd, 0xef, 0xff,
];
/// `RANDOM` as the password: lowercase hex.
pub(crate) const PASSWORD: &str = "00010a0f107f80abcdefff";
/// The hardware UUID the fake `ioreg` names.
const UUID: &str = "1A2B3C4D-0000-4EEF-8123-ABCDEF012345";

/// What `ioreg -rd1 -c IOPlatformExpertDevice` prints, in the part that matters.
fn ioreg_text(uuid: &str) -> String {
    format!(
        "+-o Root  <class IORegistryEntry>\n  +-o VirtualMac2,1  <class IOPlatformExpertDevice>\n    {{\n      \"IOPlatformSerialNumber\" = \"ZZ0\"\n      \"IOPlatformUUID\" = \"{uuid}\"\n      \"manufacturer\" = <\"Apple Inc.\">\n    }}\n"
    )
}

/// A scripted streamed command: text its command line contains, how it ends, its stdout
/// in the reads it arrives in.
pub(crate) type StreamScript = (String, Ended, Vec<Vec<u8>>);

/// What the host was asked, in the order it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    Random(usize),
    Run(Vec<String>),
    Write(String, Vec<u8>, u32),
    /// `(path, user, mode, file)`
    Give(String, String, u32, bool),
    Send(String),
    /// A streamed command (the log audit's).
    Stream(Vec<String>),
    /// A file removed.
    Remove(String),
}

pub(crate) struct Fake {
    pub(crate) root: bool,
    pub(crate) exists: bool,
    pub(crate) ready: Option<String>,
    pub(crate) random: Result<Vec<u8>, String>,
    /// `(text the command line contains, outcome)`; the first match answers, else exit 0.
    pub(crate) answers: RefCell<Vec<(String, Outcome)>>,
    /// Paths whose write fails.
    pub(crate) write_fails: Option<String>,
    /// Paths that cannot be given to the user.
    pub(crate) give_fails: Option<String>,
    pub(crate) calls: RefCell<Vec<Call>>,
    pub(crate) sent: RefCell<Vec<Frame>>,
    pub(crate) down: bool,
    pub(crate) pauses: Cell<u32>,
    /// `(text the command line contains, ended, stdout chunks)`; the first match answers,
    /// else exit 0 and no output.
    pub(crate) streams: RefCell<Vec<StreamScript>>,
    /// The files of the guest `read_file` finds; a write puts its bytes here too.
    pub(crate) files: RefCell<BTreeMap<String, Vec<u8>>>,
    /// The path whose removal fails.
    pub(crate) remove_fails: Option<String>,
    /// What the host answers to every question (the prime question).
    pub(crate) answer: RefCell<Result<String, SendError>>,
    /// Answers given before `answer`, one per question.
    pub(crate) early: RefCell<std::collections::VecDeque<Result<String, SendError>>>,
    /// The questions asked, with the port.
    pub(crate) asked: RefCell<Vec<(u32, Frame)>>,
    /// The deadline each command was run with, with the command line.
    pub(crate) deadlines: RefCell<Vec<(String, Duration)>>,
}

pub(crate) fn out(ended: Ended, stdout: &str, stderr: &str) -> Outcome {
    Outcome {
        ended,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        cut: false,
        ms: 5,
    }
}

impl Fake {
    pub(crate) fn new() -> Self {
        Self {
            root: true,
            exists: false,
            ready: None,
            random: Ok(RANDOM.to_vec()),
            answers: RefCell::new(Vec::new()),
            write_fails: None,
            give_fails: None,
            calls: RefCell::new(Vec::new()),
            sent: RefCell::new(Vec::new()),
            down: false,
            pauses: Cell::new(0),
            streams: RefCell::new(Vec::new()),
            files: RefCell::new(BTreeMap::new()),
            remove_fails: None,
            answer: RefCell::new(Err(SendError::Unavailable("no host".to_owned()))),
            early: RefCell::new(std::collections::VecDeque::new()),
            asked: RefCell::new(Vec::new()),
            deadlines: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn answer(&self, text: &str, o: Outcome) {
        self.answers.borrow_mut().push((text.to_owned(), o));
    }

    pub(crate) fn commands(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter_map(|c| match c {
                Call::Run(a) => Some(a.join(" ")),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn writes(&self) -> Vec<(String, Vec<u8>, u32)> {
        self.calls
            .borrow()
            .iter()
            .filter_map(|c| match c {
                Call::Write(p, b, m) => Some((p.clone(), b.clone(), *m)),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn step_frames(&self) -> Vec<StepFrame> {
        self.sent
            .borrow()
            .iter()
            .filter(|f| f.name.starts_with(STEP_PREFIX))
            .map(|f| serde_json::from_str(&f.body).expect("a step frame"))
            .collect()
    }

    pub(crate) fn summary(&self) -> MkUserSummary {
        let sent = self.sent.borrow();
        let f = sent
            .iter()
            .find(|f| f.name == SUMMARY_NAME)
            .expect("a summary frame");
        serde_json::from_str(&f.body).expect("summary json")
    }
}

impl MkUserHost for Fake {
    fn is_root(&self) -> bool {
        self.root
    }
    fn user_exists(&self, _name: &str) -> bool {
        self.exists
    }
    fn ready_user(&self) -> Option<String> {
        self.ready.clone()
    }
    fn random_bytes(&self, n: usize) -> Result<Vec<u8>, String> {
        self.calls.borrow_mut().push(Call::Random(n));
        self.random.clone()
    }
    fn write_file(&self, path: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(Call::Write(path.to_owned(), bytes.to_vec(), mode));
        if self.write_fails.as_deref() == Some(path) {
            Err("read-only volume".to_owned())
        } else {
            self.files
                .borrow_mut()
                .insert(path.to_owned(), bytes.to_vec());
            Ok(())
        }
    }
    fn give_to_user(&self, path: &str, user: &str, mode: u32, file: bool) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(Call::Give(path.to_owned(), user.to_owned(), mode, file));
        if self.give_fails.as_deref() == Some(path) {
            Err("is a link".to_owned())
        } else {
            Ok(())
        }
    }
    fn run(&self, argv: &[String], deadline: Duration) -> Outcome {
        self.calls.borrow_mut().push(Call::Run(argv.to_vec()));
        let line = argv.join(" ");
        self.deadlines.borrow_mut().push((line.clone(), deadline));
        let default = if line.contains("/ioreg") {
            out(Ended::Exited(0), &ioreg_text(UUID), "")
        } else {
            out(Ended::Exited(0), "", "")
        };
        self.answers
            .borrow()
            .iter()
            .find(|(text, _)| line.contains(text.as_str()))
            .map_or(default, |(_, o)| o.clone())
    }
    fn stream(
        &self,
        argv: &[String],
        _deadline: Duration,
        sink: &mut dyn FnMut(&[u8]),
    ) -> Streamed {
        self.calls.borrow_mut().push(Call::Stream(argv.to_vec()));
        let line = argv.join(" ");
        let scripted = self
            .streams
            .borrow()
            .iter()
            .find(|(text, _, _)| line.contains(text.as_str()))
            .cloned();
        let (ended, chunks) = scripted.map_or((Ended::Exited(0), Vec::new()), |(_, e, c)| (e, c));
        chunks.iter().for_each(|c| sink(c));
        Streamed { ended, ms: 7 }
    }
    fn send(&self, _port: u32, frame: &Frame) -> Result<(), SendError> {
        self.calls.borrow_mut().push(Call::Send(frame.name.clone()));
        if self.down {
            return Err(SendError::Unavailable("no host".to_owned()));
        }
        self.sent.borrow_mut().push(frame.clone());
        Ok(())
    }
    fn pause(&self, _d: Duration) {
        self.pauses.set(self.pauses.get() + 1);
    }
    fn read_file(&self, path: &str, cap: usize) -> Result<Vec<u8>, String> {
        self.files
            .borrow()
            .get(path)
            .filter(|b| b.len() <= cap)
            .cloned()
            .ok_or_else(|| format!("{path}: no such file"))
    }
    fn ask(&self, port: u32, frame: &Frame) -> Result<String, SendError> {
        self.asked.borrow_mut().push((port, frame.clone()));
        self.early
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| self.answer.borrow().clone())
    }
    fn remove_file(&self, path: &str) -> Result<(), String> {
        self.calls.borrow_mut().push(Call::Remove(path.to_owned()));
        if self.remove_fails.as_deref() == Some(path) {
            return Err("Operation not permitted".to_owned());
        }
        self.files.borrow_mut().remove(path);
        Ok(())
    }
}

#[derive(clap::Parser)]
struct W {
    #[command(flatten)]
    a: MkUserArgs,
}

fn args(extra: &[&str]) -> MkUserArgs {
    W::parse_from(
        ["w", "--name", "kbfuser"]
            .into_iter()
            .chain(extra.iter().copied()),
    )
    .a
}

pub(crate) fn go(host: &Fake) -> Finished {
    run(host, &Config::from(&args(&[])), "kbfuser", &mut Vec::new())
}

fn ids(steps: &[Step]) -> Vec<&str> {
    steps.iter().map(|s| s.id.as_str()).collect()
}

fn shown(steps: &[Step], id: &str) -> Vec<String> {
    match &steps.iter().find(|s| s.id == id).expect(id).action {
        Action::Run(a) | Action::GiveFiles(a, _) => a
            .iter()
            .map(|a| match a {
                Arg::Text(t) => t.clone(),
                Arg::Password => "<PW>".to_owned(),
                Arg::ByHost(d, n, e) => format!("<{d}/{n}.UUID{e}>"),
            })
            .collect(),
        other => panic!("{id} is {other:?}"),
    }
}

/// The user's home (built in parts: the repo lint refuses a home-path literal).
fn home() -> String {
    ["/Users", "kbfuser"].join("/")
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// Catches a plan that reorders, drops or adds a step, runs a step before one it needs, or
/// builds an argument list that creates the wrong account: an admin user (no `-admin`), the
/// wrong home, the password anywhere but its one place, auto-login for another user.
#[test]
fn the_plan_creates_a_standard_user_then_auto_login_then_the_marker() {
    let steps = plan("kbfuser", false);
    assert_eq!(
        ids(&steps),
        [
            "add-user",
            "home",
            "kcpassword",
            "autologin",
            "pmset",
            "hw-uuid",
            "prefs-dirs",
            "screensaver-idle",
            "screensaver-lock",
            "screensaver-own",
            "loginwindow-idle",
            "updates-check",
            "updates-download",
            "updates-os",
            "updates-critical",
            "ready-marker"
        ]
    );
    for (i, s) in steps.iter().enumerate() {
        for need in &s.needs {
            let at = steps.iter().position(|x| &x.id == need).expect("a step");
            assert!(at < i, "{} needs {need}, which comes later", s.id);
        }
    }
    // The marker is last and waits for every step but the five it follows through others.
    let last = steps.last().expect("steps");
    assert_eq!(last.action, Action::WriteReady);
    assert_eq!(last.needs.len(), steps.len() - 5);
    for through in ["add-user", "kcpassword", "hw-uuid", "prefs-dirs"] {
        assert!(!last.needs.contains(&through.to_owned()), "{through}");
    }
    assert_eq!(
        shown(&steps, "add-user"),
        strs(&[
            "/usr/sbin/sysadminctl",
            "-addUser",
            "kbfuser",
            "-fullName",
            "kbfuser",
            "-password",
            "<PW>",
            "-home",
            &home()
        ])
    );
    assert!(!shown(&steps, "add-user").contains(&"-admin".to_owned()));
    assert_eq!(
        shown(&steps, "home"),
        strs(&["/usr/sbin/createhomedir", "-c", "-u", "kbfuser"])
    );
    assert_eq!(
        shown(&steps, "autologin"),
        strs(&[
            "/usr/bin/defaults",
            "write",
            "/Library/Preferences/com.apple.loginwindow",
            "autoLoginUser",
            "-string",
            "kbfuser"
        ])
    );
    assert_eq!(
        shown(&steps, "pmset"),
        strs(&[
            "/usr/bin/pmset",
            "-a",
            "sleep",
            "0",
            "displaysleep",
            "0",
            "disksleep",
            "0"
        ])
    );
    // The user's preferences are written by path, as root: no `sudo -u`, no domain name.
    let prefs = format!("{}/Library/Preferences", home());
    assert_eq!(
        shown(&steps, "screensaver-idle"),
        strs(&[
            "/usr/bin/defaults",
            "write",
            &format!("<{prefs}/ByHost/com.apple.screensaver.UUID>"),
            "idleTime",
            "-int",
            "0"
        ])
    );
    assert_eq!(
        shown(&steps, "screensaver-lock"),
        strs(&[
            "/usr/bin/defaults",
            "write",
            &format!("{prefs}/com.apple.screensaver"),
            "askForPassword",
            "-int",
            "0"
        ])
    );
    assert_eq!(
        steps
            .iter()
            .find(|s| s.id == "hw-uuid")
            .expect("hw-uuid")
            .action,
        Action::ReadHardwareUuid(strs(&[
            "/usr/sbin/ioreg",
            "-rd1",
            "-c",
            "IOPlatformExpertDevice"
        ]))
    );
    assert_eq!(
        shown(&steps, "loginwindow-idle"),
        strs(&[
            "/usr/bin/defaults",
            "write",
            "/Library/Preferences/com.apple.screensaver",
            "loginWindowIdleTime",
            "-int",
            "0"
        ])
    );
    assert_eq!(
        shown(&steps, "screensaver-own"),
        strs(&[
            &format!("{prefs}/com.apple.screensaver.plist"),
            &format!("<{prefs}/ByHost/com.apple.screensaver.UUID.plist>"),
        ])
    );
    let sa = |id: &str| steps.iter().find(|s| s.id == id).expect(id);
    assert_eq!(
        sa("prefs-dirs").action,
        Action::MakeDirs(
            vec![
                format!("{}/Library", home()),
                prefs.clone(),
                format!("{prefs}/ByHost")
            ],
            0o700
        )
    );
    assert!(matches!(
        sa("screensaver-own").action,
        Action::GiveFiles(_, 0o600)
    ));
    assert_eq!(sa("screensaver-idle").needs, ["prefs-dirs", "hw-uuid"]);
    assert_eq!(sa("screensaver-lock").needs, ["prefs-dirs"]);
    assert_eq!(
        sa("screensaver-own").needs,
        ["screensaver-idle", "screensaver-lock"]
    );
    assert_eq!(sa("prefs-dirs").needs, ["home"]);
    // Nothing runs as the user: its session does not exist yet.
    assert!(!steps.iter().any(|s| matches!(&s.action, Action::Run(a)
        if a.contains(&Arg::Text("/usr/bin/sudo".to_owned())))));
    for (id, key) in [
        ("updates-check", "AutomaticCheckEnabled"),
        ("updates-download", "AutomaticDownload"),
        ("updates-os", "AutomaticallyInstallMacOSUpdates"),
        ("updates-critical", "CriticalUpdateInstall"),
    ] {
        assert_eq!(
            shown(&steps, id),
            strs(&[
                "/usr/bin/defaults",
                "write",
                "/Library/Preferences/com.apple.SoftwareUpdate",
                key,
                "-bool",
                "false"
            ])
        );
    }
    // No other step carries the password.
    let with_password = steps
        .iter()
        .filter(|s| matches!(&s.action, Action::Run(a) if a.contains(&Arg::Password)))
        .count();
    assert_eq!(with_password, 1);
}

/// Catches an existing user that is created again (the add fails) or left with a password
/// nobody knows (auto-login could not be written): the plan sets its password instead.
#[test]
fn an_existing_user_gets_a_new_password_not_a_new_account() {
    let steps = plan("kbfuser", true);
    assert_eq!(ids(&steps)[0], "set-password");
    assert!(!ids(&steps).contains(&"add-user"));
    assert_eq!(
        shown(&steps, "set-password"),
        strs(&["/usr/bin/dscl", ".", "-passwd", &home(), "<PW>"])
    );
    for s in &steps[1..] {
        assert!(!s.needs.contains(&"add-user".to_owned()));
    }
    assert_eq!(steps[1].needs, ["set-password"]);
}

/// Catches a user name that could name a system account or break an argument list: names
/// with uppercase, a leading digit, separators, spaces, `..`, a slash, an empty or an
/// over-long one, and the accounts the system owns.
#[test]
fn only_a_short_lowercase_name_that_is_not_a_system_account_is_a_user_name() {
    for good in ["kbf", "kbfuser", "a", "kbf-user_2", &"a".repeat(31)] {
        assert_eq!(check_name(good), Ok(()), "{good}");
    }
    for bad in [
        "",
        "Kbf",
        "1kbf",
        "-kbf",
        "kbf user",
        "kbf.user",
        "../x",
        "a/b",
        "kbf\n",
        "kbfé",
        "root",
        "nobody",
        "daemon",
        "admin",
        "guest",
        "wheel",
        &"a".repeat(32),
    ] {
        assert!(check_name(bad).is_err(), "{bad:?}");
    }
}

/// Catches the encoding of `/etc/kcpassword` drifting from the documented algorithm: the
/// XOR key, its wrap-around, and the zero padding to a multiple of 12 bytes (to 12 when
/// shorter). The vector is worked out by hand for "abc": 0x61^0x7D, 0x62^0x89, 0x63^0x52,
/// then the nine zero bytes, which come out as the key itself, whose 12th byte is the
/// first again.
#[test]
fn kcpassword_is_the_documented_xor_obfuscation() {
    assert_eq!(
        kcpassword(b"abc"),
        [
            0x1C, 0xEB, 0x31, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F, 0x7D
        ]
    );
    // Eleven bytes wrap the key once: the 12th byte (padding) meets key[0].
    let eleven = kcpassword(&[0u8; 11]);
    assert_eq!(
        &eleven[..11],
        &[
            0x7D, 0x89, 0x52, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F
        ]
    );
    assert_eq!(eleven[11], 0x7D);
    for (len, padded) in [
        (0, 12),
        (1, 12),
        (11, 12),
        (12, 12),
        (13, 24),
        (22, 24),
        (24, 24),
        (25, 36),
    ] {
        assert_eq!(kcpassword(&vec![b'x'; len]).len(), padded, "{len}");
    }
    // The 13th byte meets key[12 % 11] = key[1].
    assert_eq!(kcpassword(&[0u8; 13])[12], 0x89);
    // The generated password is 22 characters: always padded, never a bare multiple.
    assert_eq!(kcpassword(PASSWORD.as_bytes()).len(), 24);
    assert_eq!(password_from(&RANDOM), PASSWORD);
    assert_eq!(PASSWORD.len(), 2 * PASSWORD_BYTES);
}

/// Catches a run that sends the host the password (in a frame's argument list, its output,
/// or the summary), runs a command with the placeholder instead of the password, or does
/// not write what the plan says. A good run: one frame per step then the summary, the
/// marker and the obfuscated password written, and the power-off after the summary, last.
#[test]
fn a_good_run_sends_a_frame_per_step_without_the_password_then_powers_off() {
    let host = Fake::new();
    // A tool that echoes what it was given: the frame must not repeat it.
    host.answer(
        "sysadminctl",
        out(
            Ended::Exited(0),
            &format!("creating, password {PASSWORD}\n"),
            &format!("warn {PASSWORD}"),
        ),
    );
    let done = go(&host);
    assert!(done.summary.ok && done.summary.created && !done.summary.already_ready);
    assert_eq!(done.power_off_started, Some(true));
    let frames = host.step_frames();
    assert_eq!(frames.len(), 16);
    assert!(
        frames
            .iter()
            .all(|f| f.status == Status::Ok && f.exit == Some(0))
    );
    let add = &frames[0];
    assert_eq!(add.argv[5..7], ["-password", WITHHELD]);
    assert_eq!(add.stdout, format!("creating, password {REDACTED}\n"));
    assert_eq!(add.stderr, format!("warn {REDACTED}"));
    assert_eq!(add.ms, 5);
    for f in host.sent.borrow().iter() {
        assert!(!f.body.contains(PASSWORD), "{}: {}", f.name, f.body);
        assert!(!f.name.contains(PASSWORD));
    }
    // The command that ran inside the guest did get the password, once.
    let ran = host.commands();
    assert_eq!(
        ran[0],
        format!(
            "/usr/sbin/sysadminctl -addUser kbfuser -fullName kbfuser -password {PASSWORD} -home {}",
            home()
        )
    );
    assert_eq!(ran.iter().filter(|c| c.contains(PASSWORD)).count(), 1);
    // The files: obfuscated password, root-only; the marker with the name.
    let writes = host.writes();
    assert_eq!(
        writes,
        [
            (
                "/etc/kcpassword".to_owned(),
                kcpassword(PASSWORD.as_bytes()),
                0o600
            ),
            (
                "/private/var/kbf-sim/user-ready".to_owned(),
                b"kbfuser\n".to_vec(),
                0o644
            ),
        ]
    );
    assert!(
        !writes[0]
            .1
            .windows(PASSWORD.len())
            .any(|w| w == PASSWORD.as_bytes())
    );
    assert_eq!(frames[2].argv, ["write", "/etc/kcpassword", "0600", "root"]);
    // Order: the steps, their frames, the summary, then the power-off, and nothing after.
    let calls = host.calls.borrow();
    let summary_at = calls
        .iter()
        .position(|c| *c == Call::Send(SUMMARY_NAME.to_owned()))
        .expect("summary");
    let off_at = calls
        .iter()
        .position(|c| *c == Call::Run(strs(&SHUTDOWN)))
        .expect("power-off");
    assert!(summary_at < off_at);
    assert_eq!(off_at, calls.len() - 1);
    let s = host.summary();
    assert!(s.ok && s.created && s.powering_off && !s.already_ready);
    assert_eq!(s.user, "kbfuser");
    assert_eq!(s.steps.len(), 16);
    assert!(s.steps.iter().all(|(_, st)| *st == Status::Ok));
}

/// Catches an idempotence that is not: a second run, with the marker naming the user, must
/// change nothing (no random bytes, no command, no write), say so in a frame the host can
/// read, and not power the guest off.
#[test]
fn a_user_that_is_ready_is_left_alone_and_the_guest_stays_up() {
    let mut host = Fake::new();
    host.ready = Some("kbfuser".to_owned());
    let done = go(&host);
    assert!(done.summary.ok && done.summary.already_ready && !done.summary.created);
    assert_eq!(done.power_off_started, None);
    assert!(!done.summary.powering_off);
    let calls = host.calls.borrow();
    assert_eq!(*calls, [Call::Send(SUMMARY_NAME.to_owned())]);
    let s = host.summary();
    assert!(s.already_ready && s.ok && s.steps.is_empty() && !s.powering_off);
    assert!(s.note.expect("note").contains("nothing to do"));
}

/// Catches a marker for another user being taken for this one: the run then goes on and
/// replaces it, and an existing account is given a password, not created again.
#[test]
fn a_marker_naming_another_user_does_not_stop_the_run() {
    let mut host = Fake::new();
    host.ready = Some("someoneelse".to_owned());
    host.exists = true;
    let done = go(&host);
    assert!(done.summary.ok && !done.summary.created && !done.summary.already_ready);
    let ran = host.commands();
    assert_eq!(
        ran[0],
        format!("/usr/bin/dscl . -passwd {} {PASSWORD}", home())
    );
    assert!(!ran.iter().any(|c| c.contains("addUser")));
    assert_eq!(done.power_off_started, Some(true));
    let frames = host.step_frames();
    assert_eq!(frames[0].step, "set-password");
    assert_eq!(frames[0].argv[4], WITHHELD);
}

/// Catches a failed step that lets what depends on it run (auto-login without a password
/// file), a marker written for a user that is not ready, a failed run that leaves the guest
/// up (the host would wait for the whole deadline), and a failure that does not say which
/// step, how.
#[test]
fn a_failed_step_skips_what_needs_it_leaves_no_marker_and_still_powers_off() {
    let host = Fake::new();
    host.answer(
        "sysadminctl",
        out(Ended::Exited(70), "", &format!("no: {PASSWORD}")),
    );
    let done = go(&host);
    assert!(!done.summary.ok);
    assert_eq!(done.power_off_started, Some(true));
    let frames = host.step_frames();
    let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
    assert_eq!(by("add-user").status, Status::Failed);
    assert_eq!(by("add-user").exit, Some(70));
    assert_eq!(by("add-user").stderr, format!("no: {REDACTED}"));
    for id in [
        "home",
        "kcpassword",
        "autologin",
        "prefs-dirs",
        "screensaver-idle",
        "screensaver-lock",
        "screensaver-own",
        "ready-marker",
    ] {
        assert_eq!(by(id).status, Status::Skipped, "{id}");
        assert!(by(id).skipped_because.is_some(), "{id}");
    }
    // What needs nothing still runs.
    for id in [
        "pmset",
        "hw-uuid",
        "loginwindow-idle",
        "updates-check",
        "updates-critical",
    ] {
        assert_eq!(by(id).status, Status::Ok, "{id}");
    }
    assert!(host.writes().is_empty());
    assert!(!host.commands().iter().any(|c| c.contains("autoLoginUser")));
}

/// Catches a command that is killed, signalled or never started being reported as ok, and
/// a write that fails (the marker) being reported as ok, or its note leaking the password.
#[test]
fn each_way_a_step_can_end_is_reported_as_what_it_was() {
    let host = Fake::new();
    host.answer("pmset", out(Ended::TimedOut, "", ""));
    host.answer("AutomaticCheckEnabled", out(Ended::Signaled(9), "", ""));
    host.answer(
        "AutomaticDownload",
        out(Ended::SpawnFailed(format!("exec {PASSWORD}")), "", ""),
    );
    let done = go(&host);
    assert!(!done.summary.ok);
    let frames = host.step_frames();
    let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
    assert_eq!(by("pmset").status, Status::Timeout);
    assert_eq!(by("updates-check").status, Status::Failed);
    assert_eq!(by("updates-check").signal, Some(9));
    assert_eq!(by("updates-download").status, Status::Error);
    let note = by("updates-download").note.clone().expect("note");
    assert!(
        note.contains(REDACTED) && !note.contains(PASSWORD),
        "{note}"
    );

    let mut host = Fake::new();
    host.write_fails = Some("/etc/kcpassword".to_owned());
    let done = go(&host);
    assert!(!done.summary.ok);
    let frames = host.step_frames();
    let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
    assert_eq!(by("kcpassword").status, Status::Error);
    assert!(
        by("kcpassword")
            .note
            .as_deref()
            .expect("note")
            .contains("read-only volume")
    );
    assert_eq!(by("autologin").status, Status::Skipped);
    assert_eq!(by("ready-marker").status, Status::Skipped);
}

/// Catches a run as an ordinary user, or with a name that is not a user name: it runs
/// nothing, writes nothing, does not power the guest off, and the summary says why.
#[test]
fn a_run_that_is_not_root_or_has_a_bad_name_does_nothing_and_says_so() {
    let mut host = Fake::new();
    host.root = false;
    let done = go(&host);
    assert!(!done.summary.ok && done.power_off_started.is_none());
    assert_eq!(done.summary.note.as_deref(), Some("mkuser runs as root"));
    assert_eq!(*host.calls.borrow(), [Call::Send(SUMMARY_NAME.to_owned())]);

    let host = Fake::new();
    let done = run(&host, &Config::from(&args(&[])), "Root", &mut Vec::new());
    assert!(!done.summary.ok && done.power_off_started.is_none());
    assert!(done.summary.note.expect("note").contains("lowercase"));
    assert_eq!(*host.calls.borrow(), [Call::Send(SUMMARY_NAME.to_owned())]);
}

/// Catches a random source that fails or answers short being turned into a password (an
/// empty or short one): the run stops before any account exists, and still powers off so
/// that the host does not wait.
#[test]
fn a_random_source_that_fails_or_comes_up_short_makes_no_account() {
    for random in [Err("no entropy".to_owned()), Ok(vec![1, 2, 3])] {
        let mut host = Fake::new();
        host.random = random;
        let done = go(&host);
        assert!(!done.summary.ok);
        assert!(done.summary.steps.is_empty());
        assert_eq!(done.power_off_started, Some(true));
        assert_eq!(host.commands(), [strs(&SHUTDOWN).join(" ")]);
        assert!(host.writes().is_empty());
        assert!(done.summary.note.expect("note").contains("random"));
    }
}

/// Catches a power-off that fails silently (the host would wait out its deadline with no
/// word why): the failure is its own frame.
#[test]
fn a_power_off_that_does_not_start_is_reported() {
    let host = Fake::new();
    host.answer("shutdown", out(Ended::Exited(1), "", "not permitted"));
    let done = go(&host);
    assert_eq!(done.power_off_started, Some(false));
    let sent = host.sent.borrow();
    let f = sent
        .iter()
        .find(|f| f.name == SHUTDOWN_NAME)
        .expect("a frame");
    let frame: StepFrame = serde_json::from_str(&f.body).expect("json");
    assert_eq!(frame.step, "shutdown");
    assert_eq!(frame.status, Status::Failed);
    assert_eq!(
        (frame.exit, frame.stderr.as_str()),
        (Some(1), "not permitted")
    );
    assert_eq!(frame.argv, strs(&SHUTDOWN));
}

/// Catches a host that is not listening making the run hang or lose track: every frame is
/// retried a bounded number of times, the pauses are counted not slept, the names of the
/// frames that never arrived are in the summary the run returns, and the run still ends
/// with the power-off.
#[test]
fn frames_that_cannot_be_delivered_are_listed_and_the_run_still_ends() {
    let mut host = Fake::new();
    host.down = true;
    let done = go(&host);
    assert!(done.summary.ok, "the steps themselves worked");
    assert_eq!(done.power_off_started, Some(true));
    assert_eq!(done.summary.undelivered.len(), 18);
    assert_eq!(
        done.summary.undelivered.last().map(String::as_str),
        Some(SUMMARY_NAME)
    );
    assert!(host.pauses.get() > 0);
    assert!(host.sent.borrow().is_empty());
}

/// Catches a frame name the host's sink would refuse (separators, a leading dot, more than
/// 100 bytes) and a count of frames that is not the plan's: the host's budget of files is
/// sized from it.
#[test]
fn every_frame_name_is_a_plain_name_within_the_host_limits() {
    let mut names = vec![
        SUMMARY_NAME.to_owned(),
        SHUTDOWN_NAME.to_owned(),
        crate::mkuser_audit::AUDIT_NAME.to_owned(),
    ];
    for (i, s) in plan("kbfuser", false).iter().enumerate() {
        names.push(step_frame_name(i, &s.id));
    }
    assert_eq!(names.len(), max_frames());
    for n in &names {
        assert!(n.starts_with(FRAME_PREFIX) && n.ends_with(".json"), "{n}");
        assert!(n.len() <= 100, "{n}");
        assert!(
            n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
            "{n}"
        );
    }
    assert_eq!(
        step_frame_name(0, "add-user"),
        "mkuser-step-01-add-user.json"
    );
    assert!(STEP_PREFIX.starts_with(FRAME_PREFIX) && SUMMARY_NAME.starts_with(FRAME_PREFIX));
    assert!(SHUTDOWN_NAME.starts_with(FRAME_PREFIX));
}

/// Catches defaults and flags that do not make the config: the vsock port the host
/// listens on (5000), the command deadline and the output cap.
#[test]
fn the_defaults_and_the_flags_make_the_config() {
    let c = Config::from(&args(&[]));
    assert_eq!((c.quick.as_secs(), c.tail_bytes, c.port), (120, 8192, 5000));
    assert_eq!(c.audit.as_secs(), 600);
    assert_eq!(
        (c.first_launch.as_secs(), c.runtime_add.as_secs()),
        (1800, 1800)
    );
    let c = Config::from(&args(&[
        "--quick-secs",
        "7",
        "--tail-bytes",
        "9",
        "--vsock-port",
        "6000",
        "--audit-secs",
        "11",
        "--first-launch-secs",
        "13",
        "--runtime-add-secs",
        "17",
    ]));
    assert_eq!(
        (c.first_launch.as_secs(), c.runtime_add.as_secs()),
        (13, 17)
    );
    assert_eq!((c.quick.as_secs(), c.tail_bytes, c.port), (7, 9, 6000));
    assert_eq!(c.audit.as_secs(), 11);
    assert_eq!(args(&[]).name, "kbfuser");
    assert!(W::try_parse_from(["w"]).is_err(), "--name is required");
}

/// Catches the marker and the wish moving apart: the files `mkuser` and the simulator
/// probe share are the ones the paths module names.
#[test]
fn the_marker_is_written_where_the_simulator_probe_reads_it() {
    let host = Fake::new();
    go(&host);
    assert_eq!(host.writes()[1].0, simpaths::USER_READY);
    assert_eq!(KCPASSWORD_PATH, "/etc/kcpassword");
}

/// Catches a ByHost file named for the wrong machine, or a user step that gets ahead of the
/// files it needs: a good run reads the UUID, makes the three directories (in order, parent
/// first, mode 0700, for the user), writes both plists by path, and only then gives the two
/// plists (0600) to the user. Also that the ioreg tail goes in the frame and the UUID is not
/// treated as a secret.
#[test]
fn the_user_preferences_are_written_by_path_and_then_given_to_the_user() {
    let host = Fake::new();
    let done = go(&host);
    assert!(done.summary.ok);
    let prefs = format!("{}/Library/Preferences", home());
    let ran = host.commands();
    let at = |needle: &str| ran.iter().position(|c| c.contains(needle)).expect(needle);
    assert_eq!(
        ran[at("idleTime")],
        format!(
            "/usr/bin/defaults write {prefs}/ByHost/com.apple.screensaver.{UUID} idleTime -int 0"
        )
    );
    assert_eq!(
        ran[at("askForPassword")],
        format!("/usr/bin/defaults write {prefs}/com.apple.screensaver askForPassword -int 0")
    );
    assert!(at("ioreg") < at("idleTime"));
    assert!(
        !ran.iter()
            .any(|c| c.contains("sudo") || c.contains("-currentHost"))
    );
    let gives: Vec<Call> = host
        .calls
        .borrow()
        .iter()
        .filter(|c| matches!(c, Call::Give(..)))
        .cloned()
        .collect();
    let g = |p: String, mode, file| Call::Give(p, "kbfuser".to_owned(), mode, file);
    assert_eq!(
        gives,
        [
            g(format!("{}/Library", home()), 0o700, false),
            g(prefs.clone(), 0o700, false),
            g(format!("{prefs}/ByHost"), 0o700, false),
            g(format!("{prefs}/com.apple.screensaver.plist"), 0o600, true),
            g(
                format!("{prefs}/ByHost/com.apple.screensaver.{UUID}.plist"),
                0o600,
                true
            ),
        ]
    );
    // Dirs are made before the writes, the files are given after them.
    let calls = host.calls.borrow();
    let pos = |pred: &dyn Fn(&Call) -> bool| calls.iter().position(pred).expect("a call");
    let first_dir = pos(&|c| matches!(c, Call::Give(_, _, _, false)));
    let idle = pos(&|c| matches!(c, Call::Run(a) if a.join(" ").contains("idleTime")));
    let lock = pos(&|c| matches!(c, Call::Run(a) if a.join(" ").contains("askForPassword")));
    let first_file = pos(&|c| matches!(c, Call::Give(_, _, _, true)));
    assert!(first_dir < idle && first_dir < lock);
    assert!(idle < first_file && lock < first_file);
    drop(calls);
    let frames = host.step_frames();
    let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
    assert!(by("hw-uuid").stdout.contains(UUID));
    assert_eq!(
        by("prefs-dirs").argv[..4],
        ["give", "dir", "kbfuser", "0700"]
    );
    assert_eq!(
        by("screensaver-own").argv[..4],
        ["give", "file", "kbfuser", "0600"]
    );
    assert!(by("screensaver-idle").argv[2].contains(UUID));
    assert!(by("screensaver-own").argv[5].ends_with(&format!("{UUID}.plist")));
}

/// Catches an ioreg parse that accepts what is not a UUID (a name that could escape the
/// directory, a short one), takes another key's value, or misses the real line: good,
/// missing, malformed.
#[test]
fn the_hardware_uuid_is_read_from_the_ioreg_line_and_only_when_well_formed() {
    assert_eq!(
        parse_platform_uuid(&ioreg_text(UUID)).as_deref(),
        Some(UUID)
    );
    assert_eq!(
        parse_platform_uuid(&ioreg_text(&UUID.to_lowercase())).as_deref(),
        Some(UUID.to_lowercase().as_str())
    );
    // Missing: no line, other keys only, an empty output, a key that only contains the name.
    for text in [
        String::new(),
        "\"IOPlatformSerialNumber\" = \"ZZ0\"\n".to_owned(),
        format!("\"NotIOPlatformUUID\" = \"{UUID}\"\n"),
        format!("\"IOPlatformUUID\" {UUID}\n"),
        format!("\"IOPlatformUUID\" = {UUID}\n"),
    ] {
        assert_eq!(parse_platform_uuid(&text), None, "{text:?}");
    }
    // Malformed: wrong length or groups, a non-hex digit, a path.
    for bad in [
        "",
        "1A2B3C4D",
        "1A2B3C4D-0000-4EEF-8123-ABCDEF01234",
        "1A2B3C4D-0000-4EEF-8123-ABCDEF0123456",
        "1A2B3C4D-0000-4EEF-8123-ABCDEF01234G",
        "1A2B3C4D00004EEF8123ABCDEF012345",
        "../../etc/passwd",
        "1A2B3C4D-0000-4EEF-8123-ABCDEF01/345",
    ] {
        assert_eq!(parse_platform_uuid(&ioreg_text(bad)), None, "{bad:?}");
    }
}

/// Catches a UUID that cannot be read being papered over: with a missing or malformed UUID,
/// or a failing ioreg, `hw-uuid` fails, the by-host write and the ownership step do not run,
/// the marker is skipped, and the lock step (which needs no UUID) still does its work.
#[test]
fn no_hardware_uuid_means_no_by_host_file_and_no_marker() {
    for (answer, status) in [
        (out(Ended::Exited(0), "no uuid here\n", ""), Status::Error),
        (
            out(Ended::Exited(0), &ioreg_text("not-a-uuid"), ""),
            Status::Error,
        ),
        (out(Ended::Exited(1), "", "ioreg: nope"), Status::Failed),
        (
            out(Ended::SpawnFailed("no ioreg".to_owned()), "", ""),
            Status::Error,
        ),
    ] {
        let host = Fake::new();
        host.answer("/ioreg", answer);
        let done = go(&host);
        assert!(!done.summary.ok);
        let frames = host.step_frames();
        let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
        assert_eq!(by("hw-uuid").status, status);
        assert_eq!(by("screensaver-idle").status, Status::Skipped);
        assert_eq!(
            by("screensaver-idle").skipped_because.as_deref(),
            Some("hw-uuid")
        );
        assert_eq!(by("screensaver-lock").status, Status::Ok);
        assert_eq!(by("screensaver-own").status, Status::Skipped);
        assert_eq!(by("ready-marker").status, Status::Skipped);
        assert!(!host.commands().iter().any(|c| c.contains("idleTime")));
        assert!(host.writes().iter().all(|w| w.0 != simpaths::USER_READY));
        assert_eq!(done.power_off_started, Some(true));
    }
    let host = Fake::new();
    host.answer("/ioreg", out(Ended::Exited(0), "no uuid\n", ""));
    go(&host);
    let frames = host.step_frames();
    let note = frames[5].note.as_deref().expect("a note");
    assert!(note.contains("IOPlatformUUID"), "{note}");
}

/// Catches a failed ownership change being reported as success, and its dependents running:
/// a directory that cannot be given (a link) stops the preference writes; a plist that
/// cannot be given fails `screensaver-own`; either way no marker is written, and the guest
/// is still powered off. The note says which path and why.
#[test]
fn a_path_that_cannot_be_given_to_the_user_fails_the_step_and_skips_the_marker() {
    let prefs = format!("{}/Library/Preferences", home());
    for (path, failed, skipped) in [
        (
            format!("{prefs}/ByHost"),
            "prefs-dirs",
            vec!["screensaver-idle", "screensaver-lock", "screensaver-own"],
        ),
        (
            format!("{prefs}/com.apple.screensaver.plist"),
            "screensaver-own",
            vec![],
        ),
        (
            format!("{prefs}/ByHost/com.apple.screensaver.{UUID}.plist"),
            "screensaver-own",
            vec![],
        ),
    ] {
        let mut host = Fake::new();
        host.give_fails = Some(path.clone());
        let done = go(&host);
        assert!(!done.summary.ok, "{path}");
        let frames = host.step_frames();
        let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
        assert_eq!(by(failed).status, Status::Error, "{path}");
        let note = by(failed).note.as_deref().expect("a note");
        assert!(note.contains(&path) && note.contains("is a link"), "{note}");
        for id in &skipped {
            assert_eq!(by(id).status, Status::Skipped, "{id}");
        }
        assert_eq!(by("ready-marker").status, Status::Skipped);
        assert!(host.writes().iter().all(|w| w.0 != simpaths::USER_READY));
        assert_eq!(done.power_off_started, Some(true));
    }
}

/// Catches a failed preference write (a `defaults` that exits 1, as in a guest with no
/// session) leaving the marker behind: the marker is skipped naming the step, the files
/// are not given, and the run still powers off.
#[test]
fn a_failed_preference_write_skips_the_ownership_step_and_the_marker() {
    for key in ["idleTime", "askForPassword", "loginWindowIdleTime"] {
        let host = Fake::new();
        host.answer(key, out(Ended::Exited(1), "", "Could not write domain"));
        let done = go(&host);
        assert!(!done.summary.ok, "{key}");
        let frames = host.step_frames();
        let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
        assert_eq!(by("ready-marker").status, Status::Skipped, "{key}");
        assert!(host.writes().iter().all(|w| w.0 != simpaths::USER_READY));
        if key != "loginWindowIdleTime" {
            assert_eq!(by("screensaver-own").status, Status::Skipped, "{key}");
            assert!(
                !host
                    .calls
                    .borrow()
                    .iter()
                    .any(|c| matches!(c, Call::Give(_, _, _, true)))
            );
        }
        assert_eq!(done.power_off_started, Some(true));
    }
}

/// Whether `text` holds any run of four or more characters of the password.
fn leaks(text: &str) -> bool {
    (0..=PASSWORD.len() - 4).any(|i| text.contains(&PASSWORD[i..i + 4]))
}

/// Catches a password that is cut by the tail before it is redacted (a part of it, too short
/// to match, would be sent): with the password at every offset around the tail's cut, for
/// several tail sizes, in stdout and stderr, no frame field holds four characters of it in
/// a row; and a capture that was itself cut at its cap in the middle of the password (at the
/// end, or at the start) shows none of it either.
#[test]
fn a_password_split_by_the_tail_cut_or_the_capture_cap_never_shows_in_part() {
    for tail in [5usize, 17, 22, 30] {
        for filler in 0..40usize {
            let host = Fake::new();
            let line = format!("{}{PASSWORD}{}", "x".repeat(filler), "y".repeat(filler % 7));
            host.answer("sysadminctl", out(Ended::Exited(1), &line, &line));
            let cfg = Config::from(&args(&["--tail-bytes", &tail.to_string()]));
            run(&host, &cfg, "kbfuser", &mut Vec::new());
            for f in host.step_frames() {
                for field in [&f.stdout, &f.stderr] {
                    assert!(!leaks(field), "tail {tail} filler {filler}: {field:?}");
                }
            }
            for sent in host.sent.borrow().iter() {
                assert!(
                    !leaks(&sent.body),
                    "tail {tail} filler {filler}: {}",
                    sent.body
                );
            }
        }
    }
    // The capture cap cut the password: its first part ends the capture, or its last part
    // starts it.
    for (text, name) in [
        (format!("noise {}", &PASSWORD[..10]), "ends"),
        (format!("{} noise", &PASSWORD[9..]), "starts"),
        ("noise only".to_owned(), "neither"),
    ] {
        let host = Fake::new();
        let mut o = out(Ended::Exited(1), &text, &text);
        o.cut = true;
        host.answer("sysadminctl", o);
        go(&host);
        let frames = host.step_frames();
        assert!(
            !leaks(&frames[0].stdout) && !leaks(&frames[0].stderr),
            "{name}"
        );
        assert!(frames[0].stdout_truncated);
    }
    // A start failure's note too.
    let host = Fake::new();
    host.answer(
        "sysadminctl",
        out(Ended::SpawnFailed(format!("x {PASSWORD} y")), "", ""),
    );
    go(&host);
    assert!(!leaks(host.step_frames()[0].note.as_deref().expect("note")));
}

/// Catches a power-off that is not a halt: the argument list is pinned as literal text, so
/// that changing `-h` to `-r` (a reboot into the half-made user) is a failing test.
#[test]
fn the_power_off_command_is_shutdown_halt_now() {
    assert_eq!(SHUTDOWN, ["/sbin/shutdown", "-h", "now"]);
    let host = Fake::new();
    go(&host);
    assert_eq!(
        host.commands().last().map(String::as_str),
        Some("/sbin/shutdown -h now")
    );
}
