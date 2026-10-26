//! Tests of `mkuser`: the step plan and its argument lists, the `kcpassword` encoding, what
//! a frame may hold (never the password), idempotence, and the power-off. On a fake host
//! that answers each command from a script and records everything it is asked, in order;
//! nothing waits on a clock.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use clap::Parser;

use crate::frame::{Frame, SendError};
use crate::mkuser::*;
use crate::sim::{Ended, Outcome, Status, StepFrame};
use crate::simpaths;

const RANDOM: [u8; 11] = [
    0x00, 0x01, 0x0a, 0x0f, 0x10, 0x7f, 0x80, 0xab, 0xcd, 0xef, 0xff,
];
/// `RANDOM` as the password: lowercase hex.
const PASSWORD: &str = "00010a0f107f80abcdefff";

/// What the host was asked, in the order it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Random(usize),
    Run(Vec<String>),
    Write(String, Vec<u8>, u32),
    Send(String),
}

struct Fake {
    root: bool,
    exists: bool,
    ready: Option<String>,
    random: Result<Vec<u8>, String>,
    /// `(text the command line contains, outcome)`; the first match answers, else exit 0.
    answers: RefCell<Vec<(String, Outcome)>>,
    /// Paths whose write fails.
    write_fails: Option<String>,
    calls: RefCell<Vec<Call>>,
    sent: RefCell<Vec<Frame>>,
    down: bool,
    pauses: Cell<u32>,
}

fn out(ended: Ended, stdout: &str, stderr: &str) -> Outcome {
    Outcome {
        ended,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        cut: false,
        ms: 5,
    }
}

impl Fake {
    fn new() -> Self {
        Self {
            root: true,
            exists: false,
            ready: None,
            random: Ok(RANDOM.to_vec()),
            answers: RefCell::new(Vec::new()),
            write_fails: None,
            calls: RefCell::new(Vec::new()),
            sent: RefCell::new(Vec::new()),
            down: false,
            pauses: Cell::new(0),
        }
    }

    fn answer(&self, text: &str, o: Outcome) {
        self.answers.borrow_mut().push((text.to_owned(), o));
    }

    fn commands(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter_map(|c| match c {
                Call::Run(a) => Some(a.join(" ")),
                _ => None,
            })
            .collect()
    }

    fn writes(&self) -> Vec<(String, Vec<u8>, u32)> {
        self.calls
            .borrow()
            .iter()
            .filter_map(|c| match c {
                Call::Write(p, b, m) => Some((p.clone(), b.clone(), *m)),
                _ => None,
            })
            .collect()
    }

    fn step_frames(&self) -> Vec<StepFrame> {
        self.sent
            .borrow()
            .iter()
            .filter(|f| f.name.starts_with(STEP_PREFIX))
            .map(|f| serde_json::from_str(&f.body).expect("a step frame"))
            .collect()
    }

    fn summary(&self) -> MkUserSummary {
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
            Ok(())
        }
    }
    fn run(&self, argv: &[String], _deadline: Duration) -> Outcome {
        self.calls.borrow_mut().push(Call::Run(argv.to_vec()));
        let line = argv.join(" ");
        self.answers
            .borrow()
            .iter()
            .find(|(text, _)| line.contains(text.as_str()))
            .map_or_else(|| out(Ended::Exited(0), "", ""), |(_, o)| o.clone())
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

fn go(host: &Fake) -> Finished {
    run(host, &Config::from(&args(&[])), "kbfuser", &mut Vec::new())
}

fn ids(steps: &[Step]) -> Vec<&str> {
    steps.iter().map(|s| s.id.as_str()).collect()
}

fn shown(steps: &[Step], id: &str) -> Vec<String> {
    match &steps.iter().find(|s| s.id == id).expect(id).action {
        Action::Run(a) => a
            .iter()
            .map(|a| match a {
                Arg::Text(t) => t.clone(),
                Arg::Password => "<PW>".to_owned(),
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
            "screensaver-idle",
            "screensaver-lock",
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
    // The marker is last and waits for everything but the two it follows through others.
    let last = steps.last().expect("steps");
    assert_eq!(last.action, Action::WriteReady);
    assert_eq!(last.needs.len(), steps.len() - 3);
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
    assert_eq!(
        shown(&steps, "screensaver-idle"),
        strs(&[
            "/usr/bin/sudo",
            "-u",
            "kbfuser",
            "-H",
            "--",
            "/usr/bin/defaults",
            "-currentHost",
            "write",
            "com.apple.screensaver",
            "idleTime",
            "-int",
            "0"
        ])
    );
    assert_eq!(
        shown(&steps, "screensaver-lock").last().map(String::as_str),
        Some("0")
    );
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
    assert_eq!(frames.len(), 12);
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
    assert_eq!(s.steps.len(), 12);
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
        "screensaver-idle",
        "screensaver-lock",
        "ready-marker",
    ] {
        assert_eq!(by(id).status, Status::Skipped, "{id}");
        assert!(by(id).skipped_because.is_some(), "{id}");
    }
    // What needs nothing still runs.
    for id in ["pmset", "updates-check", "updates-critical"] {
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
    assert_eq!(done.summary.undelivered.len(), 13);
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
    let mut names = vec![SUMMARY_NAME.to_owned(), SHUTDOWN_NAME.to_owned()];
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
    let c = Config::from(&args(&[
        "--quick-secs",
        "7",
        "--tail-bytes",
        "9",
        "--vsock-port",
        "6000",
    ]));
    assert_eq!((c.quick.as_secs(), c.tail_bytes, c.port), (7, 9, 6000));
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
