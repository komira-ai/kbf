//! Tests of `sim` and `sim_run`: the step plan, skip propagation, the frames and their
//! sizes, on a fake host that answers each command from a script. None of them waits on a
//! clock: deadlines are values the fake is handed and records, and the one runner test that
//! needs a deadline to pass uses a clock that moves time forward by itself.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use clap::Parser;

use crate::frame::{Frame, Kind, MAX_BODY_BYTES, MAX_FRAME_BYTES, SendError, encode};
use crate::sim::*;
use crate::sim_run::{Clock, ProcessRunner, read_regular};

const UDID: &str = "6F3A1B2C-0D4E-4F5A-8B6C-7D8E9F0A1B2C";

const RUNTIMES: &str = r#"{"runtimes":[
 {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-26-4","version":"26.4","isAvailable":true,"name":"iOS 26.4"},
 {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-27-0","version":"27.0","isAvailable":true,"name":"iOS 27.0"},
 {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-28-0","version":"28.0","isAvailable":false,"name":"iOS 28.0"},
 {"identifier":"com.apple.CoreSimulator.SimRuntime.watchOS-27-0","version":"27.0","isAvailable":true,"name":"watchOS 27.0"}
]}"#;

const DEVICETYPES: &str = r#"{"devicetypes":[
 {"identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-15","name":"iPhone 15"},
 {"identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-16","name":"iPhone 16"},
 {"identifier":"com.apple.CoreSimulator.SimDeviceType.Apple-Watch-10","name":"Apple Watch 10"}
]}"#;

/// `simctl list runtimes -j` as Xcode 26/27 print it: each runtime lists the device types it
/// can create, oldest first. iOS 27.0 supports no iPhone older than the iPhone 12; the
/// unavailable iOS 28.0 entry supports a device the available one does not.
const RUNTIMES_WITH_SUPPORT: &str = r#"{"runtimes":[
 {"bundlePath":"/Library/Developer/CoreSimulator/Volumes/iOS_24A434/Library/Developer/CoreSimulator/Profiles/Runtimes/iOS 27.0.simruntime",
  "buildversion":"24A434","platform":"iOS","runtimeRoot":"/x","identifier":"com.apple.CoreSimulator.SimRuntime.iOS-27-0",
  "version":"27.0","isInternal":false,"isAvailable":true,"name":"iOS 27.0","supportedArchitectures":["arm64"],
  "supportedDeviceTypes":[
   {"bundlePath":"/d/iPhone 12.simdevicetype","name":"iPhone 12","identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-12","productFamily":"iPhone"},
   {"bundlePath":"/d/iPhone 17 Pro.simdevicetype","name":"iPhone 17 Pro","identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro","productFamily":"iPhone"},
   {"bundlePath":"/d/iPad Air.simdevicetype","name":"iPad Air 13-inch (M3)","identifier":"com.apple.CoreSimulator.SimDeviceType.iPad-Air-13-inch-M3","productFamily":"iPad"}]},
 {"bundlePath":"/y","buildversion":"25A1","platform":"iOS","identifier":"com.apple.CoreSimulator.SimRuntime.iOS-28-0",
  "version":"28.0","isAvailable":false,"name":"iOS 28.0",
  "supportedDeviceTypes":[
   {"bundlePath":"/d/iPhone 99.simdevicetype","name":"iPhone 99","identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-99","productFamily":"iPhone"}]}
]}"#;

/// The real-guest listing whose last iPhone the iOS 27 runtime refuses to create.
const DEVICETYPES_WITH_OLD_LAST: &str = r#"{"devicetypes":[
 {"identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro","name":"iPhone 17 Pro","productFamily":"iPhone"},
 {"identifier":"com.apple.CoreSimulator.SimDeviceType.iPhone-6s-Plus","name":"iPhone 6s Plus","productFamily":"iPhone"},
 {"identifier":"com.apple.CoreSimulator.SimDeviceType.Apple-Watch-10","name":"Apple Watch 10","productFamily":"Apple Watch"}
]}"#;

fn ok_out(stdout: &str) -> Outcome {
    Outcome {
        ended: Ended::Exited(0),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
        cut: false,
        ms: 7,
    }
}

fn ended(ended: Ended) -> Outcome {
    Outcome {
        ended,
        stdout: Vec::new(),
        stderr: b"it said no".to_vec(),
        cut: false,
        ms: 9,
    }
}

/// A host that answers each command from a script and records everything it is asked.
struct Fake {
    root: bool,
    runtimes: Vec<String>,
    apps: Vec<String>,
    /// The console user appears once this many pauses have been made; never when `None`.
    user_after_pauses: Option<u32>,
    /// `(text the command line contains, outcome)`; the first match answers, else exit 0.
    answers: RefCell<Vec<(String, Outcome)>>,
    ran: RefCell<Vec<(Vec<String>, Duration)>>,
    sent: RefCell<Vec<Frame>>,
    attempts: Cell<u32>,
    /// Every send fails like a host that is not listening.
    down: bool,
    /// Frames with this name are refused by the host.
    refuse: Option<String>,
    pauses: Cell<u32>,
    screenshot: RefCell<Result<Vec<u8>, String>>,
    screenshot_cap: Cell<usize>,
    out_dir_made: Cell<bool>,
    out_dir_fails: bool,
}

impl Fake {
    fn new() -> Self {
        let answers = vec![
            ("list runtimes".to_owned(), ok_out(RUNTIMES)),
            ("list devicetypes".to_owned(), ok_out(DEVICETYPES)),
            ("simctl create".to_owned(), ok_out(&format!("{UDID}\n"))),
            ("plutil".to_owned(), ok_out("com.example.App\n")),
        ];
        Self {
            root: true,
            runtimes: vec!["iOS_27.dmg".to_owned()],
            apps: Vec::new(),
            user_after_pauses: Some(0),
            answers: RefCell::new(answers),
            ran: RefCell::new(Vec::new()),
            sent: RefCell::new(Vec::new()),
            attempts: Cell::new(0),
            down: false,
            refuse: None,
            pauses: Cell::new(0),
            screenshot: RefCell::new(Ok(vec![0xFF, 0xD8, 0xFF, 0xD9])),
            screenshot_cap: Cell::new(0),
            out_dir_made: Cell::new(false),
            out_dir_fails: false,
        }
    }

    /// Answers every command containing `text` with `outcome`, before the defaults.
    fn answer(&self, text: &str, outcome: Outcome) {
        self.answers
            .borrow_mut()
            .insert(0, (text.to_owned(), outcome));
    }

    fn commands(&self) -> Vec<String> {
        self.ran.borrow().iter().map(|(a, _)| a.join(" ")).collect()
    }

    fn step_frames(&self) -> Vec<StepFrame> {
        self.sent
            .borrow()
            .iter()
            .filter(|f| f.name.starts_with(STEP_PREFIX))
            .map(|f| serde_json::from_str(&f.body).expect("a step frame"))
            .collect()
    }
}

impl SimHost for Fake {
    fn is_root(&self) -> bool {
        self.root
    }
    fn runtime_images(&self) -> Vec<String> {
        self.runtimes.clone()
    }
    fn app_bundles(&self) -> Vec<String> {
        self.apps.clone()
    }
    fn console_user(&self) -> Option<ConsoleUser> {
        self.user_after_pauses
            .filter(|n| self.pauses.get() >= *n)
            .map(|_| ConsoleUser {
                uid: 501,
                name: "kbf".to_owned(),
            })
    }
    fn prepare_out_dir(&self) -> Result<(), String> {
        self.out_dir_made.set(true);
        if self.out_dir_fails {
            Err("read-only volume".to_owned())
        } else {
            Ok(())
        }
    }
    fn read_screenshot(&self, cap: usize) -> Result<Vec<u8>, String> {
        self.screenshot_cap.set(cap);
        self.screenshot.borrow().clone()
    }
    fn run(&self, argv: &[String], deadline: Duration) -> Outcome {
        self.ran.borrow_mut().push((argv.to_vec(), deadline));
        let line = argv.join(" ");
        self.answers
            .borrow()
            .iter()
            .find(|(text, _)| line.contains(text.as_str()))
            .map_or_else(|| ok_out(""), |(_, o)| o.clone())
    }
    fn send(&self, _port: u32, frame: &Frame) -> Result<(), SendError> {
        self.attempts.set(self.attempts.get() + 1);
        if self.down {
            return Err(SendError::Unavailable("no host".to_owned()));
        }
        if self.refuse.as_deref() == Some(frame.name.as_str()) {
            return Err(SendError::Refused("not wanted".to_owned()));
        }
        self.sent.borrow_mut().push(frame.clone());
        Ok(())
    }
    fn pause(&self, _d: Duration) {
        self.pauses.set(self.pauses.get() + 1);
    }
}

fn cfg() -> Config {
    Config::from(&args(&[]))
}

fn args(extra: &[&str]) -> SimArgs {
    #[derive(clap::Parser)]
    struct W {
        #[command(flatten)]
        a: SimArgs,
    }
    W::parse_from(std::iter::once("w").chain(extra.iter().copied())).a
}

fn go(host: &Fake) -> SummaryFrame {
    run(host, &cfg(), &mut Vec::new())
}

fn ids(steps: &[Step]) -> Vec<&str> {
    steps.iter().map(|s| s.id.as_str()).collect()
}

fn fixed_argv(steps: &[Step], id: &str) -> Vec<String> {
    match &steps.iter().find(|s| s.id == id).expect(id).argv {
        Argv::Fixed(v) => v.clone(),
        Argv::Late(_) => panic!("{id} is late"),
    }
}

/// Catches a plan that reorders, drops or adds a step, runs a step as the wrong user, or
/// runs one before something it needs. The root steps and their arguments are the ones the
/// probe is specified by.
#[test]
fn the_plan_runs_the_root_steps_then_the_user_steps_in_order() {
    let steps = plan(&Inputs {
        runtimes: vec!["a.dmg".into(), "b.dmg".into()],
        app: None,
    });
    assert_eq!(
        ids(&steps),
        [
            "xcode-select",
            "license",
            "first-launch",
            "runtime-add-a.dmg",
            "runtime-add-b.dmg",
            "list-runtimes",
            "list-devicetypes",
            "create",
            "boot",
            "bootstatus",
            "screenshot",
            "shutdown"
        ]
    );
    let who: Vec<Who> = steps.iter().map(|s| s.who).collect();
    assert!(who[..5].iter().all(|w| *w == Who::Root));
    assert!(who[5..].iter().all(|w| *w == Who::User));
    for (i, s) in steps.iter().enumerate() {
        for need in &s.needs {
            let at = steps
                .iter()
                .position(|x| &x.id == need)
                .expect("needs a step of the plan");
            assert!(at < i, "{} needs {need}, which comes later", s.id);
        }
    }
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        fixed_argv(&steps, "xcode-select"),
        s(&["/usr/bin/xcode-select", "-s", "/Applications/Xcode.app"])
    );
    assert_eq!(
        fixed_argv(&steps, "license"),
        s(&["/usr/bin/xcodebuild", "-license", "accept"])
    );
    assert_eq!(
        fixed_argv(&steps, "first-launch"),
        s(&["/usr/bin/xcodebuild", "-runFirstLaunch"])
    );
    assert_eq!(
        fixed_argv(&steps, "runtime-add-b.dmg"),
        s(&[
            "/usr/bin/xcrun",
            "simctl",
            "runtime",
            "add",
            "/private/var/kbf-sim/runtimes/b.dmg"
        ])
    );
    assert_eq!(
        fixed_argv(&steps, "list-runtimes"),
        s(&["/usr/bin/xcrun", "simctl", "list", "runtimes", "-j"])
    );
    // The runtimes are listed only after every one has been added.
    let list = steps
        .iter()
        .find(|s| s.id == "list-runtimes")
        .expect("list");
    assert_eq!(list.needs, ["runtime-add-a.dmg", "runtime-add-b.dmg"]);
}

/// Catches an app that is installed without its identifier, a launch that is not after the
/// install, and a screenshot that waits for the app (a failed install still leaves a
/// booted device worth a picture).
#[test]
fn an_app_adds_its_identifier_install_and_launch_before_the_screenshot() {
    let steps = plan(&Inputs {
        runtimes: Vec::new(),
        app: Some("Demo.app".into()),
    });
    assert_eq!(
        ids(&steps),
        [
            "xcode-select",
            "license",
            "first-launch",
            "list-runtimes",
            "list-devicetypes",
            "create",
            "boot",
            "bootstatus",
            "app-id",
            "install",
            "launch",
            "screenshot",
            "shutdown"
        ]
    );
    assert_eq!(
        fixed_argv(&steps, "app-id").last().map(String::as_str),
        Some("/private/var/kbf-sim/apps/Demo.app/Info.plist")
    );
    let need = |id: &str| steps.iter().find(|s| s.id == id).expect(id).needs.clone();
    assert_eq!(need("install"), ["bootstatus", "app-id"]);
    assert_eq!(need("launch"), ["install"]);
    assert_eq!(need("screenshot"), ["bootstatus"]);
    assert_eq!(need("shutdown"), ["boot"]);
    // With no runtime staged the listing waits for the first launch alone.
    assert_eq!(need("list-runtimes"), ["first-launch"]);
}

/// Catches a late step built from a missing value (it would run `simctl boot` with an empty
/// device), and arguments in the wrong order.
#[test]
fn late_arguments_come_from_what_earlier_steps_found() {
    let empty = State::default();
    for which in [
        Late::Create,
        Late::Boot,
        Late::BootStatus,
        Late::Install,
        Late::Launch,
        Late::Screenshot,
        Late::Shutdown,
    ] {
        assert!(empty.resolve(which).is_err(), "{which:?}");
    }
    let state = State {
        runtime: Some("RT".into()),
        device_type: Some("DT".into()),
        udid: Some(UDID.into()),
        bundle_id: Some("com.example.App".into()),
        app: Some("Demo.app".into()),
    };
    let line = |w| state.resolve(w).expect("resolve").join(" ");
    assert_eq!(
        line(Late::Create),
        "/usr/bin/xcrun simctl create kbf-sim DT RT"
    );
    assert_eq!(
        line(Late::Boot),
        format!("/usr/bin/xcrun simctl boot {UDID}")
    );
    assert_eq!(
        line(Late::BootStatus),
        format!("/usr/bin/xcrun simctl bootstatus {UDID} -b")
    );
    assert_eq!(
        line(Late::Install),
        format!("/usr/bin/xcrun simctl install {UDID} /private/var/kbf-sim/apps/Demo.app")
    );
    assert_eq!(
        line(Late::Launch),
        format!("/usr/bin/xcrun simctl launch {UDID} com.example.App")
    );
    assert_eq!(
        line(Late::Screenshot),
        format!(
            "/usr/bin/xcrun simctl io {UDID} screenshot --type=jpeg /private/var/kbf-sim/out/screenshot.jpg"
        )
    );
    assert_eq!(
        line(Late::Shutdown),
        format!("/usr/bin/xcrun simctl shutdown {UDID}")
    );
}

/// Catches a choice of runtime that takes an unavailable or non-iOS runtime or not the
/// newest, a device type that is not an iPhone, an id that is any last line, and a
/// bundle identifier with a space in it.
#[test]
fn the_choices_read_the_simctl_output() {
    assert_eq!(
        choose_runtime(RUNTIMES).as_deref(),
        Ok("com.apple.CoreSimulator.SimRuntime.iOS-27-0")
    );
    assert!(choose_runtime("{").unwrap_err().contains("not JSON"));
    assert!(choose_runtime("{}").unwrap_err().contains("runtimes"));
    let only_watch = r#"{"runtimes":[{"identifier":"com.apple.CoreSimulator.SimRuntime.watchOS-27-0","version":"27.0","isAvailable":true}]}"#;
    assert!(
        choose_runtime(only_watch)
            .unwrap_err()
            .contains("no available iOS runtime among 1")
    );
    // 9.10 is newer than 9.9: versions compare as numbers.
    let two = r#"{"runtimes":[
      {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-9-9","version":"9.9","isAvailable":true},
      {"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-9-10","version":"9.10","isAvailable":true}]}"#;
    assert_eq!(
        choose_runtime(two).as_deref(),
        Ok("com.apple.CoreSimulator.SimRuntime.iOS-9-10")
    );

    assert_eq!(
        choose_device_type(DEVICETYPES).as_deref(),
        Ok("com.apple.CoreSimulator.SimDeviceType.iPhone-16")
    );
    assert!(choose_device_type(r#"{"devicetypes":[{"identifier":"x","name":"iPad"}]}"#).is_err());
    assert!(choose_device_type("[]").is_err());

    assert_eq!(
        parse_udid(&format!("warning\n{UDID}\n\n")).as_deref(),
        Ok(UDID)
    );
    for bad in [
        "",
        "\n",
        "not-a-udid",
        "6F3A1B2C-0D4E-4F5A-8B6C-7D8E9F0A1B2",
        &format!("{UDID}x"),
        "6F3A1B2C-0D4E-4F5A-8B6C-7D8E9F0A1B2G",
    ] {
        assert!(parse_udid(bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        parse_bundle_id(" com.example.App\n").as_deref(),
        Ok("com.example.App")
    );
    assert!(parse_bundle_id("").is_err() && parse_bundle_id("a b").is_err());
    // An identifier is a reverse-DNS name: nothing else is put in a command or a frame.
    assert!(parse_bundle_id("com.example/App").is_err());
    assert!(parse_bundle_id("com.example.\u{e9}").is_err());
    assert!(parse_bundle_id(&"a".repeat(MAX_BUNDLE_ID_BYTES)).is_ok());
    assert!(parse_bundle_id(&"a".repeat(MAX_BUNDLE_ID_BYTES + 1)).is_err());
}

/// Catches a step frame that cannot be sent: what `simctl create` or `plutil` printed was
/// repeated whole in the error note, and a multi-MiB line made the frame exceed the body
/// limit. The note keeps a bounded part of the output and says it was cut.
#[test]
fn an_error_note_repeats_a_bounded_part_of_the_output_and_the_frame_encodes() {
    let huge = format!("{}\n", "\u{e9}".repeat(1 << 20));
    assert!(huge.len() > 2 << 20);
    for (step, command) in [("create", "simctl create"), ("app-id", "plutil")] {
        let mut host = Fake::new();
        host.apps = vec!["Demo.app".to_owned()];
        host.answer(
            command,
            Outcome {
                ended: Ended::Exited(0),
                stdout: huge.clone().into_bytes(),
                stderr: Vec::new(),
                cut: false,
                ms: 1,
            },
        );
        go(&host);
        let sent = host.sent.borrow();
        let f = sent
            .iter()
            .find(|f| f.name.contains(step))
            .unwrap_or_else(|| {
                panic!(
                    "{step}: {:?}",
                    sent.iter().map(|f| f.name.clone()).collect::<Vec<_>>()
                )
            });
        assert!(encode(f).is_ok(), "{step}: {} body bytes", f.body.len());
        let frame: StepFrame = serde_json::from_str(&f.body).expect("a step frame");
        assert_eq!(frame.status, Status::Error, "{step}");
        let note = frame.note.expect("a note");
        assert!(note.len() < 4 * MAX_ECHO_BYTES, "{step}: {}", note.len());
        assert!(note.contains(&format!("{CUT_MARK}\"")), "{step}: {note}");
    }
    // A cut never splits a character, and short output is repeated whole.
    let two_byte = "\u{e9}".repeat(MAX_ECHO_BYTES);
    let note = parse_bundle_id(&format!("{two_byte} x")).unwrap_err();
    assert!(
        note.contains(&"\u{e9}".repeat(MAX_ECHO_BYTES / 2)),
        "{note}"
    );
    assert!(note.contains(CUT_MARK));
    assert!(parse_bundle_id("a b").unwrap_err().contains("\"a b\""));
}

/// Catches a run that drops a step, runs the user steps as root or outside a session, loses
/// what an earlier step found, sends frames out of order, or gives a command the wrong
/// deadline.
#[test]
fn a_good_run_sends_one_frame_per_step_and_a_summary() {
    let mut host = Fake::new();
    host.apps = vec!["Demo.app".to_owned()];
    let summary = go(&host);
    assert!(host.out_dir_made.get());
    assert!(summary.ok && summary.complete, "{summary:?}");
    let names: Vec<String> = host.sent.borrow().iter().map(|f| f.name.clone()).collect();
    assert_eq!(
        names,
        [
            "sim-step-01-xcode-select.json",
            "sim-step-02-license.json",
            "sim-step-03-first-launch.json",
            "sim-step-04-runtime-add-iOS_27.dmg.json",
            "sim-step-05-list-runtimes.json",
            "sim-step-06-list-devicetypes.json",
            "sim-step-07-create.json",
            "sim-step-08-boot.json",
            "sim-step-09-bootstatus.json",
            "sim-step-10-app-id.json",
            "sim-step-11-install.json",
            "sim-step-12-launch.json",
            "sim-screenshot.json",
            "sim-step-13-screenshot.json",
            "sim-step-14-shutdown.json",
            "sim-summary.json",
        ]
    );
    assert!(host.sent.borrow().iter().all(|f| f.kind == Kind::Result));
    let cmds = host.commands();
    // Root steps are plain; user steps enter the console user's session.
    assert_eq!(cmds[0], "/usr/bin/xcode-select -s /Applications/Xcode.app");
    let wrap = "/bin/launchctl asuser 501 /usr/bin/sudo -u kbf -H -- ";
    assert!(
        cmds[4].starts_with(&format!("{wrap}/usr/bin/xcrun simctl list runtimes")),
        "{}",
        cmds[4]
    );
    assert!(
        cmds[3].starts_with("/usr/bin/xcrun simctl runtime add"),
        "{}",
        cmds[3]
    );
    // What the listings found reaches `create`, and what `create` printed reaches the rest.
    assert_eq!(
        cmds[6],
        format!(
            "{wrap}/usr/bin/xcrun simctl create kbf-sim com.apple.CoreSimulator.SimDeviceType.iPhone-16 com.apple.CoreSimulator.SimRuntime.iOS-27-0"
        )
    );
    assert_eq!(cmds[7], format!("{wrap}/usr/bin/xcrun simctl boot {UDID}"));
    assert_eq!(
        cmds[11],
        format!("{wrap}/usr/bin/xcrun simctl launch {UDID} com.example.App")
    );
    assert_eq!(cmds.len(), 14);
    assert_eq!(summary.udid.as_deref(), Some(UDID));
    assert_eq!(
        summary.runtime.as_deref(),
        Some("com.apple.CoreSimulator.SimRuntime.iOS-27-0")
    );
    assert_eq!(summary.screenshot_bytes, Some(4));
    // The frame shows the step's own argument list, not the wrapper.
    let frames = host.step_frames();
    assert_eq!(frames[7].argv[..3], ["/usr/bin/xcrun", "simctl", "boot"]);
    assert_eq!(frames[7].who, Who::User);
    assert_eq!(
        (frames[0].status, frames[0].exit, frames[0].ms),
        (Status::Ok, Some(0), 7)
    );
    // Each class has its own deadline.
    let secs = |i: usize| host.ran.borrow()[i].1.as_secs();
    assert_eq!((secs(0), secs(2), secs(3), secs(7)), (120, 1800, 1800, 600));
    assert_eq!(host.attempts.get(), 16, "every frame was sent once");
}

/// Catches a failed step that lets its dependents run, a skipped step blamed on the wrong
/// step, a skipped command that is run anyway, and a failure that is not reported with its
/// exit code and output.
#[test]
fn a_failed_step_skips_what_depends_on_it_and_runs_nothing_more_of_them() {
    let host = Fake::new();
    host.answer(
        "-license",
        Outcome {
            ended: Ended::Exited(69),
            stdout: b"out".to_vec(),
            stderr: b"license not accepted".to_vec(),
            cut: false,
            ms: 12,
        },
    );
    let summary = go(&host);
    assert!(!summary.ok);
    // Only the two commands that were due ran.
    assert_eq!(host.commands().len(), 2, "{:?}", host.commands());
    let frames = host.step_frames();
    assert_eq!(frames.len(), 11, "a frame for every step, run or not");
    let f = &frames[1];
    assert_eq!(
        (f.status, f.exit, f.stderr.as_str()),
        (Status::Failed, Some(69), "license not accepted")
    );
    assert_eq!(f.stdout, "out");
    // Each skipped step names a step it needs that did not succeed.
    let blame: Vec<(&str, Option<&str>)> = frames[2..]
        .iter()
        .map(|f| (f.step.as_str(), f.skipped_because.as_deref()))
        .collect();
    assert_eq!(blame[0], ("first-launch", Some("license")));
    assert_eq!(blame[1], ("runtime-add-iOS_27.dmg", Some("first-launch")));
    assert_eq!(blame[2], ("list-runtimes", Some("runtime-add-iOS_27.dmg")));
    assert_eq!(blame[3], ("list-devicetypes", Some("first-launch")));
    assert_eq!(blame[4], ("create", Some("list-runtimes")));
    assert_eq!(blame.last().copied(), Some(("shutdown", Some("boot"))));
    assert!(
        frames[2..]
            .iter()
            .all(|f| f.status == Status::Skipped && f.argv.is_empty() && f.exit.is_none())
    );
    assert_eq!(
        summary
            .steps
            .iter()
            .filter(|(_, s)| *s == Status::Skipped)
            .count(),
        9
    );
}

/// Catches a command that overruns its deadline and hangs the run (the run goes on, the
/// step is a timeout, and what needs it is skipped) and a timeout reported as an exit code.
#[test]
fn a_command_past_its_deadline_is_a_timeout_and_the_run_goes_on() {
    let host = Fake::new();
    host.answer("bootstatus", ended(Ended::TimedOut));
    let summary = go(&host);
    let frames = host.step_frames();
    let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
    assert_eq!(
        (by("bootstatus").status, by("bootstatus").exit),
        (Status::Timeout, None)
    );
    assert_eq!(by("bootstatus").deadline_secs, 600);
    assert_eq!(by("screenshot").status, Status::Skipped);
    assert_eq!(
        by("screenshot").skipped_because.as_deref(),
        Some("bootstatus")
    );
    // The shutdown needs only the boot: the booted device is shut down even though its
    // boot status never came.
    assert_eq!(by("shutdown").status, Status::Ok);
    assert!(!summary.ok);
}

/// Catches a command that cannot be started, or dies by a signal, being reported as a
/// success or as an exit code.
#[test]
fn a_command_that_cannot_start_or_is_killed_is_reported_as_such() {
    let host = Fake::new();
    host.answer(
        "xcode-select",
        ended(Ended::SpawnFailed("no such file".into())),
    );
    go(&host);
    let f = &host.step_frames()[0];
    assert_eq!((f.status, f.exit), (Status::Error, None));
    assert!(
        f.note
            .as_deref()
            .expect("note")
            .contains("could not start: no such file")
    );

    let host = Fake::new();
    host.answer("-runFirstLaunch", ended(Ended::Signaled(9)));
    go(&host);
    let f = &host.step_frames()[2];
    assert_eq!(
        (f.status, f.exit, f.signal),
        (Status::Failed, None, Some(9))
    );
}

/// Catches a listing that names no iOS runtime being taken as success, and the steps after
/// it running with nothing to build their arguments from.
#[test]
fn output_that_names_no_runtime_makes_the_step_an_error_and_skips_the_device() {
    let host = Fake::new();
    host.answer("list runtimes", ok_out(r#"{"runtimes":[]}"#));
    let summary = go(&host);
    let frames = host.step_frames();
    let list = frames
        .iter()
        .find(|f| f.step == "list-runtimes")
        .expect("list");
    assert_eq!((list.status, list.exit), (Status::Error, Some(0)));
    assert!(
        list.note
            .as_deref()
            .expect("note")
            .contains("no available iOS runtime")
    );
    let create = frames.iter().find(|f| f.step == "create").expect("create");
    assert_eq!(
        (create.status, create.skipped_because.as_deref()),
        (Status::Skipped, Some("list-runtimes"))
    );
    assert!(!host.commands().iter().any(|c| c.contains("simctl create")));
    assert!(!summary.ok);
}

/// Catches an app whose identifier cannot be read being installed anyway, and the
/// screenshot and shutdown not happening after an app step fails.
#[test]
fn a_failed_app_step_still_gets_the_screenshot_and_the_shutdown() {
    let mut host = Fake::new();
    host.apps = vec!["Demo.app".to_owned()];
    host.answer("plutil", ok_out("two words\n"));
    let summary = go(&host);
    let frames = host.step_frames();
    let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
    assert_eq!(by("app-id").status, Status::Error);
    assert_eq!(by("install").status, Status::Skipped);
    assert_eq!(by("launch").status, Status::Skipped);
    assert_eq!(
        (by("screenshot").status, by("shutdown").status),
        (Status::Ok, Status::Ok)
    );
    assert!(!summary.ok);
    assert!(!host.commands().iter().any(|c| c.contains("simctl install")));
}

/// Catches an unbounded wait for the console user: the pauses are counted and end, the
/// first user step is an error that says so, the steps that need it are skipped, and the
/// root steps are untouched.
#[test]
fn without_a_console_user_the_wait_is_bounded_and_the_user_steps_fail() {
    let mut host = Fake::new();
    host.user_after_pauses = None;
    let c = Config::from(&args(&["--console-wait-secs", "3"]));
    let summary = run(&host, &c, &mut Vec::new());
    assert_eq!(host.pauses.get(), 3);
    let frames = host.step_frames();
    let by = |id: &str| frames.iter().find(|f| f.step == id).expect(id);
    assert_eq!(by("first-launch").status, Status::Ok);
    assert_eq!(by("runtime-add-iOS_27.dmg").status, Status::Ok);
    assert_eq!(by("list-runtimes").status, Status::Error);
    assert!(
        by("list-runtimes")
            .note
            .as_deref()
            .expect("note")
            .contains("no user was logged in")
    );
    // The user steps that do not need `list-runtimes` also find no user, without waiting again.
    assert_eq!(by("list-devicetypes").status, Status::Error);
    assert_eq!(by("create").status, Status::Skipped);
    assert!(!summary.ok);
}

/// Catches a wait that does not end when the user appears, and a run that starts the user
/// steps before there is one.
#[test]
fn a_console_user_that_appears_during_the_wait_is_used() {
    let mut host = Fake::new();
    host.user_after_pauses = Some(2);
    let summary = go(&host);
    assert!(summary.ok);
    assert_eq!(host.pauses.get(), 2);
}

/// Catches a run as an ordinary user: it must run nothing, and say why in a summary the
/// host can read.
#[test]
fn a_run_that_is_not_root_runs_nothing_and_says_so() {
    let mut host = Fake::new();
    host.root = false;
    let summary = go(&host);
    assert!(host.ran.borrow().is_empty());
    assert!(!summary.ok && summary.steps.is_empty());
    assert_eq!(summary.note.as_deref(), Some("sim runs as root"));
    assert_eq!(host.sent.borrow().len(), 1);
    assert_eq!(host.sent.borrow()[0].name, SUMMARY_NAME);
}

/// Catches a screenshot that is not sent, is sent without its size, can overflow the frame
/// body limit, or whose failure is not a failure of the step. The largest allowed
/// screenshot must encode as a frame.
#[test]
fn the_screenshot_goes_as_base64_within_the_frame_limits() {
    let host = Fake::new();
    let bytes: Vec<u8> = (0..SCREENSHOT_MAX_BYTES).map(|i| (i % 251) as u8).collect();
    *host.screenshot.borrow_mut() = Ok(bytes.clone());
    let summary = go(&host);
    assert_eq!(host.screenshot_cap.get(), SCREENSHOT_MAX_BYTES);
    assert_eq!(summary.screenshot_bytes, Some(SCREENSHOT_MAX_BYTES));
    let sent = host.sent.borrow();
    let shot = sent
        .iter()
        .find(|f| f.name == SCREENSHOT_NAME)
        .expect("screenshot frame");
    assert!(shot.body.len() <= MAX_BODY_BYTES, "{}", shot.body.len());
    assert!(encode(shot).expect("encodes").len() <= MAX_FRAME_BYTES);
    let v: serde_json::Value = serde_json::from_str(&shot.body).expect("json");
    assert_eq!(
        (v["format"].as_str(), v["bytes"].as_u64()),
        (Some("jpeg"), Some(SCREENSHOT_MAX_BYTES as u64))
    );
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(v["base64"].as_str().expect("b64"))
        .expect("decodes");
    assert_eq!(decoded, bytes);
    drop(sent);

    let host = Fake::new();
    *host.screenshot.borrow_mut() = Err("over the cap".into());
    let summary = go(&host);
    let frames = host.step_frames();
    let shot = frames
        .iter()
        .find(|f| f.step == "screenshot")
        .expect("step");
    assert_eq!((shot.status, shot.exit), (Status::Error, Some(0)));
    assert_eq!(shot.note.as_deref(), Some("over the cap"));
    assert!(summary.screenshot_bytes.is_none());
    assert!(!host.sent.borrow().iter().any(|f| f.name == SCREENSHOT_NAME));
}

/// Catches a tail that cuts a character in half, keeps more than the cap, forgets to say
/// it cut, or a capture that hit its own cap going unreported.
#[test]
fn output_is_cut_to_its_tail_at_a_character_boundary() {
    assert_eq!(tail_text(b"abc", 8), ("abc".to_owned(), false));
    assert_eq!(tail_text(b"abcdef", 3), ("def".to_owned(), true));
    // 'é' is two bytes: a cut inside it starts after it.
    let text = "aéé".as_bytes();
    let (tail, cut) = tail_text(text, 3);
    assert!(cut);
    assert_eq!(tail, "é");
    for max in 0..text.len() {
        let (t, _) = tail_text(text, max);
        assert!(t.len() <= max && !t.contains('\u{FFFD}'), "{max}: {t:?}");
    }
    assert_eq!(tail_text(b"", 0), (String::new(), false));

    let host = Fake::new();
    let big = "é".repeat(20_000);
    host.answer(
        "-license",
        Outcome {
            ended: Ended::Exited(0),
            stdout: big.into_bytes(),
            stderr: b"small".to_vec(),
            cut: true,
            ms: 1,
        },
    );
    go(&host);
    let f = &host.step_frames()[1];
    assert!(f.stdout.len() <= cfg().tail_bytes && f.stdout.chars().all(|c| c == 'é'));
    // The capture's own cut marks both streams: neither shows everything.
    assert!(f.stdout_truncated && f.stderr_truncated);
    assert_eq!(f.stderr, "small");
}

/// Catches a frame that can outgrow the codec's limits: the worst output, every byte a
/// control character that JSON writes as six, still encodes, and every name stays within
/// the host's name limit for the longest runtime name.
#[test]
fn the_largest_step_frame_and_every_frame_name_fit_the_limits() {
    let host = Fake::new();
    host.answer(
        "-license",
        Outcome {
            ended: Ended::Exited(1),
            stdout: vec![1u8; 100_000],
            stderr: vec![2u8; 100_000],
            cut: false,
            ms: 1,
        },
    );
    go(&host);
    let sent = host.sent.borrow();
    let f = sent
        .iter()
        .find(|f| f.name.contains("license"))
        .expect("license frame");
    assert!(
        encode(f).expect("encodes").len() < MAX_FRAME_BYTES / 4,
        "{}",
        f.body.len()
    );
    drop(sent);

    let longest = format!("{}.dmg", "n".repeat(60));
    let steps = plan(&Inputs {
        runtimes: vec![longest.clone()],
        app: Some("A.app".into()),
    });
    for (i, s) in steps.iter().enumerate() {
        let name = step_frame_name(i, &s.id);
        assert!(name.len() <= 100, "{name}");
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
            "{name}"
        );
        assert!(!name.starts_with('.') && !name.contains(".."), "{name}");
    }
    assert_eq!(step_frame_name(0, "a"), "sim-step-01-a.json");
    assert_eq!(step_frame_name(99, "a"), "sim-step-100-a.json");
    assert_eq!(
        max_frames(0),
        plan(&Inputs {
            runtimes: vec![],
            app: Some("a.app".into())
        })
        .len()
            + 2
    );
}

/// Catches a host that is not listening hanging the run or aborting it: every frame is tried
/// a bounded number of times, the run still finishes, and the summary names what never
/// arrived. A refusal is not retried.
#[test]
fn frames_that_cannot_be_delivered_are_retried_a_bounded_number_of_times() {
    let mut host = Fake::new();
    host.down = true;
    let summary = go(&host);
    // Eleven steps, the screenshot (its frame fails, and with it the step) and the summary:
    // thirteen frames, five attempts each, with the probes' retry pause between attempts.
    assert_eq!(host.attempts.get(), 13 * 5);
    assert_eq!(host.pauses.get(), 13 * 4);
    assert_eq!(summary.undelivered.len(), 13);
    assert!(summary.undelivered.contains(&SUMMARY_NAME.to_owned()));

    let mut host = Fake::new();
    host.refuse = Some("sim-step-02-license.json".into());
    let summary = go(&host);
    assert_eq!(summary.undelivered, ["sim-step-02-license.json"]);
    assert_eq!(
        host.attempts.get(),
        13,
        "the refused frame was tried once, the others once"
    );
    assert!(summary.ok, "the steps themselves all passed");
}

/// Catches a runtime or app name the host did not stage being used in a command line or a
/// frame name.
#[test]
fn only_names_the_host_stages_are_used() {
    let mut host = Fake::new();
    host.runtimes = vec![
        "good_1.dmg".into(),
        "a b.dmg".into(),
        "x..dmg".into(),
        ".hidden.dmg".into(),
        "notes.txt".into(),
        "good_1.dmg".into(),
    ];
    host.apps = vec!["bad name.app".into(), "z.app".into(), "Demo.app".into()];
    go(&host);
    let cmds = host.commands();
    assert_eq!(cmds.iter().filter(|c| c.contains("runtime add")).count(), 1);
    assert!(cmds.iter().any(|c| c.ends_with("/runtimes/good_1.dmg")));
    // The first of the valid apps by name.
    assert!(cmds.iter().any(|c| c.contains("/apps/Demo.app/Info.plist")));
    assert!(name_ok("a.dmg", ".dmg") && !name_ok("a.dmg", ".app") && !name_ok(".dmg", ".dmg"));
    assert!(!name_ok(&format!("{}.dmg", "n".repeat(70)), ".dmg"));
}

/// Catches defaults that changed: the deadlines the daemon runs with when image setup passes
/// no flag, and flags that do not reach the config.
#[test]
fn the_defaults_and_the_flags_make_the_config() {
    let d = cfg();
    assert_eq!(
        (
            d.quick.as_secs(),
            d.first_launch.as_secs(),
            d.runtime_add.as_secs(),
            d.boot.as_secs(),
            d.console_wait.as_secs(),
            d.tail_bytes,
            d.port
        ),
        (120, 1800, 1800, 600, 300, 8192, 5000)
    );
    let c = Config::from(&args(&[
        "--quick-secs",
        "1",
        "--first-launch-secs",
        "2",
        "--runtime-add-secs",
        "3",
        "--boot-secs",
        "4",
        "--console-wait-secs",
        "5",
        "--tail-bytes",
        "6",
        "--vsock-port",
        "7",
    ]));
    assert_eq!(
        (
            c.deadline(Class::Quick).as_secs(),
            c.deadline(Class::FirstLaunch).as_secs(),
            c.deadline(Class::RuntimeAdd).as_secs(),
            c.deadline(Class::Boot).as_secs(),
            c.console_wait.as_secs(),
            c.tail_bytes,
            c.port
        ),
        (1, 2, 3, 4, 5, 6, 7)
    );
}

/// The user wrapper puts the argument list after `--`, so that nothing in it is read as an
/// option of `sudo`.
#[test]
fn the_user_wrapper_ends_sudos_options_before_the_command() {
    let u = ConsoleUser {
        uid: 502,
        name: "tester".into(),
    };
    let w = as_user(&u, &["-x".to_owned(), "y".to_owned()]);
    assert_eq!(
        w,
        [
            "/bin/launchctl",
            "asuser",
            "502",
            "/usr/bin/sudo",
            "-u",
            "tester",
            "-H",
            "--",
            "-x",
            "y"
        ]
    );
}

// The process runner: real, tiny commands; the clock is the only thing that moves time.

/// A clock whose `sleep` moves time forward at once, so a deadline passes without a wait.
struct Manual(Cell<u64>);

impl Clock for Manual {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
    fn sleep(&self, d: Duration) {
        self.0
            .set(self.0.get() + u64::try_from(d.as_millis()).expect("ms"));
    }
}

fn runner() -> ProcessRunner<Manual> {
    ProcessRunner::new(
        Manual(Cell::new(0)),
        vec![("KBF_SIM_TEST".into(), "yes".into())],
    )
}

fn argv(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

const LONG: Duration = Duration::from_secs(1 << 40);

/// Catches a runner that loses output, mixes the streams, reports a non-zero exit as success
/// or the other way, ignores the environment it was given, or lets a missing program pass.
#[test]
fn the_runner_reports_exit_output_environment_and_a_missing_program() {
    let r = runner();
    let out = r.run(&argv(&["/bin/echo", "hello"]), LONG);
    assert_eq!(
        (out.ended, out.stdout, out.stderr, out.cut),
        (Ended::Exited(0), b"hello\n".to_vec(), Vec::new(), false)
    );
    assert_eq!(
        r.run(&argv(&["/usr/bin/false"]), LONG).ended,
        Ended::Exited(1)
    );
    let env = r.run(&argv(&["/usr/bin/env"]), LONG);
    assert!(
        String::from_utf8_lossy(&env.stdout)
            .lines()
            .any(|l| l == "KBF_SIM_TEST=yes")
    );
    let err = r.run(&argv(&["/nonexistent/program"]), LONG);
    assert!(
        matches!(&err.ended, Ended::SpawnFailed(m) if m.contains("/nonexistent/program")),
        "{err:?}"
    );
    let empty = r.run(&[], LONG);
    assert!(matches!(empty.ended, Ended::SpawnFailed(_)));
    // A command that writes to stderr: kept apart from stdout.
    let e = r.run(&argv(&["/bin/ls", "/nonexistent/dir"]), LONG);
    assert!(matches!(e.ended, Ended::Exited(n) if n != 0));
    assert!(e.stdout.is_empty() && !e.stderr.is_empty());
}

/// Catches a runner that waits for a command for ever. Time here is the manual clock's, so
/// the deadline passes without a wait: the command (a sleep of a minute) is killed and the
/// run reports a timeout, with the time the clock showed.
#[test]
fn a_command_still_running_at_its_deadline_is_killed() {
    let r = runner();
    let out = r.run(&argv(&["/bin/sleep", "60"]), Duration::from_secs(5));
    assert_eq!(out.ended, Ended::TimedOut);
    assert!(out.ms >= 5000, "{}", out.ms);
}

/// A clock that holds time still until a file exists (the command has started what the test
/// needs it to have started), then moves it forward at once like [`Manual`]. The real
/// sleep only yields until that progress event; no assertion reads wall time.
struct AfterReady {
    time: Cell<u64>,
    ready: PathBuf,
}

impl Clock for AfterReady {
    fn now_ms(&self) -> u64 {
        self.time.get()
    }
    fn sleep(&self, d: Duration) {
        if self.ready.exists() {
            self.time
                .set(self.time.get() + u64::try_from(d.as_millis()).expect("ms"));
        } else {
            std::thread::yield_now();
        }
    }
}

/// Whether process `pid` is running (a zombie awaiting its reaper is not).
fn is_running(pid: &str) -> bool {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// The command of `the_deadline_also_ends_what_the_command_started`: this test binary run
/// again for this one test. It starts a minute-long sleep of its own, publishes that
/// sleep's pid in the file named by the environment, and waits for it. Run on its own (no
/// variable set) it does nothing.
#[test]
fn group_test_helper_starts_a_sleep_and_waits() {
    let Some(path) = std::env::var_os("KBF_SIM_GROUP_PIDFILE") else {
        return;
    };
    let path = PathBuf::from(path);
    let mut sleeper = std::process::Command::new("/bin/sleep")
        .arg("600")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("sleep");
    // Renamed into place once complete, so that "exists" means "fully written".
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, sleeper.id().to_string()).expect("pid");
    std::fs::rename(&tmp, &path).expect("publish");
    let _ = sleeper.wait();
}

/// Catches a deadline that ends only the command it started: the command here starts a
/// minute-long sleep of its own and waits for it; once the deadline passes, that sleep must
/// be gone too, not left running as an orphan of the killed command.
#[test]
fn the_deadline_also_ends_what_the_command_started() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-sim-group");
    let pid_file = dir.join("grandchild.pid");
    let r = ProcessRunner::new(
        AfterReady {
            time: Cell::new(0),
            ready: pid_file.clone(),
        },
        vec![(
            "KBF_SIM_GROUP_PIDFILE".into(),
            pid_file.display().to_string(),
        )],
    );
    let me = std::env::current_exe().expect("test binary");
    let out = r.run(
        &argv(&[
            me.to_str().expect("utf-8 path"),
            "--exact",
            "sim_tests::group_test_helper_starts_a_sleep_and_waits",
            "--test-threads=1",
        ]),
        Duration::from_secs(5),
    );
    assert_eq!(out.ended, Ended::TimedOut);
    let pid = std::fs::read_to_string(&pid_file)
        .expect("pid")
        .trim()
        .to_owned();
    // The group is killed at once; a reaper may need a moment to clear the entry, so wait
    // for that event (bounded by tries, not by a time the test asserts).
    let mut running = true;
    for _ in 0..3_000 {
        running = is_running(&pid);
        if !running {
            break;
        }
        std::thread::yield_now();
    }
    if running {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-9", &pid])
            .status();
    }
    assert!(!running, "the grandchild {pid} outlived the deadline");
}

/// Catches output past the capture cap going unreported or unbounded.
#[test]
fn output_past_the_capture_cap_is_dropped_and_flagged() {
    let r = runner();
    let n = MAX_CAPTURE_BYTES + 12_345;
    let out = r.run(
        &argv(&["/usr/bin/head", "-c", &n.to_string(), "/dev/zero"]),
        LONG,
    );
    assert_eq!(out.ended, Ended::Exited(0));
    assert_eq!((out.stdout.len(), out.cut), (MAX_CAPTURE_BYTES, true));
}

/// Catches a screenshot read that follows a link, reads a directory or a larger file, or
/// refuses one exactly at the cap.
#[test]
fn a_screenshot_is_read_only_from_a_small_regular_file() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-sim-shot");
    let file: PathBuf = dir.join("s.jpg");
    std::fs::write(&file, vec![7u8; 10]).expect("file");
    assert_eq!(read_regular(&file, 10).expect("exact"), vec![7u8; 10]);
    assert!(
        read_regular(&file, 9)
            .unwrap_err()
            .contains("over the 9 byte cap")
    );
    std::os::unix::fs::symlink(&file, dir.join("link.jpg")).expect("link");
    assert!(read_regular(&dir.join("link.jpg"), 100).is_err());
    assert!(
        read_regular(&dir, 100)
            .unwrap_err()
            .contains("not a regular file")
    );
    assert!(read_regular(&dir.join("none"), 100).is_err());
}

/// Catches an order of fake answers that depends on how they were queued: the default
/// answers must not mask a scripted one.
#[test]
fn a_scripted_answer_wins_over_the_defaults() {
    let host = Fake::new();
    host.answer("simctl create", ok_out("not an id\n"));
    go(&host);
    let frames = host.step_frames();
    let create = frames.iter().find(|f| f.step == "create").expect("create");
    assert_eq!(create.status, Status::Error);
}

/// Catches a screenshot directory that cannot be made going unsaid: the run goes on (the
/// device may still boot) and the summary carries the reason.
#[test]
fn a_screenshot_directory_that_cannot_be_made_is_noted_in_the_summary() {
    let mut host = Fake::new();
    host.out_dir_fails = true;
    let summary = go(&host);
    assert_eq!(
        summary.note.as_deref(),
        Some("prepare /private/var/kbf-sim/out: read-only volume")
    );
    assert!(summary.ok, "the steps themselves passed: {summary:?}");
}

/// Catches a late step whose earlier step did not give it what it needs but was not
/// skipped: it is an error that names the missing value, and no command is run for it.
#[test]
fn a_late_step_with_nothing_to_build_from_is_an_error_and_runs_no_command() {
    let host = Fake::new();
    let steps = vec![Step {
        id: "boot".to_owned(),
        who: Who::User,
        class: Class::Boot,
        needs: Vec::new(),
        argv: Argv::Late(Late::Boot),
    }];
    let summary = run_plan(&host, &cfg(), &steps, &mut Vec::new());
    let frames = host.step_frames();
    assert_eq!(frames.len(), 1);
    assert_eq!((frames[0].status, frames[0].argv.len()), (Status::Error, 0));
    assert_eq!(
        frames[0].note.as_deref(),
        Some("no device id from an earlier step")
    );
    assert!(host.ran.borrow().is_empty());
    assert!(!summary.ok);
}

/// Catches a host that hands back more than the cap it was given: the frame would be over
/// the body limit, so it is not sent and the step says why.
#[test]
fn a_screenshot_larger_than_a_frame_can_hold_is_not_sent() {
    let host = Fake::new();
    *host.screenshot.borrow_mut() = Ok(vec![7u8; 800_000]);
    go(&host);
    let frames = host.step_frames();
    let shot = frames
        .iter()
        .find(|f| f.step == "screenshot")
        .expect("step");
    assert_eq!(shot.status, Status::Error);
    assert!(
        shot.note.as_deref().expect("note").contains("byte limit"),
        "{shot:?}"
    );
    assert!(!host.sent.borrow().iter().any(|f| f.name == SCREENSHOT_NAME));
}

/// Catches an exit status read wrongly: a normal exit keeps its code, a signal is a signal,
/// and a stopped process is neither (it is not an end).
#[test]
fn an_exit_status_is_an_exit_code_a_signal_or_neither() {
    use crate::sim_run::ended_of;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    assert_eq!(ended_of(ExitStatus::from_raw(0)), Ended::Exited(0));
    assert_eq!(ended_of(ExitStatus::from_raw(3 << 8)), Ended::Exited(3));
    assert_eq!(ended_of(ExitStatus::from_raw(9)), Ended::Signaled(9));
    assert_eq!(ended_of(ExitStatus::from_raw(0x137f)), Ended::Exited(-1));
}

/// Catches a real clock that does not tell time forward or whose sleep is not a sleep (the
/// deadline of a real run rests on both). No duration is asserted.
#[test]
fn the_system_clock_moves_forward() {
    use crate::sim_run::SystemClock;
    let c = SystemClock::default();
    let a = c.now_ms();
    c.sleep(Duration::ZERO);
    assert!(c.now_ms() >= a);
    let _ = SystemClock::new();
}

/// Catches an exported bundle added with `simctl runtime add` (which takes an image), an image
/// imported with `-importPlatform`, an import that is not a root step after first launch, one
/// the listing does not wait for, one on a deadline of its own, and a frame name for the
/// longest bundle name that the host's sink would refuse.
#[test]
fn a_bundle_runtime_is_imported_and_an_image_is_added() {
    let steps = plan(&Inputs {
        runtimes: vec!["a.dmg".into(), "b_27.exportedBundle".into()],
        app: None,
    });
    assert_eq!(
        ids(&steps)[3..6],
        [
            "runtime-add-a.dmg",
            "runtime-import-b_27.exportedBundle",
            "list-runtimes"
        ]
    );
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        fixed_argv(&steps, "runtime-import-b_27.exportedBundle"),
        s(&[
            "/usr/bin/xcodebuild",
            "-importPlatform",
            "/private/var/kbf-sim/runtimes/b_27.exportedBundle"
        ])
    );
    assert_eq!(
        fixed_argv(&steps, "runtime-add-a.dmg")[..4],
        s(&["/usr/bin/xcrun", "simctl", "runtime", "add"])[..]
    );
    let import = &steps[4];
    assert_eq!(import.needs, ["first-launch"]);
    assert_eq!((import.who, import.class), (Who::Root, Class::RuntimeAdd));
    let list = steps
        .iter()
        .find(|s| s.id == "list-runtimes")
        .expect("list");
    assert_eq!(
        list.needs,
        ["runtime-add-a.dmg", "runtime-import-b_27.exportedBundle"]
    );
    // The same class as an add: one deadline, from one flag.
    assert_eq!(import.class, Class::RuntimeAdd);

    let longest = format!(
        "{}{BUNDLE_SUFFIX}",
        "n".repeat(MAX_STAGED_NAME_BYTES - BUNDLE_SUFFIX.len())
    );
    assert!(name_ok(&longest, BUNDLE_SUFFIX));
    let steps = plan(&Inputs {
        runtimes: vec![longest],
        app: Some("A.app".into()),
    });
    for (i, st) in steps.iter().enumerate() {
        assert!(step_frame_name(i, &st.id).len() <= 100, "{}", st.id);
    }
}

/// Catches the run importing a bundle through the add step or the reverse, using a bundle name
/// the host did not stage (bad characters, wrong case of the suffix, no name), the
/// two runtimes of one run not both reaching a command, and a frame not named for the import.
#[test]
fn a_run_imports_the_staged_bundle_and_ignores_names_the_host_did_not_stage() {
    let mut host = Fake::new();
    host.runtimes = vec![
        "a.dmg".into(),
        "b_27.exportedBundle".into(),
        "bad name.exportedBundle".into(),
        ".exportedBundle".into(),
        "x.exportedbundle".into(),
        "b_27.exportedBundle".into(),
    ];
    let summary = go(&host);
    assert!(summary.ok);
    let cmds = host.commands();
    assert_eq!(
        cmds[3],
        "/usr/bin/xcrun simctl runtime add /private/var/kbf-sim/runtimes/a.dmg"
    );
    assert_eq!(
        cmds[4],
        "/usr/bin/xcodebuild -importPlatform /private/var/kbf-sim/runtimes/b_27.exportedBundle"
    );
    assert_eq!(cmds.iter().filter(|c| c.contains("import")).count(), 1);
    assert_eq!(cmds.iter().filter(|c| c.contains("runtime add")).count(), 1);
    assert_eq!(host.ran.borrow()[4].1.as_secs(), 1800);
    let names: Vec<String> = host.sent.borrow().iter().map(|f| f.name.clone()).collect();
    assert!(
        names.contains(&"sim-step-05-runtime-import-b_27.exportedBundle.json".to_owned()),
        "{names:?}"
    );
}

fn runtime_listing_run(runtimes: &str) -> (Fake, SummaryFrame) {
    let host = Fake::new();
    host.answer("list runtimes", ok_out(runtimes));
    host.answer("list devicetypes", ok_out(DEVICETYPES_WITH_OLD_LAST));
    let summary = go(&host);
    (host, summary)
}

fn frame_of(host: &Fake, step: &str) -> StepFrame {
    host.step_frames()
        .into_iter()
        .find(|f| f.step == step)
        .expect(step)
}

/// Catches a device type taken from the global list (whose last iPhone, the 6s Plus, the new
/// runtime refuses) instead of from the chosen runtime's `supportedDeviceTypes`; and a
/// runtime chosen from an unavailable entry whose support list differs.
#[test]
fn the_device_type_comes_from_the_chosen_runtimes_supported_list() {
    let (host, summary) = runtime_listing_run(RUNTIMES_WITH_SUPPORT);
    assert!(summary.ok, "{summary:?}");
    let create = host
        .commands()
        .into_iter()
        .find(|c| c.contains("simctl create"))
        .expect("create ran");
    assert!(
        create.ends_with(
            "simctl create kbf-sim com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro com.apple.CoreSimulator.SimRuntime.iOS-27-0"
        ),
        "{create}"
    );
    assert_eq!(
        summary.device_type.as_deref(),
        Some("com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro")
    );
    // The frames say what was chosen and from where.
    let note = frame_of(&host, "list-runtimes").note.expect("note");
    assert!(
        note.contains("iPhone-17-Pro") && note.contains("supportedDeviceTypes"),
        "{note}"
    );
    let note = frame_of(&host, "list-devicetypes").note.expect("note");
    assert!(
        note.contains("iPhone-17-Pro") && note.contains("not used"),
        "{note}"
    );
}

/// Catches the pick rules of the runtime's list: the last iPhone by `productFamily` (later
/// iPads and watches ignored), a missing `productFamily` read from the name, and the unit
/// answers for a runtime that is not listed or has no iPhone.
#[test]
fn the_supported_list_rule_picks_the_last_iphone() {
    let id = "com.apple.CoreSimulator.SimRuntime.iOS-27-0";
    assert_eq!(
        choose_device_type_for_runtime(RUNTIMES_WITH_SUPPORT, id),
        Ok(RuntimeDeviceType::Chosen(
            "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro".to_owned()
        ))
    );
    let no_family = r#"{"runtimes":[{"identifier":"R","supportedDeviceTypes":[
      {"identifier":"p1","name":"iPhone 1"},{"identifier":"p2","name":"iPhone 2"},{"identifier":"w","name":"Watch"}]}]}"#;
    assert_eq!(
        choose_device_type_for_runtime(no_family, "R"),
        Ok(RuntimeDeviceType::Chosen("p2".to_owned()))
    );
    // A family that is not iPhone is not taken even when the name says iPhone.
    let odd = r#"{"runtimes":[{"identifier":"R","supportedDeviceTypes":[
      {"identifier":"p1","name":"iPhone 1","productFamily":"iPhone"},{"identifier":"x","name":"iPhone Mirror","productFamily":"Mac"}]}]}"#;
    assert_eq!(
        choose_device_type_for_runtime(odd, "R"),
        Ok(RuntimeDeviceType::Chosen("p1".to_owned()))
    );
    assert!(
        choose_device_type_for_runtime(RUNTIMES_WITH_SUPPORT, "nope")
            .unwrap_err()
            .contains("nope")
    );
    assert!(
        choose_device_type_for_runtime("{", "R")
            .unwrap_err()
            .contains("not JSON")
    );
    assert!(
        choose_device_type_for_runtime("{}", "R")
            .unwrap_err()
            .contains("runtimes")
    );
}

/// Catches a runtime whose supported list holds no iPhone falling back to the global list
/// (which would pick a device the runtime refuses): the step is an error naming the
/// runtime, and nothing is created.
#[test]
fn a_runtime_supporting_no_iphone_is_an_error_naming_it() {
    let ipads_only = r#"{"runtimes":[{"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-27-0",
      "version":"27.0","isAvailable":true,"name":"iOS 27.0","supportedDeviceTypes":[
      {"identifier":"i","name":"iPad Pro","productFamily":"iPad"}]}]}"#;
    let (host, summary) = runtime_listing_run(ipads_only);
    assert!(!summary.ok);
    let f = frame_of(&host, "list-runtimes");
    assert_eq!(f.status, Status::Error);
    let note = f.note.expect("note");
    assert!(
        note.contains("com.apple.CoreSimulator.SimRuntime.iOS-27-0"),
        "{note}"
    );
    assert!(note.contains("no iPhone"), "{note}");
    assert_eq!(frame_of(&host, "create").status, Status::Skipped);
    assert!(!host.commands().iter().any(|c| c.contains("simctl create")));
}

/// Catches a runtime with no `supportedDeviceTypes` key failing, or the fallback not being
/// used and not saying so: the old devicetypes rule applies and both notes say it.
#[test]
fn a_runtime_without_a_supported_list_falls_back_to_the_devicetypes_listing() {
    let (host, summary) = runtime_listing_run(RUNTIMES);
    assert!(summary.ok, "{summary:?}");
    assert_eq!(
        summary.device_type.as_deref(),
        Some("com.apple.CoreSimulator.SimDeviceType.iPhone-6s-Plus")
    );
    let note = frame_of(&host, "list-runtimes").note.expect("note");
    assert!(note.contains("no supportedDeviceTypes"), "{note}");
    let note = frame_of(&host, "list-devicetypes").note.expect("note");
    assert!(
        note.contains("iPhone-6s-Plus") && note.contains("may not be one it supports"),
        "{note}"
    );
}
