//! The transient unit that runs a step: the command line as a pure function, and the run
//! against a fake.

mod support;

use std::cell::RefCell;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;

use support::{Scratch, sample, with};
use update_hook::{Class, Outcome, Step};
use update_rights::launch::FIXED_PROPERTIES;
use update_rights::{
    Declaration, Declared, Finished, Launch, LaunchError, Refusal, RunError, Spawner, Systemd,
    SystemdRun, command, properties, run_step, unit_name,
};

const RELEASE: &str = "/var/lib/updater/releases/tierstore/abc";
const PARAMS: &str = "/run/updater/params.toml";
/// The uid of a hook account that is not root.
const ORDINARY: u32 = 65534;

/// A launch of the hook of `d` as the loader would resolve it: the uid of its account and the names of
/// the host's services (its own). The loaded declaration is leaked: a test is short and the borrow
/// of a local would not outlive the helper.
fn built<'a>(
    d: &Declaration,
    uid: u32,
    step: Step,
    release: &'a Path,
    params: Option<&'a Path>,
) -> Launch<'a> {
    let declared: &'static Declared = Box::leak(Box::new(support::declared(
        d.clone(),
        uid,
        &[d.name.as_str()],
    )));
    Launch::of(declared, step, 7, release, params)
}

fn launch<'a>(d: &Declaration, step: Step, params: Option<&'a Path>) -> Launch<'a> {
    built(d, ORDINARY, step, Path::new(RELEASE), params)
}

fn args_of(d: &Declaration, step: Step, params: Option<&Path>) -> Vec<String> {
    command(&launch(d, step, params))
        .unwrap()
        .args
        .iter()
        .map(|a| a.to_str().unwrap().to_owned())
        .collect()
}

fn property_args(args: &[String]) -> Vec<&str> {
    args.iter()
        .filter_map(|a| a.strip_prefix("--property="))
        .collect()
}

fn fixed() -> Vec<String> {
    FIXED_PROPERTIES.iter().map(|p| (*p).to_owned()).collect()
}

/// The whole command of the design's example for `prepare`: every property from the declaration
/// and the fixed ones, the deterministic unit name, the options that carry the exit status and
/// the last line, and the hook as the declaration names it with the step's arguments.
#[test]
fn the_command_for_prepare_is_exactly_this() {
    let d = sample();
    let mut expected: Vec<String> = [
        "--system",
        "--quiet",
        "--wait",
        "--collect",
        "--pipe",
        "--unit=updater-tierstore-prepare-7",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let mut properties = vec![
        "User=tierstore-update",
        "Group=tierstore-update",
        "AmbientCapabilities=CAP_NET_ADMIN",
        "CapabilityBoundingSet=CAP_NET_ADMIN",
        "NoNewPrivileges=yes",
        "ProtectSystem=strict",
        "ProtectHome=yes",
        "PrivateTmp=yes",
        "ReadOnlyPaths=/etc/tierstore /etc/updater.d/tierstore.toml /usr/libexec/kbf /usr/sbin/nft",
        "ReadWritePaths=/var/lib/updater/hooks/tierstore",
        "RestrictAddressFamilies=AF_INET AF_INET6 AF_NETLINK AF_UNIX",
        "RuntimeMaxSec=600",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    properties.extend(fixed());
    expected.extend(properties.iter().map(|p| format!("--property={p}")));
    expected.extend(
        [
            "--",
            "/var/lib/updater/releases/tierstore/abc/tierstore",
            "update-hook",
            "prepare",
            RELEASE,
            "--params",
            PARAMS,
        ]
        .map(str::to_owned),
    );
    assert_eq!(
        args_of(&d, Step::Prepare, Some(Path::new(PARAMS))),
        expected
    );
}

/// The fixed properties are those of the design, none dropped.
#[test]
fn the_fixed_properties_are_the_designs() {
    assert_eq!(
        fixed(),
        [
            "ProtectKernelTunables=yes",
            "ProtectKernelModules=yes",
            "ProtectKernelLogs=yes",
            "ProtectControlGroups=yes",
            "RestrictNamespaces=yes",
            "RestrictRealtime=yes",
            "LockPersonality=yes",
            "MemoryDenyWriteExecute=yes",
            "SystemCallFilter=@system-service",
            "SystemCallArchitectures=native",
        ]
    );
    for step in Step::ALL {
        let all = properties(&sample(), step);
        assert_eq!(&all[all.len() - 10..], &fixed()[..], "{step}");
    }
}

/// Catches a capability held in a step that does not need it: tierstore holds `CAP_NET_ADMIN`
/// in `prepare`, `cutover` and `rollback` and nothing in `verify` and `cleanup`, both as an
/// ambient capability and as the bounding set; and each step's own timeout.
#[test]
fn capabilities_and_timeouts_are_per_step() {
    let d = sample();
    for (step, held, seconds) in [
        (Step::Prepare, "CAP_NET_ADMIN", 600),
        (Step::Cutover, "CAP_NET_ADMIN", 300),
        (Step::Verify, "", 1000),
        (Step::Rollback, "CAP_NET_ADMIN", 600),
        (Step::Cleanup, "", 1300),
    ] {
        let all = properties(&d, step);
        assert!(
            all.contains(&format!("AmbientCapabilities={held}")),
            "{step}: {all:?}"
        );
        assert!(
            all.contains(&format!("CapabilityBoundingSet={held}")),
            "{step}"
        );
        assert!(all.contains(&format!("RuntimeMaxSec={seconds}")), "{step}");
        assert_eq!(
            all.iter()
                .filter(|p| p.starts_with("RuntimeMaxSec="))
                .count(),
            1
        );
        assert!(all.contains(&"NoNewPrivileges=yes".to_owned()));
    }
}

/// Catches the hook-wide grant being dropped for a step with no entry, or applied to a step that
/// has one: the fallback is for the step with no entry only. Several capabilities are space
/// separated and sorted.
#[test]
fn a_step_with_no_entry_holds_the_hook_wide_grant() {
    let text = with(
        "capabilities = []",
        "capabilities = [\"CAP_SYS_PTRACE\", \"CAP_KILL\"]",
    )
    .replace("verify = []\n", "");
    let d = Declaration::parse(&text).unwrap();
    assert!(
        properties(&d, Step::Verify)
            .contains(&"AmbientCapabilities=CAP_KILL CAP_SYS_PTRACE".to_owned())
    );
    assert!(
        properties(&d, Step::Verify)
            .contains(&"CapabilityBoundingSet=CAP_KILL CAP_SYS_PTRACE".to_owned())
    );
    // Cleanup has an entry, and it is empty: it holds nothing whatever the grant says.
    assert!(properties(&d, Step::Cleanup).contains(&"AmbientCapabilities=".to_owned()));
    assert!(
        properties(&d, Step::Prepare).contains(&"AmbientCapabilities=CAP_NET_ADMIN".to_owned())
    );
}

/// Catches the record directory not being writable, listed twice, or the declared paths being
/// lost: `ReadWritePaths=` is the writable paths and the record directory, sorted, once each.
#[test]
fn the_record_directory_is_writable_once() {
    let d = Declaration::parse(&with(
        "write_paths = []",
        "write_paths = [\"/opt/b\", \"/opt/a\", \"/var/lib/updater/hooks/tierstore\"]",
    ))
    .unwrap();
    let all = properties(&d, Step::Cleanup);
    assert!(
        all.contains(&"ReadWritePaths=/opt/a /opt/b /var/lib/updater/hooks/tierstore".to_owned())
    );
}

/// Catches a read-only property for no paths (systemd reads an empty `ReadOnlyPaths=` as a
/// reset), and the empty address family list taking a value that would be read as a reset.
#[test]
fn empty_lists_are_not_resets() {
    let text = with(
        "read_paths = [\"/usr/libexec/kbf\", \"/usr/sbin/nft\", \"/etc/tierstore\", \"/etc/updater.d/tierstore.toml\"]",
        "read_paths = []",
    )
    .replace("address_families = [\"AF_UNIX\", \"AF_INET\", \"AF_INET6\", \"AF_NETLINK\"]", "address_families = []");
    let d = Declaration::parse(&text).unwrap();
    let all = properties(&d, Step::Prepare);
    assert!(
        all.iter().all(|p| !p.starts_with("ReadOnlyPaths")),
        "{all:?}"
    );
    assert!(all.contains(&"RestrictAddressFamilies=".to_owned()));
}

/// Catches the environment being merged into one property systemd would split on white space,
/// or inherited: one `Environment=` per variable, sorted, and none when there are none.
#[test]
fn each_variable_is_its_own_property() {
    let d = Declaration::parse(&with(
        "environment = { }",
        "environment = { B = \"2\", A = \"/x:y\" }",
    ))
    .unwrap();
    let all = properties(&d, Step::Verify);
    let env: Vec<&String> = all
        .iter()
        .filter(|p| p.starts_with("Environment="))
        .collect();
    assert_eq!(env, ["Environment=A=/x:y", "Environment=B=2"]);
    assert!(
        properties(&sample(), Step::Verify)
            .iter()
            .all(|p| !p.starts_with("Environment"))
    );
}

/// Catches the step arguments being wrong: only `prepare` is given the artifact, `--params`
/// follows when there is a file, and `hook_args` come first (none for an executable that takes
/// the step first).
#[test]
fn the_hook_is_run_with_the_declared_arguments_and_the_steps() {
    let d = sample();
    let tail = |args: &[String]| args[args.iter().position(|a| a == "--").unwrap() + 1..].to_vec();
    assert_eq!(
        tail(&args_of(&d, Step::Cutover, None)),
        [
            "/var/lib/updater/releases/tierstore/abc/tierstore",
            "update-hook",
            "cutover"
        ]
    );
    assert_eq!(
        tail(&args_of(&d, Step::Verify, Some(Path::new(PARAMS)))),
        [
            "/var/lib/updater/releases/tierstore/abc/tierstore",
            "update-hook",
            "verify",
            "--params",
            PARAMS
        ]
    );
    assert_eq!(
        tail(&args_of(&d, Step::Prepare, None)),
        [
            "/var/lib/updater/releases/tierstore/abc/tierstore",
            "update-hook",
            "prepare",
            RELEASE
        ]
    );
    let bare = Declaration::parse(&with(
        "binary = \"tierstore\"",
        "binary = \"bin/run\"\nhook_args = []",
    ))
    .unwrap();
    assert_eq!(
        tail(&args_of(&bare, Step::Rollback, None)),
        [
            "/var/lib/updater/releases/tierstore/abc/bin/run",
            "rollback"
        ]
    );
}

/// Catches the unit name not being the design's, or two runs sharing one: service, step and
/// generation each change it.
#[test]
fn the_unit_is_named_for_service_step_and_generation() {
    assert_eq!(
        unit_name("tierstore", Step::Cutover, 12),
        "updater-tierstore-cutover-12"
    );
    assert_eq!(
        unit_name("js-agent", Step::Prepare, 0),
        "updater-js-agent-prepare-0"
    );
    let names: std::collections::BTreeSet<String> = [("a", 1), ("b", 1), ("a", 2)]
        .into_iter()
        .flat_map(|(service, generation)| {
            Step::ALL.map(|step| unit_name(service, step, generation))
        })
        .collect();
    assert_eq!(names.len(), 15);
    let args = args_of(&sample(), Step::Verify, None);
    assert!(args.contains(&"--unit=updater-tierstore-verify-7".to_owned()));
}

/// Catches the options that make the exit status and the last line readable being dropped, or
/// the options being read by the hook: all of them come before `--`.
#[test]
fn the_wait_collect_and_pipe_options_come_first() {
    let args = args_of(&sample(), Step::Verify, None);
    let separator = args.iter().position(|a| a == "--").unwrap();
    for option in ["--system", "--quiet", "--wait", "--collect", "--pipe"] {
        let at = args.iter().position(|a| a == option).unwrap();
        assert!(at < separator, "{option}");
    }
    assert_eq!(args.iter().filter(|a| a.as_str() == "--").count(), 1);
    assert!(property_args(&args).len() > 20);
}

/// Catches a relative staged release or params file reaching the command: the hook would be run
/// from wherever the updater happens to be.
#[test]
fn a_relative_path_is_refused() {
    let d = sample();
    let relative = built(&d, ORDINARY, Step::Prepare, Path::new("releases/x"), None);
    assert_eq!(
        command(&relative),
        Err(LaunchError::RelativeRelease("releases/x".into()))
    );
    let params = Path::new("params.toml");
    assert_eq!(
        command(&launch(&d, Step::Prepare, Some(params))),
        Err(LaunchError::RelativeParams("params.toml".into()))
    );
    assert!(
        LaunchError::RelativeRelease("x".into())
            .to_string()
            .contains("not an absolute path")
    );
    assert!(
        LaunchError::RelativeParams("x".into())
            .to_string()
            .contains("params")
    );
}

struct Fake {
    seen: RefCell<Vec<SystemdRun>>,
    answer: Result<Finished, io::ErrorKind>,
}

impl Fake {
    fn answering(code: Option<i32>, stdout: &str, stderr: &str) -> Fake {
        Fake {
            seen: RefCell::default(),
            answer: Ok(Finished {
                code,
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            }),
        }
    }
}

impl Spawner for Fake {
    fn run(&self, command: &SystemdRun) -> io::Result<Finished> {
        self.seen.borrow_mut().push(command.clone());
        self.answer.clone().map_err(io::Error::from)
    }
}

/// Catches the exit status and the last line being read wrongly: each of the four codes is its
/// outcome, any other code and a signal are failures, only the last line is the report, and the
/// command that was run is the one the pure function builds.
#[test]
fn a_step_is_read_as_the_updater_reads_it() {
    let d = sample();
    let report = "{\"class\":\"environment\",\"reason\":\"drain_bound\",\"retry_after_secs\":300,\"detail\":\"busy\"}";
    let stdout = format!("starting\nnot json\n{report}\n");
    for (code, outcome) in [
        (Some(0), Outcome::Success),
        (Some(1), Outcome::Failure),
        (Some(75), Outcome::Deferred),
        (Some(76), Outcome::Halt),
        (Some(42), Outcome::Failure),
        (Some(203), Outcome::Failure),
        (None, Outcome::Failure),
    ] {
        let fake = Fake::answering(code, &stdout, "to the log\n");
        let run = run_step(&fake, &launch(&d, Step::Verify, None)).unwrap();
        assert_eq!(run.outcome, outcome, "{code:?}");
        assert_eq!(run.unit, "updater-tierstore-verify-7");
        assert_eq!(run.report.class, Class::Environment);
        assert_eq!(run.report.reason.as_deref(), Some("drain_bound"));
        assert_eq!(run.report.retry_after_secs, Some(300));
        assert_eq!(run.report.detail.as_deref(), Some("busy"));
        assert_eq!(run.stderr, "to the log\n");
        assert_eq!(
            *fake.seen.borrow(),
            [command(&launch(&d, Step::Verify, None)).unwrap()]
        );
    }
}

/// Catches a step that wrote no report, or a malformed one, being read as anything but `{}`.
#[test]
fn a_missing_or_malformed_last_line_is_the_empty_report() {
    let d = sample();
    for stdout in [
        "",
        "\n\n",
        "just text\n",
        "{\"class\": 5}\n",
        "{\"reason\":\"Bad Token\"}\n",
    ] {
        let fake = Fake::answering(Some(1), stdout, "");
        let run = run_step(&fake, &launch(&d, Step::Cutover, None)).unwrap();
        assert_eq!(run.report, update_hook::Report::default(), "{stdout:?}");
    }
}

/// Catches the two ways a step does not run being mistaken for each other: a command that
/// cannot be built never reaches the spawner, and a spawner that fails is a spawn error.
#[test]
fn a_step_that_does_not_run_says_why() {
    let d = sample();
    let fake = Fake::answering(Some(0), "", "");
    let relative = built(&d, ORDINARY, Step::Verify, Path::new("x"), None);
    let error = run_step(&fake, &relative).unwrap_err();
    assert!(matches!(error, RunError::Launch(_)), "{error}");
    assert!(fake.seen.borrow().is_empty());
    let failing = Fake {
        seen: RefCell::default(),
        answer: Err(io::ErrorKind::NotFound),
    };
    let error = run_step(&failing, &launch(&d, Step::Verify, None)).unwrap_err();
    assert!(matches!(error, RunError::Spawn(_)), "{error}");
    assert!(
        error
            .to_string()
            .contains("systemd-run could not be started")
    );
}

fn run_program(program: &str, args: &[&str]) -> io::Result<Finished> {
    let command = SystemdRun {
        args: args.iter().map(OsString::from).collect(),
    };
    Systemd::new(Path::new(program)).run(&command)
}

/// Catches the real spawner passing something to the program it was not told to: the arguments
/// are the command's, the environment is empty (`env` prints what it inherited, and nothing
/// follows its own arguments), and the input is nothing (`cat` returns at once).
#[test]
fn the_spawner_runs_the_program_with_the_arguments_and_nothing_inherited() {
    let echoed = run_program("/bin/echo", &["a", "b c"]).unwrap();
    assert_eq!((echoed.code, echoed.stdout), (Some(0), b"a b c\n".to_vec()));
    let env = run_program("/usr/bin/env", &["ONLY=this"]).unwrap();
    assert_eq!(String::from_utf8(env.stdout).unwrap(), "ONLY=this\n");
    let cat = run_program("/bin/cat", &[]).unwrap();
    assert_eq!((cat.code, cat.stdout), (Some(0), Vec::new()));
}

/// Catches the exit status and the error output being lost, and a missing program being
/// swallowed.
#[test]
fn the_spawner_returns_the_status_the_error_output_and_a_missing_program() {
    assert_eq!(run_program("/usr/bin/true", &[]).unwrap().code, Some(0));
    assert_eq!(run_program("/usr/bin/false", &[]).unwrap().code, Some(1));
    let ls = run_program("/bin/ls", &["/no/such/path/for/the/test"]).unwrap();
    assert_ne!(ls.code, Some(0));
    assert!(String::from_utf8_lossy(&ls.stderr).contains("no/such/path"));
    let missing = run_program("/no/such/program", &[]).unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    assert!(format!("{:?}", Systemd::system().clone()).contains("/usr/bin/systemd-run"));
}

/// Catches the launcher granting a hook the right to change unit files: a polkit rule scoped to
/// the listed units fails the moment `manage-unit-files` is granted, which implies every unit, so
/// no step is given anything about polkit or unit files, and the command names neither.
#[test]
fn no_step_is_given_a_polkit_or_unit_file_right() {
    for step in Step::ALL {
        let args = args_of(&sample(), step, None).join(" ");
        for word in ["manage-unit-files", "polkit", "ReadWritePaths=/etc", "mask"] {
            assert!(!args.contains(word), "{step}: {word} in {args}");
        }
        for property in properties(&sample(), step) {
            assert!(
                !property.contains("unit-files") && !property.contains("polkit"),
                "{property}"
            );
        }
    }
}

/// Catches a step started on targets that changed since the load: a link that pointed somewhere
/// harmless when the declaration was read and points at `/etc` when the step is about to run. The
/// launch is refused and nothing is spawned; before the change it runs.
#[test]
fn a_step_is_not_started_when_a_target_changed_since_the_load() {
    let scratch = Scratch::new("run-recheck");
    let real = scratch.dir("real", 0o755);
    let link = scratch.path().join("link");
    symlink(&real, &link).unwrap();
    let d = Declaration::parse(&with(
        "\"/etc/tierstore\"",
        &format!("\"{}\"", link.display()),
    ))
    .unwrap();
    let fake = Fake::answering(Some(0), "", "");
    run_step(&fake, &launch(&d, Step::Verify, None)).expect("the targets are as loaded");
    assert_eq!(fake.seen.borrow().len(), 1);
    std::fs::remove_file(&link).unwrap();
    symlink("/etc", &link).unwrap();
    let error = run_step(&fake, &launch(&d, Step::Verify, None)).unwrap_err();
    assert!(
        matches!(error, RunError::Recheck(Refusal::Reserved { .. })),
        "{error}"
    );
    assert!(error.to_string().contains("changed since the load"));
    assert_eq!(
        fake.seen.borrow().len(),
        1,
        "nothing was spawned for the refused step"
    );
}

/// Catches the launcher not telling a root hook from another: for a hook that runs as root a path
/// below a directory an ordinary account owns is refused at launch, and for another hook it is
/// not.
#[test]
fn a_root_hook_is_launched_only_below_directories_root_controls() {
    let scratch = Scratch::new("run-root");
    let inside = scratch.path().join("data");
    let d = Declaration::parse(&with(
        "\"/etc/tierstore\"",
        &format!("\"{}\"", inside.display()),
    ))
    .unwrap();
    let fake = Fake::answering(Some(0), "", "");
    run_step(&fake, &launch(&d, Step::Verify, None)).expect("an ordinary hook");
    let root = built(&d, 0, Step::Verify, Path::new(RELEASE), None);
    assert!(root.root_hook());
    assert!(!launch(&d, Step::Verify, None).root_hook());
    let error = run_step(&fake, &root).unwrap_err();
    assert!(
        matches!(error, RunError::Recheck(Refusal::Ancestor { .. })),
        "{error}"
    );
    assert_eq!(fake.seen.borrow().len(), 1);
}

/// Catches the root rules depending on how a `Launch` is built: the hook's uid is all it is built
/// from, so a hook of uid 0 is a root hook whatever a caller meant, and no uid but 0 is.
#[test]
fn a_launch_is_a_root_launch_exactly_when_the_hook_uid_is_zero() {
    let d = sample();
    for (uid, root) in [(0, true), (1, false), (65534, false), (u32::MAX, false)] {
        let launch = built(&d, uid, Step::Verify, Path::new(RELEASE), None);
        assert_eq!(launch.root_hook(), root, "uid {uid}");
    }
}
