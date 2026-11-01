//! The pieces of the root part on their own: the launcher that ties a step of the runner to its
//! declaration, the staged releases and their seals, the reading of a record after a boot, the
//! agent's socket, the registration socket, and the binary's `root` and `approve` words.

mod root_support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, symlink};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use kbf_step_runner::record::{HookSel, Loaded, Progress, Record, RecordStore, SCHEMA, State};
use kbf_step_runner::register::Declared as _;
use kbf_step_runner::{LaunchError, Stamp, StepCall, StepLauncher};
use kbf_updater::root::boot::{BOOT_ID_FILE, first_start_since_boot, read_as_killed};
use kbf_updater::root::flags::{self, Register};
use kbf_updater::root::launch::{Grants, HookLauncher, Shared};
use kbf_updater::root::releases::{Releases, is_version};
use kbf_updater::root::socket::{self, Answer, Request};
use root_support::{Systemctl, World, accounts, dir, my_gid, my_uid, put};
use update_hook::{Invocation, Step, Verdict};
use update_rights::{Finished, Policy, Spawner, SystemdRun, load_dir};

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// The world's declarations, loaded as the root part loads them.
fn loaded(world: &World) -> Shared {
    let accounts = accounts();
    let policy = Policy {
        trusted_uid: my_uid(),
        accounts: &accounts,
    };
    Arc::new(RwLock::new(
        load_dir(&world.path("updater.d"), &policy).unwrap(),
    ))
}

/// A `systemd-run` that answers every run with `code` and keeps the commands.
struct Answering {
    code: Option<i32>,
    seen: Mutex<Vec<SystemdRun>>,
}

impl Spawner for Answering {
    fn run(&self, command: &SystemdRun) -> std::io::Result<Finished> {
        self.seen.lock().unwrap().push(command.clone());
        Ok(Finished {
            code: self.code,
            stdout: b"noise\n{\"reason\":\"said\"}\n\n".to_vec(),
            stderr: Vec::new(),
        })
    }
}

fn launcher(world: &World, code: Option<i32>, stop_code: i32) -> (HookLauncher, Arc<Answering>) {
    let spawner = Arc::new(Answering {
        code,
        seen: Mutex::default(),
    });
    let launcher = HookLauncher::new(
        loaded(world),
        spawner.clone(),
        Systemctl::answering(stop_code),
    );
    (launcher, spawner)
}

fn call<'a>(service: &'a str, executable: &'a Path, unit: &'a str, step: Step) -> StepCall<'a> {
    StepCall {
        service,
        invocation: Invocation {
            step,
            artifact: None,
            params: Some(PathBuf::from("/run/params.toml")),
        },
        executable,
        unit,
        timeout_secs: 60,
    }
}

/// Catches a step run under a declaration it is not tied to: a service nothing declares, a unit
/// name that is not the runner's name for that step, and an executable that is not the declared
/// hook of a release are each refused without anything being spawned.
#[test]
fn the_launcher_refuses_a_step_it_cannot_tie_to_its_declaration() {
    let world = World::new("launch-refuse");
    world.declare("svc", &world.tierstore_shaped());
    let (launcher, spawner) = launcher(&world, Some(0), 0);
    let hook = Path::new("/var/lib/updater/releases/svc/2.0.0/hook");
    for (service, executable, unit, step) in [
        ("nope", hook, "updater-nope-verify-1", Step::Verify),
        ("svc", hook, "updater-svc-cutover-1", Step::Verify),
        ("svc", hook, "updater-svc-verify-01", Step::Verify),
        ("svc", hook, "updater-svc-verify-x", Step::Verify),
        (
            "svc",
            Path::new("/var/lib/updater/releases/svc/2.0.0/other"),
            "updater-svc-verify-1",
            Step::Verify,
        ),
    ] {
        let refused = launcher
            .run(&call(service, executable, unit, step))
            .unwrap_err();
        assert!(
            matches!(refused, LaunchError::Launcher(_)),
            "{service} {unit}: {refused}"
        );
    }
    assert!(spawner.seen.lock().unwrap().is_empty());
}

/// Catches the exit status and the last line being lost on the way to the runner, and a hook
/// that could not be executed being read as one that ran and failed: a step's code and its last
/// line that is not blank come back as they are, and `203` (systemd's `EXEC`) is a hook that did
/// not start, which the runner treats as the one reason to try the previous release's hook.
#[test]
fn the_launcher_gives_the_runner_the_code_and_the_last_line() {
    let world = World::new("launch-exit");
    world.declare("svc", &world.tierstore_shaped());
    fs::create_dir_all(world.hook_dir("svc")).unwrap();
    let hook = Path::new("/var/lib/updater/releases/svc/2.0.0/hook");
    for code in [Some(0), Some(1), Some(75), None] {
        let (launcher, spawner) = launcher(&world, code, 0);
        let exit = launcher
            .run(&call("svc", hook, "updater-svc-verify-3", Step::Verify))
            .unwrap();
        assert_eq!(exit.code, code);
        assert_eq!(exit.last_line.as_deref(), Some("{\"reason\":\"said\"}"));
        let seen = spawner.seen.lock().unwrap();
        assert!(root_support::words(&seen[0]).contains(&"--unit=updater-svc-verify-3".to_owned()));
    }
    let (launcher, _) = launcher(&world, Some(203), 0);
    let error = launcher
        .run(&call("svc", hook, "updater-svc-rollback-3", Step::Rollback))
        .unwrap_err();
    assert!(matches!(error, LaunchError::NoStart(_)), "{error}");
}

/// Catches a step started with a record directory the service's account can write: the check
/// is made again at every launch, and nothing is spawned.
#[test]
fn the_launcher_refuses_a_record_directory_the_service_can_write() {
    let world = World::new("launch-state");
    world.declare("svc", &world.tierstore_shaped());
    let (launcher, spawner) = launcher(&world, Some(0), 0);
    // Opened to the service's account after the load, as a step is about to start.
    dir(&world.hook_dir("svc"), 0o777);
    let hook = Path::new("/var/lib/updater/releases/svc/2.0.0/hook");
    let error = launcher
        .run(&call("svc", hook, "updater-svc-prepare-1", Step::Prepare))
        .unwrap_err();
    assert!(error.to_string().contains("can be written"), "{error}");
    assert!(spawner.seen.lock().unwrap().is_empty());
}

/// Catches a stop that is not a stop: `systemctl stop` that succeeds, or finds no such unit, is a
/// stopped unit; any other answer is an error the runner stops on.
#[test]
fn a_unit_is_stopped_by_name_and_one_that_is_not_loaded_needs_no_stop() {
    let world = World::new("launch-stop");
    for (code, stopped) in [(0, true), (5, true), (1, false)] {
        let systemctl = Systemctl::answering(code);
        let launcher = HookLauncher::new(
            loaded(&world),
            Arc::new(Answering {
                code: Some(0),
                seen: Mutex::default(),
            }),
            systemctl.clone(),
        );
        assert_eq!(
            launcher.stop("updater-svc-verify-2").is_ok(),
            stopped,
            "{code}"
        );
        assert_eq!(systemctl.stopped(), ["updater-svc-verify-2"]);
    }
}

/// Catches the registration socket answering from anything but the declarations: a declared
/// service is granted its account's uid, its health ports and its registration time to live, and
/// a name nothing declares is granted nothing.
#[test]
fn the_registration_grants_are_the_declarations() {
    let world = World::new("grants");
    let text = world.tierstore_shaped().replace(
        "promotion_ceiling_secs = 3600\n",
        "promotion_ceiling_secs = 3600\nhealth_ports = [9301]\nregistration_ttl_secs = 120\n",
    );
    world.declare("svc", &text);
    let grants = Grants(loaded(&world));
    let grant = grants.grant("svc").expect("svc is declared");
    assert_eq!(grant.uid, 990);
    assert_eq!(grant.health_ports, [9301]);
    assert_eq!(grant.registration_ttl_secs, 120);
    assert!(grants.grant("nope").is_none());
}

/// Catches a registration socket that the root part does not serve, or serves for anyone: a
/// declared service registers its liveness and version with the root part, and a name nothing
/// declares is refused.
#[test]
fn the_root_part_serves_registrations_of_declared_services() {
    let world = World::new("register");
    let text = world
        .declaration("reg", "", "user = \"reg-update\"\ngroup = \"reg-update\"\n")
        .replace("uid = \"svc\"", "uid = \"me\"")
        .replace(
            &world.hook_dir("reg").display().to_string(),
            "/var/lib/updater/hooks/reg",
        );
    world.declare("reg", &text);
    let root = world.root_part();
    assert_eq!(root.services(), ["reg"], "{:?}", root.refused());
    let run = world.path("register");
    dir(&run, 0o750);
    let sock = run.join("register.sock");
    root.serve_registrations(&sock, my_gid()).unwrap();
    let said = |line: &str| {
        let mut client = UnixStream::connect(&sock).unwrap();
        writeln!(client, "{line}").unwrap();
        let mut answer = String::new();
        BufReader::new(&client).read_line(&mut answer).unwrap();
        serde_json::from_str::<serde_json::Value>(&answer).unwrap()
    };
    let ok = said(r#"{"name":"reg","version":"1.0.0"}"#);
    assert_eq!(ok["ok"], true, "{ok}");
    let unknown = said(r#"{"name":"nope","version":"1.0.0"}"#);
    assert_eq!(unknown["error"], "unknown_service", "{unknown}");
    assert!(
        root.serve_registrations(&world.path("register.sock"), my_gid())
            .is_err(),
        "a socket directory others may enter is refused"
    );
}

/// Catches a release name that leads somewhere else, or a seal that is not trusted or not a seal:
/// a version is a plain file name and not `current`, the seal must be the trusted account's and
/// closed to others, and the `current` link must name a staged release.
#[test]
fn a_release_is_found_only_by_a_version_and_a_trusted_seal() {
    for good in ["2.0.0", "1.2.3-rc.1+g0abc", "v1_2"] {
        assert!(is_version(good), "{good}");
    }
    let long = "1".repeat(129);
    for bad in ["", ".hidden", "a/b", "..", "current", "1 2", long.as_str()] {
        assert!(!is_version(bad), "{bad}");
    }
    let world = World::new("releases");
    world.stage("svc", "1.0.0", None);
    let releases = Releases::new(&world.path("releases"), my_uid());
    let found = releases.staged("svc", "1.0.0").unwrap();
    assert_eq!(found.version, "1.0.0");
    assert_eq!(found.staged.dir, world.path("releases/svc/1.0.0"));
    assert_eq!(found.artifact_digest, "ab".repeat(32));
    assert!(releases.staged("svc", "../svc/1.0.0").is_err());
    assert!(releases.staged("svc", "9.9.9").is_err());
    assert!(
        Releases::new(&world.path("releases"), my_uid() + 1)
            .staged("svc", "1.0.0")
            .is_err(),
        "a seal of another account"
    );
    let seal = world.path("releases/svc/1.0.0.seal");
    let text = fs::read(&seal).unwrap();
    put(&seal, &text, 0o664);
    assert!(
        releases.staged("svc", "1.0.0").is_err(),
        "a seal others may write"
    );
    put(&seal, b"{\"tree_digest\":\"x\"}", 0o644);
    assert!(releases.staged("svc", "1.0.0").is_err(), "not a seal");

    assert_eq!(releases.current("svc").unwrap(), None);
    world.stage("svc", "1.0.0", None);
    releases.set_current("svc", "1.0.0").unwrap();
    releases.set_current("svc", "1.0.0").unwrap();
    assert_eq!(releases.current("svc").unwrap().unwrap().version, "1.0.0");
    let current = world.path("releases/svc/current");
    fs::remove_file(&current).unwrap();
    symlink("9.9.9", &current).unwrap();
    assert!(releases.current("svc").is_err(), "a link to no release");
    fs::remove_file(&current).unwrap();
    fs::write(&current, "not a link").unwrap();
    assert!(releases.current("svc").is_err(), "not a link");
}

fn record(step: Step, state: State, watching: bool) -> Record {
    Record {
        schema: SCHEMA,
        service: "svc".to_owned(),
        target_version: "2.0.0".to_owned(),
        artifact_digest: "cd".repeat(32),
        tarball_digest: "ab".repeat(32),
        previous_digest: None,
        generation: 4,
        params: PathBuf::from("/run/params.toml"),
        step,
        attempt: 1,
        state,
        hook: HookSel::New,
        progress: Progress {
            watching: watching.then(|| Stamp {
                boot_id: "boot-a".to_owned(),
                mono_secs: 1,
            }),
            ..Progress::default()
        },
    }
}

/// Catches the boot rule reading the wrong steps as killed: a running step of any kind and a
/// `cutover` about to start are read as killed (no code, no last line), and a step about to start
/// that is not `cutover`, a step of the post-success watcher, a finished update and no record at
/// all are left as they are.
#[test]
fn after_a_boot_a_running_step_and_a_pending_cutover_are_read_as_killed() {
    let world = World::new("killed");
    let store = RecordStore::new(&world.root);
    assert!(!read_as_killed(&store).unwrap(), "no record");
    let running = || State::Running {
        started: Stamp {
            boot_id: "boot-a".to_owned(),
            mono_secs: 5,
        },
        deadline_secs: 600,
    };
    let finished = State::Finished {
        verdict: Verdict::Halt,
        reason: None,
    };
    for (step, state, watching, killed) in [
        (Step::Prepare, running(), false, true),
        (Step::Rollback, running(), false, true),
        (Step::Cutover, State::Pending, false, true),
        (Step::Verify, State::Pending, false, false),
        (Step::Verify, running(), true, false),
        (Step::Cleanup, finished.clone(), false, false),
    ] {
        let before = record(step, state, watching);
        store.save(&before).unwrap();
        assert_eq!(read_as_killed(&store).unwrap(), killed, "{step} {before:?}");
        let Loaded::Current(after) = store.load().unwrap() else {
            panic!("a record");
        };
        let want = if killed {
            State::Done {
                code: None,
                last_line: None,
            }
        } else {
            before.state.clone()
        };
        assert_eq!(after.state, want, "{step}");
    }
}

/// Catches a restart taken for a boot, or a boot taken for a restart: the first start, and a
/// start under another boot id, are the first since a boot; a start under the same id is not.
#[test]
fn a_start_is_the_first_since_a_boot_when_the_boot_id_changed() {
    let world = World::new("boot-id");
    let state = world.root.clone();
    assert!(first_start_since_boot(&state, &*world.clock).unwrap());
    assert!(!first_start_since_boot(&state, &*world.clock).unwrap());
    world.clock.reboot("boot-b");
    assert!(first_start_since_boot(&state, &*world.clock).unwrap());
    assert_eq!(
        fs::read_to_string(state.join(BOOT_ID_FILE)).unwrap(),
        "boot-b"
    );
    assert!(first_start_since_boot(&world.path("missing"), &*world.clock).is_err());
}

/// Catches an agent's socket that someone else can use, or that takes more than a service and a
/// version: the socket is mode 0600 and owned by the agent's account; an update, a status and
/// their answers go through it; a request that names a step, a command or anything else is
/// refused as malformed and never reaches the root part.
#[test]
fn the_agents_socket_is_0600_and_takes_only_a_service_and_a_version() {
    let world = World::new("socket");
    let path = world.path("root.sock");
    put(&path, b"a stale file", 0o644);
    let listener = socket::bind(&path, my_uid(), my_gid()).unwrap();
    let meta = fs::symlink_metadata(&path).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.mode() & 0o777, 0o600);
    assert_eq!((meta.uid(), meta.gid()), (my_uid(), my_gid()));
    let asked: Arc<Mutex<Vec<Request>>> = Arc::default();
    let kept = asked.clone();
    let server = std::thread::spawn(move || {
        for stream in listener.incoming().take(5) {
            let kept = kept.clone();
            socket::serve_connection(&stream.unwrap(), &move |request| {
                kept.lock().unwrap().push(request.clone());
                Answer {
                    ok: true,
                    state: Some("pending".to_owned()),
                    ..Answer::default()
                }
            })
            .unwrap();
        }
    });
    let update = Request::Update {
        service: "svc".to_owned(),
        version: "2.0.0".to_owned(),
    };
    let status = Request::Status {
        service: "svc".to_owned(),
    };
    for request in [&update, &status] {
        let answer = socket::request(&path, request).unwrap();
        assert_eq!(answer.state.as_deref(), Some("pending"));
    }
    for line in [
        r#"{"update":{"service":"svc","version":"2.0.0","step":"cleanup"}}"#,
        r#"{"run":{"command":"/bin/true"}}"#,
        "not json",
    ] {
        let mut client = UnixStream::connect(&path).unwrap();
        writeln!(client, "{line}").unwrap();
        let mut answer = String::new();
        BufReader::new(&client).read_line(&mut answer).unwrap();
        let answer: Answer = serde_json::from_str(&answer).unwrap();
        assert_eq!(answer.token(), Some("malformed"), "{line}");
        assert!(!answer.ok);
    }
    server.join().unwrap();
    assert_eq!(*asked.lock().unwrap(), [update, status]);
}

/// Catches requests the root part answers wrongly before it runs anything: an unknown service, a
/// release that is not staged, a status with no update yet, a damaged record, and an update of a
/// declared service while another of its updates is unfinished.
#[test]
fn the_root_part_refuses_what_it_cannot_run() {
    let world = World::new("refusals");
    world.declare("svc", &world.tierstore_shaped());
    world.stage("svc", "2.0.0", None);
    world.stage("nope", "2.0.0", None);
    let root = world.root_part();
    let ask = |request: Request| root.answer(&request);
    let update = |service: &str, version: &str| {
        ask(Request::Update {
            service: service.to_owned(),
            version: version.to_owned(),
        })
    };
    let status = |service: &str| {
        ask(Request::Status {
            service: service.to_owned(),
        })
    };
    assert_eq!(update("nope", "2.0.0").token(), Some("unknown_service"));
    assert_eq!(update("Bad Name", "2.0.0").token(), Some("unknown_service"));
    assert_eq!(update("svc", "3.0.0").token(), Some("not_staged"));
    assert_eq!(status("nope").token(), Some("unknown_service"));
    let never = status("svc");
    assert!(never.ok && never.step.is_none(), "{never:?}");
    assert_eq!(root.start("nope", false).token(), Some("unknown_service"));

    world.tell("svc", "prepare", "75 {\"retry_after_secs\":5}");
    let deferred = update("svc", "2.0.0");
    assert!(
        matches!(deferred.verdict, Some(Verdict::Deferred { .. })),
        "{deferred:?}"
    );
    let state = world.path("state/svc/step-record.json");
    fs::write(&state, b"{ damaged").unwrap();
    assert_eq!(status("svc").token(), Some("record"));
    assert_eq!(root.start("svc", false).token(), Some("record"));
}

/// Catches two updates of one service at once: while an update of `svc` is unfinished (the root
/// part was killed in its `prepare`), an order for another release is refused as busy and runs
/// nothing, and the status says where the unfinished one is.
#[test]
fn an_order_for_another_release_while_one_is_unfinished_is_busy() {
    let world = World::new("busy");
    world.declare("svc", &world.tierstore_shaped());
    world.stage("svc", "2.0.0", None);
    world.stage("svc", "3.0.0", None);
    let gate =
        |point: kbf_step_runner::CrashPoint| point == kbf_step_runner::CrashPoint::AfterWrite(2);
    let killed = world.root_part_gated(Arc::new(gate)).update("svc", "2.0.0");
    assert_eq!(killed.token(), Some("runner"), "{killed:?}");
    let root = world.root_part();
    let status = root.status("svc");
    assert_eq!(status.step, Some(Step::Prepare), "{status:?}");
    assert_eq!(status.state.as_deref(), Some("running"));
    assert_eq!(status.verdict, None);
    let busy = root.update("svc", "3.0.0");
    assert_eq!(busy.token(), Some("busy"), "{busy:?}");
    assert!(world.steps("svc").is_empty(), "nothing ran");
}

fn passwd_name(id: u32, file: &str) -> String {
    fs::read_to_string(file)
        .unwrap()
        .lines()
        .find_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.get(2) == Some(&id.to_string().as_str())).then(|| fields[0].to_owned())
        })
        .unwrap_or_else(|| panic!("no name for {id} in {file}"))
}

/// Catches the binary's `root` word not starting the root part: with its flags it loads the
/// declarations, binds the agent's socket for the account named, answers the agent and stops when
/// told; without them it is a usage error, and with an account or a registration socket it cannot
/// use it does not start.
#[test]
fn kbf_updater_root_serves_the_agent_until_stopped() {
    let world = World::new("binary");
    let socket = world.path("root.sock");
    let me = passwd_name(my_uid(), "/etc/passwd");
    let args = |extra: &[&str]| {
        let mut args = strings(&[
            "kbf-updater",
            "root",
            "--socket",
            &socket.display().to_string(),
            "--socket-user",
            &me,
            "--declarations",
            &world.path("updater.d").display().to_string(),
            "--state-dir",
            &world.path("state").display().to_string(),
            "--releases-dir",
            &world.path("releases").display().to_string(),
        ]);
        args.extend(strings(extra));
        args
    };
    let socket_path = socket.clone();
    let (sent, answered) = std::sync::mpsc::channel();
    let (code, text) = kbf_updater::run_root_with(&args(&[]), move |stop| {
        std::thread::spawn(move || {
            let answer = socket::request(
                &socket_path,
                &Request::Status {
                    service: "svc".to_owned(),
                },
            );
            stop.stop();
            sent.send(answer.map_err(|e| e.to_string())).unwrap();
        });
    });
    assert_eq!((code, text.as_str()), (0, ""));
    let answer = answered.recv().unwrap().unwrap();
    assert_eq!(answer.token(), Some("unknown_service"), "{answer:?}");
    assert_eq!(fs::symlink_metadata(&socket).unwrap().mode() & 0o777, 0o600);

    let (code, text) = kbf_updater::run(&strings(&["kbf-updater", "root"]));
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("--socket is required"), "{text}");
    let mut nobody = args(&[]);
    nobody[5] = "no-such-account".to_owned();
    let (code, text) = kbf_updater::run_root_with(&nobody, |_| {});
    assert_eq!(code, 1, "{text}");
    let group = passwd_name(my_gid(), "/etc/group");
    let register = world.path("open");
    dir(&register, 0o755);
    let (code, text) = kbf_updater::run_root_with(
        &args(&[
            "--register-socket",
            &register.join("r.sock").display().to_string(),
            "--register-group",
            &group,
        ]),
        |_| {},
    );
    assert_eq!(code, 1, "{text}");
    let (code, text) = kbf_updater::run_root_with(
        &args(&[
            "--register-group",
            "no-such-group",
            "--register-socket",
            "/x",
        ]),
        |_| {},
    );
    assert_eq!(code, 1, "{text}");
}

/// Catches flags read wrongly: the defaults are the design's paths, and a registration socket is
/// given with its group or not at all.
#[test]
fn the_root_flags_are_read_with_the_designs_defaults() {
    let parsed = flags::parse(&strings(&[
        "--socket=/run/r.sock",
        "--socket-user",
        "kbf-updater",
    ]))
    .unwrap();
    assert_eq!(parsed.socket, Path::new("/run/r.sock"));
    assert_eq!(parsed.socket_user, "kbf-updater");
    assert_eq!(parsed.declarations, Path::new("/etc/updater.d"));
    assert_eq!(parsed.approvals, Path::new("/etc/updater.d/approved"));
    assert_eq!(parsed.state, Path::new("/var/lib/updater/state"));
    assert_eq!(parsed.releases, Path::new("/var/lib/updater/releases"));
    assert_eq!(parsed.register, None);
    let with = flags::parse(&strings(&[
        "--socket=/s",
        "--socket-user=u",
        "--register-socket=/run/updater/register.sock",
        "--register-group=updater-register",
    ]))
    .unwrap();
    assert_eq!(
        with.register,
        Some(Register {
            socket: PathBuf::from("/run/updater/register.sock"),
            group: "updater-register".to_owned(),
        })
    );
    for bad in [
        &["--socket=/s"][..],
        &["--socket-user=u"],
        &["--socket=/s", "--socket-user=u", "--register-socket=/r"],
        &["--socket=/s", "--socket-user=u", "--step=cleanup"],
    ] {
        assert!(flags::parse(&strings(bad)).is_err(), "{bad:?}");
    }
}

/// Catches `approve` doing anything but the update-rights command: a usage error is exit 2, an
/// approval by an account that does not own the record is refused (exit 1), and the binary routes
/// the word to it.
#[test]
fn kbf_updater_approve_is_the_update_rights_command() {
    let world = World::new("approve");
    let env = update_rights::cli::Env {
        approvals: world.path("updater.d/approved"),
        trusted_uid: my_uid() + 1,
        effective_uid: my_uid(),
    };
    let digest = "ab".repeat(32);
    let (code, text) =
        kbf_updater::run_approve_with(&strings(&["kbf-updater", "approve", "svc", &digest]), &env);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("not approved"), "{text}");
    let (code, text) = kbf_updater::run(&strings(&["kbf-updater", "approve"]));
    assert_eq!(code, 2, "{text}");
    assert!(text.contains("usage: approve"), "{text}");
}

/// Catches the start of the root part skipping a service, or serving the agent before the start
/// pass: with a good declaration and a refused one, it serves the registrations, reads the good
/// service's record (writing the boot id only after that), binds the agent's socket for the
/// account named and answers the agent until stopped.
#[test]
fn the_root_part_runs_the_start_pass_before_it_answers_the_agent() {
    let world = World::new("serve-start");
    world.declare("svc", &world.tierstore_shaped());
    put(&world.path("updater.d/other.toml"), b"x", 0o666);
    let run = world.path("run");
    dir(&run, 0o750);
    let socket = world.path("root.sock");
    let path = |p: &Path| p.display().to_string();
    let flags = flags::parse(&strings(&[
        "--socket",
        &path(&socket),
        "--socket-user",
        "me",
        "--register-socket",
        &path(&run.join("register.sock")),
        "--register-group",
        "me",
    ]))
    .unwrap();
    let (sent, answered) = std::sync::mpsc::channel();
    let socket_path = socket.clone();
    let boot_id = world.path("state").join(BOOT_ID_FILE);
    let (code, text) =
        kbf_updater::root::run_config(&flags, world.config(), world.parts(None), move |stop| {
            let committed = boot_id.exists();
            std::thread::spawn(move || {
                let answer = socket::request(
                    &socket_path,
                    &Request::Status {
                        service: "svc".to_owned(),
                    },
                );
                stop.stop();
                sent.send((committed, answer.map_err(|e| e.to_string())))
                    .unwrap();
            });
        });
    assert_eq!((code, text.as_str()), (0, ""));
    let (committed, answer) = answered.recv().unwrap();
    assert!(
        committed,
        "the boot id is written once the start pass is over"
    );
    let answer = answer.unwrap();
    assert!(answer.ok, "{answer:?}");
    assert!(run.join("register.sock").exists());
}
