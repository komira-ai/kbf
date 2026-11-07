//! `job-service apply` and `back-out` on a scripted host: the tools' answers and the HTTP
//! statuses are the test's, the job service is `kbf-job-client`'s in-process fake on loopback,
//! and the pause between two asks is the hook that moves the canary job. No test reads a
//! clock or sleeps.

use std::collections::{BTreeMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use kbf_host::testkit::reply;
use kbf_job_client::fake::{Call, FakeJobServer, Fault, Machine, TOKEN};

use super::*;
use crate::cutover::tests::{KBF_IMAGE, ready_config, scratch, write};

/// The canary job's id at the instant the tests run at.
const CANARY: &str = "kbf-canary-1767225600";
const NOW: u64 = 1_767_225_600;

/// How a scripted tool answers.
type Answer<'a> = Box<dyn Fn(&Cmd) -> io::Result<Output> + 'a>;
/// What a case changes in the flags and the host.
type Setup = Box<dyn Fn(&mut ApplyArgs, &mut Script<'_>)>;
/// What a case changes in the restart flags.
type RestartSetup = Box<dyn Fn(&mut Restart)>;

/// A host whose tools answer by `answer`, whose URLs answer from per-path queues (the last
/// answer repeats), and whose pause calls `on_pause`.
struct Script<'a> {
    answer: Answer<'a>,
    statuses: BTreeMap<String, VecDeque<io::Result<u16>>>,
    on_pause: Box<dyn FnMut() + 'a>,
    runs: Vec<Cmd>,
    asked: Vec<String>,
    pauses: Vec<u64>,
}

fn refused() -> io::Result<u16> {
    Err(io::ErrorKind::ConnectionRefused.into())
}

impl<'a> Script<'a> {
    /// Every tool succeeds (`kbf-host-install` answers `restarted`); the frontend is up, then
    /// exits after one more ask; it is ready, and its first worker is there, at once.
    fn healthy() -> Script<'a> {
        let mut statuses = BTreeMap::new();
        statuses.insert("/healthz".to_owned(), VecDeque::from([Ok(200), refused()]));
        statuses.insert("/readyz".to_owned(), VecDeque::from([Ok(503), Ok(200)]));
        statuses.insert(
            "/v1/nodes/js:fe-a-0".to_owned(),
            VecDeque::from([Ok(404), Ok(200)]),
        );
        Script {
            answer: Box::new(|cmd: &Cmd| {
                Ok(if cmd.program.as_str() == "/usr/bin/ssh" {
                    reply(
                        0,
                        "{\"result\":\"restarted\",\"unit\":\"kbf-frontend.service\"}\n",
                        "",
                    )
                } else {
                    reply(0, "", "")
                })
            }),
            statuses,
            on_pause: Box::new(|| {}),
            runs: Vec::new(),
            asked: Vec::new(),
            pauses: Vec::new(),
        }
    }

    fn answers(mut self, path: &str, answers: Vec<io::Result<u16>>) -> Self {
        self.statuses.insert(path.to_owned(), answers.into());
        self
    }
}

impl Host for Script<'_> {
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
        self.runs.push(cmd.clone());
        (self.answer)(cmd)
    }

    fn status(&mut self, url: &HttpUrl) -> io::Result<u16> {
        self.asked.push(url.path().to_owned());
        let queue = self.statuses.get_mut(url.path()).expect("an asked path");
        match queue.len() {
            1 => match &queue[0] {
                Ok(code) => Ok(*code),
                Err(e) => Err(e.kind().into()),
            },
            _ => queue.pop_front().unwrap(),
        }
    }

    fn pause(&mut self, secs: u64) {
        self.pauses.push(secs);
        (self.on_pause)();
    }
}

/// A job service with one host the canary fits on.
async fn service() -> FakeJobServer {
    let server = FakeJobServer::start().await.unwrap();
    server.add_host(
        "host-a",
        Machine {
            shape: js_proto::v1::Shape {
                cpu_millis: 4000,
                memory_bytes: 16 << 30,
                disk_bytes: Some(100 << 30),
                ..Default::default()
            },
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: js_proto::v1::ExecMode::Container,
        },
    );
    server
}

/// Moves the canary job to the end `exit` at each pause, once it is submitted.
fn finishes(server: &FakeJobServer, exit: i32) -> impl FnMut() + '_ {
    move || {
        server.place(CANARY, "host-a");
        server.starting(CANARY);
        server.run(CANARY);
        server.finish(CANARY, exit);
    }
}

fn token_file(dir: &Path, text: &str) -> std::path::PathBuf {
    let path = dir.join("job-service.token");
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
    path
}

pub(super) fn restart() -> Restart {
    let p = |s: &str| HostPath::new(s).unwrap();
    Restart {
        instance: "-".to_owned(),
        api: "127.0.0.1:8982".parse().unwrap(),
        api_token_file: p("/etc/kbf/api-token"),
        ssh_host: "fe.farm.example".to_owned(),
        ssh_port: Some(2222),
        ssh_user: "kbf-updater".to_owned(),
        ssh_key_file: p("/var/lib/kbf-updater/id_ed25519"),
        known_hosts: p("/var/lib/kbf-updater/known_hosts"),
        kbf: p("/usr/bin/kbf"),
        ssh: p("/usr/bin/ssh"),
        poll_secs: 5,
        drain_deadline_secs: None,
        ready_deadline_secs: 20,
    }
}

/// `apply`'s flags for the file `config` and the service at `url`.
fn args(dir: &Path, config: &Path, url: &str, apply: bool) -> ApplyArgs {
    ApplyArgs {
        target: Target {
            config: config.to_path_buf(),
            url: url.to_owned(),
            token_file: token_file(dir, TOKEN),
            ca_file: None,
            hosts: 4,
            host_cpus: 16,
            host_memory_gib: 64,
            host_slots: 8,
        },
        restart: restart(),
        canary_deadline_secs: 30,
        apply,
    }
}

/// `apply` on `host`: its result and stdout.
async fn applied(a: &ApplyArgs, host: &mut dyn Host) -> (Result<(), CutoverError>, String) {
    let mut out = Vec::new();
    let done = apply(a, host, NOW, &mut out).await;
    (done, String::from_utf8(out).unwrap())
}

/// The ssh argument vector of the restart: the updater's, with the request as one argument.
fn ssh_args() -> Vec<String> {
    [
        "-F",
        "/dev/null",
        "-i",
        "/var/lib/kbf-updater/id_ed25519",
        "-o",
        "BatchMode=yes",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "UserKnownHostsFile=/var/lib/kbf-updater/known_hosts",
        "-o",
        "GlobalKnownHostsFile=/dev/null",
        "-o",
        "ConnectTimeout=10",
        "-p",
        "2222",
        "-T",
        "--",
        "kbf-updater@fe.farm.example",
        "restart frontend=-",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Behaviour: without `--apply` the plan and every step are printed and nothing is run,
/// asked, written or called.
#[tokio::test]
async fn a_dry_run_runs_asks_and_writes_nothing() {
    let dir = scratch("apply-dry");
    let text = ready_config();
    let config = write(dir.path(), "kbf.toml", &text);
    let server = service().await;
    let a = args(dir.path(), &config, &server.origin(), false);
    let mut host = Script::healthy();
    let (done, out) = applied(&a, &mut host).await;
    done.unwrap();
    let file = std::fs::canonicalize(&config).unwrap();
    let dry: Vec<&str> = out.lines().filter(|l| l.starts_with("DRY-RUN: ")).collect();
    assert_eq!(
        dry,
        [
            format!(
                "DRY-RUN: check: ListJobs on {} with the token in {}",
                server.origin(),
                a.target.token_file.display()
            ),
            format!(
                "DRY-RUN: stage {f}.job-service-new and keep the original as {f}.pre-job-service",
                f = file.display()
            ),
            format!("DRY-RUN: /usr/bin/kbf --mode=frontend --config {}.job-service-new --check", file.display()),
            "DRY-RUN: rename the staged file over the config".to_owned(),
            "DRY-RUN: /usr/bin/kbf cluster --api 127.0.0.1:8982 drain --frontend --token-file /etc/kbf/api-token (unless http://127.0.0.1:8982/healthz refuses the connection)".to_owned(),
            "DRY-RUN: wait until http://127.0.0.1:8982/healthz refuses the connection (the frontend has exited), however long".to_owned(),
            format!("DRY-RUN: /usr/bin/ssh {} (env PATH, HOME, LANG)", ssh_args().join(" ").replace("restart frontend=-", "\"restart frontend=-\"")),
            "DRY-RUN: wait for http://127.0.0.1:8982/readyz to answer 200 (the frontend is ready), at most 4 asks".to_owned(),
            "DRY-RUN: wait for http://127.0.0.1:8982/v1/nodes/js:fe-a-0 to answer 200 (the executor of the job service registered its first worker), at most 4 asks".to_owned(),
            format!("DRY-RUN: canary: submit job {CANARY} ({KBF_IMAGE} --version, no network, label kbf-canary=fe-a) and ask after it at most 6 times"),
        ]
    );
    assert!(
        out.contains("\ndry run: nothing changed. Re-run with --apply.\n"),
        "{out}"
    );
    assert!(
        out.contains("back-out: kbf-deploy job-service back-out --config "),
        "{out}"
    );
    assert!(!out.contains(TOKEN), "{out}");
    assert!(host.runs.is_empty() && host.asked.is_empty() && host.pauses.is_empty());
    assert!(server.calls().is_empty());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    cutover_edit::require_no_backup(&file).unwrap();
}

/// Behaviour: `--apply` checks the service, switches the file (the original kept), drains
/// and waits for the exit, restarts through kbf-host-install, waits for readiness and the
/// first worker, and passes the canary; the token is in no output.
#[tokio::test]
async fn an_apply_switches_restarts_and_passes_the_canary() {
    let dir = scratch("apply");
    let text = ready_config();
    let config = write(dir.path(), "kbf.toml", &text);
    let server = service().await;
    let a = args(dir.path(), &config, &server.origin(), true);
    let mut host = Script::healthy();
    host.on_pause = Box::new(finishes(&server, 0));
    let (done, out) = applied(&a, &mut host).await;
    done.unwrap();
    let file = std::fs::canonicalize(&config).unwrap();
    let settings = Settings::of(&a.target).unwrap();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        cutover_edit::rewritten(&file, &text, &settings).unwrap()
    );
    let (_, kept) = cutover_edit::read_backup(&file).unwrap();
    assert_eq!(kept, text);
    let programs: Vec<(String, Vec<String>)> = host
        .runs
        .iter()
        .map(|c| (c.program.to_string(), c.args.clone()))
        .collect();
    let staged = format!("{}.job-service-new", file.display());
    assert_eq!(
        programs,
        [
            (
                "/usr/bin/kbf".to_owned(),
                ["--mode=frontend", "--config", &staged, "--check"]
                    .map(str::to_owned)
                    .to_vec()
            ),
            (
                "/usr/bin/kbf".to_owned(),
                [
                    "cluster",
                    "--api",
                    "127.0.0.1:8982",
                    "drain",
                    "--frontend",
                    "--token-file",
                    "/etc/kbf/api-token"
                ]
                .map(str::to_owned)
                .to_vec()
            ),
            ("/usr/bin/ssh".to_owned(), ssh_args()),
        ]
    );
    assert_eq!(
        host.asked,
        [
            "/healthz",
            "/healthz",
            "/readyz",
            "/readyz",
            "/v1/nodes/js:fe-a-0",
            "/v1/nodes/js:fe-a-0"
        ]
    );
    // The drain's ask sees the frontend up, and the exit wait sees it gone at once; one pause
    // in the readiness and the worker waits, and one before the canary's second ask.
    assert_eq!(host.pauses, [5, 5, 5]);
    let spec = server.submitted(CANARY).unwrap();
    assert_eq!(spec.image, KBF_IMAGE);
    assert!(!spec.labels.contains_key("kbf-frontend"));
    assert!(matches!(server.calls()[0], Call::ListJobs(_)));
    assert!(
        out.contains("\npass: the job service answered ListJobs (no job of this client yet)\n"),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "\npass: the canary job {CANARY} succeeded on host host-a\n"
        )),
        "{out}"
    );
    assert!(
        out.contains("\ndone: the frontend runs executor = \"js-agent\"\n"),
        "{out}"
    );
    assert!(out.ends_with("  --executor=kbf-daemon\n  no --executor-job-url\n  no --executor-job-token-file\n  no --executor-job-ca-file\n  no --executor-hosts\n  no --executor-host-cpus\n  no --executor-host-memory-gib\n  --executor-host-slots=8\n"), "{out}");
    assert!(!out.contains(TOKEN), "{out}");
}

/// Behaviour: a service that refuses the token stops the apply at its first step: the
/// failure names the cause, and nothing is run or written.
#[tokio::test]
async fn an_unauthorized_service_stops_the_apply_before_any_change() {
    let dir = scratch("apply-unauthorized");
    let text = ready_config();
    let config = write(dir.path(), "kbf.toml", &text);
    let server = service().await;
    server.inject_on(
        "ListJobs",
        Fault::Status {
            code: tonic::Code::Unauthenticated,
            message: String::new(),
        },
    );
    let a = args(dir.path(), &config, &server.origin(), true);
    let mut host = Script::healthy();
    let (done, _) = applied(&a, &mut host).await;
    let e = done.unwrap_err();
    assert_eq!(
        e.to_string(),
        format!(
            "check: unauthorized: the job service at {} refused the token in {} (Unauthenticated)",
            server.origin(),
            a.target.token_file.display()
        )
    );
    assert_eq!(e.code(), 1);
    assert!(host.runs.is_empty());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    cutover_edit::require_no_backup(&std::fs::canonicalize(&config).unwrap()).unwrap();
}

/// Behaviour: a staged file that `kbf --check` refuses fails the apply with kbf's words, and
/// leaves the file as it was with no backup or staged file.
#[tokio::test]
async fn a_staged_file_kbf_refuses_changes_nothing() {
    let dir = scratch("apply-check");
    let text = ready_config();
    let config = write(dir.path(), "kbf.toml", &text);
    let server = service().await;
    let a = args(dir.path(), &config, &server.origin(), true);
    let mut host = Script::healthy();
    host.answer = Box::new(|_| {
        Ok(reply(
            2,
            "",
            "--executor js-agent needs --pack-store-endpoint\n",
        ))
    });
    let (done, out) = applied(&a, &mut host).await;
    assert_eq!(
        done.unwrap_err().to_string(),
        "the config check: exit 2: --executor js-agent needs --pack-store-endpoint"
    );
    assert!(!out.contains("the switch is in place"), "{out}");
    assert_eq!(host.runs.len(), 1);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    cutover_edit::require_no_backup(&std::fs::canonicalize(&config).unwrap()).unwrap();
}

/// Behaviour: each failure after the switch is in place names its step and prints the
/// back-out: a restart kbf-host-install refuses, a frontend that never exits by the drain
/// deadline, one that is never ready, a canary that exits 1.
#[tokio::test]
async fn a_failure_after_the_switch_prints_the_back_out() {
    let server = service().await;
    let cases: Vec<(&str, Setup, String)> =
        vec![
        (
            "restart",
            Box::new(|_, h| {
                h.answer = Box::new(|cmd| {
                    Ok(if cmd.program.as_str() == "/usr/bin/ssh" {
                        reply(2, "{\"result\":\"refused\",\"reason\":\"busy\"}\n", "")
                    } else {
                        reply(0, "", "")
                    })
                });
            }),
            "the restart through kbf-host-install: exit 2: refused: busy".to_owned(),
        ),
        (
            "drain",
            Box::new(|a, h| {
                a.restart.drain_deadline_secs = Some(10);
                h.statuses.insert("/healthz".to_owned(), VecDeque::from([Ok(200)]));
            }),
            "the frontend has not exited by the drain deadline; it is left draining, and \
             nothing was restarted"
                .to_owned(),
        ),
        (
            "ready",
            Box::new(|_, h| {
                h.statuses.insert("/readyz".to_owned(), VecDeque::from([Ok(503)]));
            }),
            "http://127.0.0.1:8982/readyz did not answer 200 in 4 asks: not the frontend is ready"
                .to_owned(),
        ),
        (
            "canary",
            Box::new(|_, _| {}),
            format!("the canary job {CANARY} ended failed on host host-a"),
        ),
    ];
    for (name, set, says) in cases {
        let dir = scratch(&format!("apply-after-{name}"));
        let config = write(dir.path(), "kbf.toml", &ready_config());
        let mut a = args(dir.path(), &config, &server.origin(), true);
        let mut host = Script::healthy();
        host.on_pause = Box::new(finishes(&server, 1));
        set(&mut a, &mut host);
        let (done, out) = applied(&a, &mut host).await;
        assert_eq!(done.unwrap_err().to_string(), says, "{name}");
        assert!(
            out.contains("but a step after it failed\nback-out: kbf-deploy job-service back-out"),
            "{name}: {out}"
        );
        let file = std::fs::canonicalize(&config).unwrap();
        assert!(cutover_edit::read_backup(&file).is_ok(), "{name}");
    }
}

/// Behaviour: apply refuses (exit 2, nothing run or written) a file that is not ready, one
/// that already runs the job service, one with a backup there, and bad restart flags.
#[tokio::test]
async fn apply_refuses_before_any_step() {
    let dir = scratch("apply-refused");
    let server = service().await;
    let ready = ready_config();
    let cases: Vec<(String, RestartSetup, &str)> = vec![
        (
            "[executor]\n".to_owned(),
            Box::new(|_| {}),
            "lacks executor-frontend-id",
        ),
        (
            ready.replace("executor = \"kbf-daemon\"", "executor = \"js-agent\""),
            Box::new(|_| {}),
            "already runs executor = \"js-agent\"",
        ),
        (
            ready.clone(),
            Box::new(|r| r.instance = "Bad".to_owned()),
            "--instance: ",
        ),
        (
            ready.clone(),
            Box::new(|r| r.instance = "a".repeat(33)),
            "--instance: ",
        ),
        (
            ready.clone(),
            Box::new(|r| r.ssh_user = "9x".to_owned()),
            "--ssh-user: ",
        ),
        (
            ready.clone(),
            Box::new(|r| r.ssh_user = "u".repeat(33)),
            "--ssh-user: ",
        ),
        (
            ready.clone(),
            Box::new(|r| r.ssh_host = "-oProxyCommand=x".to_owned()),
            "--ssh-host: ",
        ),
        (
            ready.clone(),
            Box::new(|r| r.ssh_host = "a b".to_owned()),
            "--ssh-host: ",
        ),
        (
            ready.clone(),
            Box::new(|r| r.ssh_host = String::new()),
            "--ssh-host: ",
        ),
        (
            ready.clone(),
            Box::new(|r| r.api = "192.0.2.1:8982".parse().unwrap()),
            "--api: ",
        ),
        ("[executor\n".to_owned(), Box::new(|_| {}), "is not TOML"),
    ];
    for (i, (text, set, says)) in cases.into_iter().enumerate() {
        let sub = dir.path().join(i.to_string());
        std::fs::create_dir(&sub).unwrap();
        let config = write(&sub, "kbf.toml", &text);
        let mut a = args(&sub, &config, &server.origin(), true);
        set(&mut a.restart);
        let mut host = Script::healthy();
        let (done, out) = applied(&a, &mut host).await;
        let e = done.unwrap_err();
        assert_eq!(e.code(), 2, "{says}: {e}");
        assert!(e.to_string().contains(says), "{says}: {e}");
        assert_eq!(out, "", "{says}");
        assert!(host.runs.is_empty());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    }
    let config = write(dir.path(), "kbf.toml", &ready);
    write(dir.path(), "kbf.toml.pre-job-service", "earlier\n");
    let a = args(dir.path(), &config, &server.origin(), false);
    let e = applied(&a, &mut Script::healthy()).await.0.unwrap_err();
    assert!(e.to_string().contains("is there already"), "{e}");
    assert!(server.calls().is_empty());
}

/// `back-out`'s flags for `config`.
fn back_args(config: &Path, apply: bool) -> BackOutArgs {
    BackOutArgs {
        config: config.to_path_buf(),
        restart: restart(),
        apply,
    }
}

async fn backed_out(a: &BackOutArgs, host: &mut dyn Host) -> (Result<(), CutoverError>, String) {
    let mut out = Vec::new();
    let done = back_out(a, host, &mut out).await;
    (done, String::from_utf8(out).unwrap())
}

/// Behaviour: a back-out dry run prints the flags it returns to and its steps and changes
/// nothing; with `--apply` it checks the backup, puts it back byte for byte, and restarts
/// the same way; with no backup it is refused.
#[tokio::test]
async fn a_back_out_restores_the_original_and_restarts() {
    let dir = scratch("back-out");
    let text = ready_config();
    let config = write(
        dir.path(),
        "kbf.toml",
        "[executor]\nexecutor = \"js-agent\"\n",
    );
    write(dir.path(), "kbf.toml.pre-job-service", &text);
    let mut host = Script::healthy();
    let (done, out) = backed_out(&back_args(&config, false), &mut host).await;
    done.unwrap();
    let file = std::fs::canonicalize(&config).unwrap();
    assert!(
        out.starts_with(&format!(
            "kbf-deploy job-service back-out (dry run)\nconfig: {f}.pre-job-service back over {f}\n\
         \x20 executor now: js-agent, then: kbf-daemon\nthe flags it returns to:\n\
         \x20 --executor=kbf-daemon\n",
            f = file.display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "DRY-RUN: /usr/bin/kbf --mode=frontend --config {}.pre-job-service --check\n",
            file.display()
        )),
        "{out}"
    );
    assert!(
        out.ends_with("dry run: nothing changed. Re-run with --apply.\n"),
        "{out}"
    );
    assert!(host.runs.is_empty());
    let (done, out) = backed_out(&back_args(&config, true), &mut host).await;
    done.unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    assert!(!dir.path().join("kbf.toml.pre-job-service").exists());
    assert_eq!(host.runs.len(), 3);
    assert_eq!(host.runs[2].args, ssh_args());
    assert!(
        out.ends_with("done: the frontend runs executor = \"kbf-daemon\" again\n"),
        "{out}"
    );
    let e = backed_out(&back_args(&config, true), &mut host)
        .await
        .0
        .unwrap_err();
    assert!(
        e.to_string().contains("there is no switch to back out"),
        "{e}"
    );
    let e = backed_out(
        &BackOutArgs {
            restart: Restart {
                instance: String::new(),
                ..restart()
            },
            ..back_args(&config, false)
        },
        &mut host,
    )
    .await
    .0
    .unwrap_err();
    assert!(e.to_string().starts_with("--instance: "), "{e}");
}

/// Behaviour: a tool that cannot be started, a tool that fails with words on stdout only, one
/// killed by a signal, and a restart answer that is not kbf-host-install's are each said.
#[tokio::test]
async fn tools_that_fail_are_named() {
    let dir = scratch("tools");
    let config = write(
        dir.path(),
        "kbf.toml",
        "[executor]\nexecutor = \"js-agent\"\n",
    );
    write(dir.path(), "kbf.toml.pre-job-service", &ready_config());
    let cases: Vec<(Answer<'static>, &str)> = vec![
        (
            Box::new(|_| Err(io::ErrorKind::NotFound.into())),
            "the config check: cannot run /usr/bin/kbf: ",
        ),
        (
            Box::new(|_| Ok(reply(1, "on stdout\n", ""))),
            "the config check: exit 1: on stdout",
        ),
        (
            Box::new(|_| {
                use std::os::unix::process::ExitStatusExt as _;
                Ok(Output {
                    status: std::process::ExitStatus::from_raw(9),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }),
            "the config check: exit a signal: ",
        ),
        (
            Box::new(|cmd| {
                Ok(if cmd.program.as_str() == "/usr/bin/ssh" {
                    reply(255, "", "")
                } else {
                    reply(0, "", "")
                })
            }),
            "the restart through kbf-host-install: exit 255: no answer of kbf-host-install",
        ),
        (
            Box::new(|cmd| {
                Ok(if cmd.program.as_str() == "/usr/bin/ssh" {
                    reply(3, "{\"result\":\"failed\"}", "")
                } else {
                    reply(0, "", "")
                })
            }),
            "the restart through kbf-host-install: exit 3: failed",
        ),
        (
            Box::new(|cmd| {
                use std::os::unix::process::ExitStatusExt as _;
                if cmd.program.as_str() == "/usr/bin/ssh" {
                    Ok(Output {
                        status: std::process::ExitStatus::from_raw(9),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    })
                } else {
                    Ok(reply(0, "", ""))
                }
            }),
            "the restart through kbf-host-install: exit a signal: ",
        ),
        (
            Box::new(|cmd| {
                if cmd.program.as_str() == "/usr/bin/ssh" {
                    Err(io::ErrorKind::NotFound.into())
                } else {
                    Ok(reply(0, "", ""))
                }
            }),
            "the restart: cannot run /usr/bin/ssh: ",
        ),
    ];
    for (answer, says) in cases {
        let backup = dir.path().join("kbf.toml.pre-job-service");
        if !backup.exists() {
            std::fs::copy(&config, &backup).unwrap();
        }
        let mut host = Script::healthy();
        host.answer = answer;
        let e = backed_out(&back_args(&config, true), &mut host)
            .await
            .0
            .unwrap_err();
        assert!(e.to_string().starts_with(says), "{says}: {e}");
    }
}

/// Behaviour: the wait for the exit takes only a refused connection: another failure of the
/// ask (a timeout) is a frontend still there.
#[test]
fn only_a_refused_connection_is_an_exit() {
    let mut host = Script::healthy().answers(
        "/healthz",
        vec![Err(io::ErrorKind::TimedOut.into()), Ok(200), refused()],
    );
    let mut c = Carry {
        host: &mut host,
        poll_secs: 1,
        staged: None,
    };
    let url = api_url("127.0.0.1:1".parse().unwrap(), "/healthz").unwrap();
    assert!(c.wait(&url, None, exited));
    assert_eq!(host.pauses, [1, 1]);
}

/// Behaviour: a step that has nothing staged to put in place, and a canary with no client,
/// fail rather than pass.
#[tokio::test]
async fn steps_out_of_order_fail() {
    let mut host = Script::healthy();
    let mut c = Carry {
        host: &mut host,
        poll_secs: 1,
        staged: None,
    };
    let mut out = Vec::new();
    let e = c.step(&Step::PutInPlace, &mut out).await.unwrap_err();
    assert_eq!(e.to_string(), "nothing is staged");
    let canary = Canary {
        id: "c".into(),
        image: "i".into(),
        arch: "x86_64".into(),
        frontend: "f".into(),
    };
    let e = carry(
        &[Step::Canary { canary, asks: 1 }],
        true,
        &mut host,
        1,
        None,
        &mut out,
    )
    .await
    .unwrap_err();
    assert_eq!(e.to_string(), "the canary needs the job service's client");
}

/// Behaviour: a runtime that could not be built fails the verb.
#[test]
fn no_runtime_fails_the_verb() {
    let dir = scratch("no-runtime");
    let config = write(dir.path(), "kbf.toml", "");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_on(
        Err(io::Error::other("no threads")),
        Verb::BackOut(&back_args(&config, false)),
        &mut Script::healthy(),
        &mut out,
        &mut err,
    );
    assert_eq!(code, ExitCode::from(1));
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "FAIL: no runtime: no threads\n"
    );
}

/// Behaviour: the verb's exit code is 0 for a back-out dry run on its own runtime.
#[test]
fn a_dry_run_on_the_verbs_runtime_exits_0() {
    let dir = scratch("verb-runtime");
    let config = write(
        dir.path(),
        "kbf.toml",
        "[executor]\nexecutor = \"js-agent\"\n",
    );
    write(dir.path(), "kbf.toml.pre-job-service", "");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let runtime = tokio::runtime::Builder::new_current_thread().build();
    let code = run_on(
        runtime,
        Verb::BackOut(&back_args(&config, false)),
        &mut Script::healthy(),
        &mut out,
        &mut err,
    );
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(err.is_empty());
}

/// Behaviour: a back-out whose restart fails keeps the backup and says so, and run again it
/// finishes; a frontend that is not running (its operator API refuses the connection) is not
/// drained, and the restart starts it.
#[tokio::test]
async fn a_stopped_back_out_can_run_again_and_a_down_frontend_is_not_drained() {
    let dir = scratch("back-out-again");
    let text = ready_config();
    let config = write(
        dir.path(),
        "kbf.toml",
        "[executor]\nexecutor = \"js-agent\"\n",
    );
    let backup = write(dir.path(), "kbf.toml.pre-job-service", &text);
    let mut host = Script::healthy();
    host.answer = Box::new(|cmd| {
        Ok(if cmd.program.as_str() == "/usr/bin/ssh" {
            reply(2, "{\"result\":\"refused\",\"reason\":\"busy\"}", "")
        } else {
            reply(0, "", "")
        })
    });
    let (done, out) = backed_out(&back_args(&config, true), &mut host).await;
    assert_eq!(
        done.unwrap_err().to_string(),
        "the restart through kbf-host-install: exit 2: refused: busy"
    );
    let kept = std::fs::canonicalize(&backup).unwrap();
    assert!(
        out.ends_with(&format!(
            "the back-out stopped; {} is kept: run back-out again once the cause is fixed\n",
            kept.display()
        )),
        "{out}"
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    assert!(backup.exists());

    let mut host = Script::healthy().answers("/healthz", vec![refused()]);
    let (done, out) = backed_out(&back_args(&config, true), &mut host).await;
    done.unwrap();
    assert!(
        out.contains("\nthe frontend is not running: nothing to drain\n"),
        "{out}"
    );
    let programs: Vec<String> = host.runs.iter().map(|c| c.program.to_string()).collect();
    assert_eq!(programs, ["/usr/bin/kbf", "/usr/bin/ssh"]);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    assert!(!backup.exists());
}
