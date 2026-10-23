//! `kbf-cell cross-version`: a frontend from one set of binaries and a daemon from
//! another, over the real worker protocol on loopback.
//!
//! `--server-dir` holds the frontend's build and `--daemon-dir` the daemon's. Each holds
//! `kbf` (its `--version --json` states the protocol range of that build; `kbf-server`
//! and `kbf-daemon` print no such JSON) and, for the server, `kbf-server`; for the
//! daemon, `kbf-daemon`. The two may be one directory: that is the self-test, a build
//! against itself, which runs wherever a tip build exists. A directory of an older
//! release in either place is the cross-version test; nothing here knows which it got.
//! `--installer-dir` (optional) names the release whose installer is on the hosts when
//! those two kbf are staged: the upgrade-order rule below.
//!
//! What is checked, in order; the first failure ends the run with exit status 1, after
//! printing the last lines of the server's and the daemon's logs:
//! 1. Both `kbf --version --json` answer within `--wait` with a `commit`, a
//!    `min_protocol`, a `protocol` (positive whole numbers) and a `state_schema`;
//!    `kbf-server --version` and `kbf-daemon --version` name that same commit (a directory
//!    that mixes two builds would otherwise state one build's protocols and run
//!    another's); and the two builds are compatible, which is what the server's
//!    `negotiate` and the daemon's `welcome::acceptable` make of it: the server refuses a
//!    `Hello` below its `min_protocol` and answers any other with the lesser of the
//!    `Hello`'s version and its own `protocol`, and a daemon takes a `Welcome` from the
//!    oldest version it speaks (its build's `min_protocol`) up to the version it sent. So
//!    the daemon's `protocol` must be at least the server's `min_protocol` (a release that
//!    raised `min_protocol` past a shipped daemon fails here, before anything starts) and
//!    the server's `protocol` at least the daemon's `min_protocol`. A daemon newer than
//!    its server is compatible, and is welcomed at the server's newest. When both builds
//!    are one commit the run says it is a self-test, not a cross-version run;
//!
//!    With `--installer-dir`, the installer goes first: the installer of that release
//!    must be able to stage the `kbf-server` and the `kbf-daemon` under test. An
//!    installer refuses a release that declares a `protocol`, `min_protocol` or
//!    `state_schema` above what it understands (`installer_too_old`), and an installer is
//!    put on a host by hand, never updated by a release, so a kbf that raises one of the
//!    three can only follow its installer. This harness makes that comparison itself, on
//!    the numbers the `kbf` of each build states ([`installer_problem`]): the release is
//!    refused when any of the three is greater than the installer's, and the refusal names
//!    the first such number in the order `protocol`, `min_protocol`, `state_schema`. What an
//!    installer understands is what the `kbf` of its release declares (the tip pins that
//!    its installer understands at least what its kbf declares). The reverse order, a kbf
//!    staged by the installer of an older release, fails when the newer declares more.
//!    When the three numbers are equal in every build the order cannot fail and the run
//!    says so;
//! 2. `kbf-server` (a cold store and a metadata log in the cell's directory, every
//!    listener on 127.0.0.1 with a port the system picks) prints its ` reapi=` start
//!    line, which names the worker listener and the operator API; it must not exit
//!    before;
//! 3. a probe that sends the first message a daemon sends, a `Hello`, over the same
//!    mutual TLS as a daemon: at the daemon's protocol it is welcomed as the lesser of
//!    that protocol and the server's `protocol` (the version the session will speak);
//!    at one above the server's `protocol` it is welcomed *as the server's `protocol`*,
//!    not refused (a daemon newer than its server is the rolling upgrade's other half);
//!    at one below the server's `min_protocol` it is refused, FAILED_PRECONDITION naming
//!    the version as not accepted. While the server's `min_protocol` is 1 the probe below
//!    is protocol 0, which the wire reads as "not named" and any server refuses: that
//!    edge is then not exercised, and the run says so;
//! 4. `kbf-daemon --driver fake` is started with the cell's certificates and logs
//!    `welcomed`, before it exits, within `--wait`. If it does not, the failure carries the
//!    `reason=` of the daemon's own log (the numbers in the facts are the server's);
//!    then the session is held for eight heartbeat intervals (`--heartbeat-interval-ms
//!    250` on the server) with the daemon running, and neither log may say a heartbeat
//!    went unacknowledged, the session ended, or a message did not parse. That is the
//!    worker protocol past `Welcome` as far as the heartbeat and its acknowledgement; no
//!    work is offered, so `Start` and `Result` are not exercised;
//! 5. `GET /v1/nodes` lists the daemon's node connected and, when the entry carries a
//!    `protocol`, that is the daemon's. A server that lists none (one that predates the
//!    field) is noted, not failed.
//!
//! Every wait of a step is bounded by `--wait`, so a process that hangs fails the run
//! with the step's name instead of the job's timeout. The stop at the end is bounded
//! too, though not by `--wait`: the server and the daemon get SIGINT and, if still
//! running 30 s later, are killed (a run that passed stays passed). Every log is under
//! `<work>/logs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use kbf_daemon::TlsFiles;
use kbf_proto::worker::worker_client::WorkerClient;
use kbf_proto::worker::{Capability, DaemonMessage, Hello, daemon_message, server_message};
use serde_json::Value;
use tonic::transport::Endpoint;
use tonic::{Code, Status};

use crate::cell::{
    CellError, OrBroken as _, POLL, Process, absolute, ensure, plain, print_tails, read, to_log,
};
use crate::pki::{PkiFiles, SERVER_NAME};

/// The node id the daemon's certificate names, and the probe's `Hello`.
const NODE_ID: &str = "cell-node-1";
/// Where every listener binds: loopback, a port the system picks.
const LOOPBACK: &str = "127.0.0.1:0";
/// How many lines of each log a failed run prints.
const TAIL: usize = 60;
/// The heartbeat interval, in milliseconds, the server asks daemons for (`--heartbeat-interval-ms`;
/// the default is 5 s).
const HEARTBEAT_MS: u64 = 250;
/// How many heartbeat intervals the session is held for after the daemon is welcomed.
const BEATS_HELD: u32 = 8;
/// What the daemon or the server logs when the session is not carrying the heartbeats: the daemon
/// when none is acknowledged for two intervals, or when its session ends; either side when a
/// message of the other's does not parse as one it knows (a shape change without a version bump).
const BROKEN_SESSION: [&str; 4] = [
    "heartbeat gap",
    "session ended",
    "empty daemon message",
    "empty server message",
];

/// `kbf-cell cross-version` flags.
#[derive(Clone, Debug, clap::Args)]
pub struct CrossVersionArgs {
    /// The directory holding the frontend's `kbf-server` and `kbf`.
    #[arg(long)]
    pub server_dir: PathBuf,
    /// The directory holding the daemon's `kbf-daemon` and `kbf`. May be the same
    /// directory as `--server-dir`.
    #[arg(long)]
    pub daemon_dir: PathBuf,
    /// The directory of the release whose installer stages both kbf under test (its `kbf`
    /// states what that installer understands). Without it the order of installer and
    /// kbf is not checked, and the run says so.
    #[arg(long)]
    pub installer_dir: Option<PathBuf>,
    /// The cell's directory: certificates, the server's stores, the daemon's data
    /// directory, and `logs/`.
    #[arg(long)]
    pub work: PathBuf,
    /// The longest any one step may take, in seconds: a `kbf --version --json`, the
    /// server's start line, a probe session, the daemon's `welcomed`, the operator
    /// API's answer.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    pub wait: u64,
}

/// Runs the cross-version test (the module docs say what it checks). Prints what it
/// does on stdout.
///
/// # Errors
/// [`CellError::Failed`] when a check fails, [`CellError::Broken`] when the cell cannot
/// be set up or driven.
pub fn run(args: &CrossVersionArgs) -> Result<(), CellError> {
    let cell = Cell::new(args)?;
    // The server and the daemon are stopped when `steps` returns, before the tails print.
    let result = cell.steps();
    if result.is_err() {
        print_tails(&cell.logs, TAIL);
    }
    result
}

/// What `kbf --version --json` says of a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// The first 12 hex digits of the commit built, or `unknown`.
    pub commit: String,
    /// The oldest `kbf.worker.v1` protocol the build's server accepts.
    pub min_protocol: u32,
    /// The newest it speaks, and the one its daemon speaks.
    pub protocol: u32,
    /// The state schema the build embeds.
    pub state_schema: u64,
}

impl Facts {
    /// Reads the line `kbf --version --json` prints.
    ///
    /// # Errors
    /// Why `text` is not that: not JSON, no `commit` string, or a protocol that is not a
    /// positive whole number (the wire's 0 means "not named").
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| format!("not one JSON object ({e}): {text:?}"))?;
        let commit = value.get("commit").and_then(Value::as_str);
        let commit = commit.ok_or_else(|| format!("no \"commit\" string in {text:?}"))?;
        let number = |key: &str| {
            let found = value.get(key).and_then(Value::as_u64);
            let found = found.and_then(|n| u32::try_from(n).ok());
            found
                .filter(|&n| n > 0)
                .ok_or_else(|| format!("no positive whole number {key:?} in {text:?}"))
        };
        let min_protocol = number("min_protocol")?;
        let protocol = number("protocol")?;
        let state_schema = value.get("state_schema").and_then(Value::as_u64);
        let state_schema =
            state_schema.ok_or_else(|| format!("no whole number \"state_schema\" in {text:?}"))?;
        Ok(Self {
            commit: commit.to_owned(),
            min_protocol,
            protocol,
            state_schema,
        })
    }

    /// The numbers an installer checks a release by, named, in the order it checks them.
    fn numbers(&self) -> [(&'static str, u64); 3] {
        [
            ("protocol", u64::from(self.protocol)),
            ("min_protocol", u64::from(self.min_protocol)),
            ("state_schema", self.state_schema),
        ]
    }
}

/// Why the installer of `installer`'s release cannot stage the `component` of `release`,
/// or `None` when it can: the release declares a number (`protocol`, `min_protocol` or
/// `state_schema`) greater than the installer's. The first such number, in that order, is
/// the one named.
#[must_use]
pub fn installer_problem(installer: &Facts, component: &str, release: &Facts) -> Option<String> {
    let mut pairs = release.numbers().into_iter().zip(installer.numbers());
    let too_new = pairs.find(|&((_, declared), (_, most))| declared > most);
    too_new.map(|((field, declared), (_, most))| {
        format!(
            "the installer of build {} cannot stage the {component} of build {}: the release \
             needs a newer installer: {field} {declared}; the installer understands up to \
             {most} (update the installer before the kbf that raises it)",
            installer.commit, release.commit
        )
    })
}

/// How far the numbers of `a` are from those of `b`: the sum of the three differences,
/// which is 0 only when every number of both builds is the same (a sum of differences, not
/// a comparison of tuples).
fn apart(a: &Facts, b: &Facts) -> u64 {
    let pairs = a.numbers().into_iter().zip(b.numbers());
    pairs.map(|((_, x), (_, y))| x.abs_diff(y)).sum()
}

/// Why a daemon of `daemon`'s build cannot join a server of `server`'s build, or `None`
/// when it can (the module docs say what that means).
#[must_use]
pub fn mismatch(server: &Facts, daemon: &Facts) -> Option<String> {
    if daemon.protocol < server.min_protocol {
        return Some(format!(
            "the daemon (build {}) speaks protocol {}, but the server (build {}) accepts \
             protocol {} and newer",
            daemon.commit, daemon.protocol, server.commit, server.min_protocol
        ));
    }
    (server.protocol < daemon.min_protocol).then(|| {
        format!(
            "the daemon (build {}) speaks protocol {} and newer, but the server (build {}) \
             speaks up to protocol {}: its Welcome would be below the daemon's oldest",
            daemon.commit, daemon.min_protocol, server.commit, server.protocol
        )
    })
}

/// The commit a build says it was made from in the line it prints: `+<commit>` at the
/// end of the version, the second word (`kbf-server 0.1.0+0123456789ab` is what
/// `--version` prints, and the start line begins the same way).
fn built_from(printed: &str) -> Option<&str> {
    let version = printed.split_whitespace().nth(1)?;
    version.split_once('+').map(|(_, commit)| commit)
}

/// Why `program`, which printed `printed` for `--version`, is not a build of the commit
/// `facts` (the `kbf` of its directory) states, or `None` when it is.
fn build_problem(program: &Path, printed: &str, facts: &Facts) -> Option<String> {
    let dir = program.parent().unwrap_or(program);
    match built_from(printed) {
        Some(commit) if commit == facts.commit => None,
        Some(commit) => Some(format!(
            "{} is built from commit {commit}, but {}/kbf states commit {}: the directory \
             mixes two builds",
            program.display(),
            dir.display(),
            facts.commit
        )),
        None => Some(format!(
            "{} --version prints no <version>+<commit> as its second word: {printed:?}",
            program.display()
        )),
    }
}

/// The worker and operator API addresses in the server's start line.
#[derive(Debug, PartialEq, Eq)]
struct Addresses {
    worker: String,
    api: String,
}

impl Addresses {
    /// Reads ` worker=<addr>` and ` api=<addr>` out of `line`.
    fn parse(line: &str) -> Result<Self, String> {
        let find = |key: &str| {
            let found = line.split_whitespace().find_map(|w| w.strip_prefix(key));
            let missing = || format!("the start line names no {key}<address>: {line}");
            found.map(str::to_owned).ok_or_else(missing)
        };
        Ok(Self {
            worker: find("worker=")?,
            api: find("api=")?,
        })
    }
}

/// What a `Hello` was answered with: the protocol the `Welcome` named, or the status
/// that ended the session (a refusal, or the probe's own: no answer in time, no
/// connection).
type Answer = Result<u32, Status>;

/// Why `answer` is not the welcome a `Hello` at `version` is owed by a server whose newest
/// protocol is `newest`, or `None` when it is: a `Welcome` that names the lesser of the two
/// (a daemon takes a `Welcome` no newer than the version it sent, and the server speaks
/// nothing newer than `newest`).
fn welcome_problem(answer: &Answer, version: u32, newest: u32) -> Option<String> {
    let owed = version.min(newest);
    match answer {
        Ok(named) if *named == owed => None,
        Ok(named) => Some(format!(
            "a Hello at protocol {version} was welcomed as protocol {named}, not {owed} (the \
             lesser of the Hello's and the server's newest, {newest})"
        )),
        Err(status) => Some(format!(
            "a Hello at protocol {version} was not welcomed: {:?} {:?}",
            status.code(),
            status.message()
        )),
    }
}

/// Why `answer` is not the refusal of a `Hello` at `version` that the server owes it, or
/// `None` when it is one: FAILED_PRECONDITION saying that protocol version is not
/// accepted.
fn refusal_problem(answer: &Answer, version: u32) -> Option<String> {
    let owed = format!("protocol version {version} is not accepted");
    match answer {
        Ok(named) => Some(format!(
            "a Hello at protocol {version} was welcomed (as protocol {named}); it is outside \
             the range the server accepts and should have been refused"
        )),
        Err(s) if s.code() == Code::FailedPrecondition && s.message().contains(&owed) => None,
        Err(s) => Some(format!(
            "a Hello at protocol {version} was refused with {:?} {:?}, not FailedPrecondition \
             saying {owed:?}",
            s.code(),
            s.message()
        )),
    }
}

/// The reason a daemon's log gives for ending a session: the text from the last
/// `reason=` on, with what it says after (`protocol: server speaks ...`).
fn refusal(log: &str) -> Option<String> {
    let line = plain(log);
    let said = line.lines().rev().find_map(|l| l.split_once("reason="));
    said.map(|(_, reason)| reason.trim().to_owned())
}

/// `error` and its sources, `: `-joined (tonic's transport errors say only "transport
/// error" by themselves).
fn causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        text = format!("{text}: {next}");
        source = next.source();
    }
    text
}

/// Opens a session, sends a `Hello` at `protocol` as a daemon of this cell would, and
/// reads the first answer, all within `wait`.
async fn say_hello(endpoint: &Endpoint, protocol: u32, wait: Duration) -> Answer {
    let platform = [
        ("arch", "x86_64"),
        ("cpus", "1"),
        ("drivers", "fake"),
        ("mem_gib", "1"),
        ("os", "linux"),
    ];
    let hello = Hello {
        protocol_version: protocol,
        node_id: NODE_ID.to_owned(),
        daemon_version: "kbf-cell cross-version".to_owned(),
        capabilities: platform
            .map(|(key, value)| Capability {
                key: key.to_owned(),
                value: value.to_owned(),
            })
            .to_vec(),
        instance_id: format!("{NODE_ID}-probe"),
        ..Hello::default()
    };
    let first = DaemonMessage {
        message: Some(daemon_message::Message::Hello(hello)),
    };
    // The request stream stays open after the Hello, as a daemon's does.
    let requests =
        futures::stream::once(std::future::ready(first)).chain(futures::stream::pending());
    let asked = async {
        let connect = endpoint.connect().await;
        let channel =
            connect.map_err(|e| Status::unavailable(format!("connect: {}", causes(&e))))?;
        let response = WorkerClient::new(channel).session(requests).await?;
        match response
            .into_inner()
            .message()
            .await?
            .and_then(|m| m.message)
        {
            Some(server_message::Message::Welcome(welcome)) => Ok(welcome.protocol_version),
            other => Err(Status::unknown(format!(
                "the first answer was not a Welcome: {other:?}"
            ))),
        }
    };
    tokio::time::timeout(wait, asked).await.unwrap_or_else(|_| {
        Err(Status::deadline_exceeded(format!(
            "no answer within {wait:?}"
        )))
    })
}

/// Why `view`, the answer of `GET /v1/nodes`, does not show the daemon's node as it
/// should; else the line to print. The node must be listed and connected. Its
/// `protocol`, when the entry has the key, must be `session`, the version the daemon's
/// session speaks (the `Welcome`'s: the lesser of the daemon's protocol and the server's
/// newest); an entry without the key is a server that predates it.
fn node_report(view: &Value, session: u32) -> Result<String, String> {
    let nodes = view.get("nodes").and_then(Value::as_array);
    let node = nodes
        .into_iter()
        .flatten()
        .find(|n| n.get("node_id").and_then(Value::as_str) == Some(NODE_ID));
    let node = node.ok_or_else(|| format!("GET /v1/nodes does not list {NODE_ID}: {view}"))?;
    let connected = node.get("connected").and_then(Value::as_bool);
    let listed = format!("GET /v1/nodes lists {NODE_ID}, connected");
    if connected != Some(true) {
        return Err(format!(
            "GET /v1/nodes lists {NODE_ID}, but not connected: {node}"
        ));
    }
    match node.get("protocol") {
        None => Ok(format!(
            "NOTE: {listed}; its entry carries no `protocol` (this server predates the \
             field), so the session's protocol {session} is not checked there"
        )),
        Some(named) if named.as_u64() == Some(u64::from(session)) => {
            Ok(format!("OK: {listed}, at protocol {session}"))
        }
        Some(named) => Err(format!(
            "GET /v1/nodes lists {NODE_ID} at protocol {named}, but its session speaks {session}"
        )),
    }
}

/// What one run works with. Paths are absolute.
struct Cell {
    server_dir: PathBuf,
    daemon_dir: PathBuf,
    installer_dir: Option<PathBuf>,
    work: PathBuf,
    logs: PathBuf,
    wait: Duration,
    http: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl Cell {
    fn new(args: &CrossVersionArgs) -> Result<Self, CellError> {
        let wait = Duration::from_secs(args.wait);
        let http = reqwest::Client::builder().timeout(wait).build();
        let http = http.or_broken(&"the HTTP client")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let runtime = runtime.or_broken(&"the async runtime")?;
        let work = absolute(&args.work)?;
        let logs = work.join("logs");
        std::fs::create_dir_all(&logs).or_broken(&logs.display())?;
        Ok(Self {
            server_dir: absolute(&args.server_dir)?,
            daemon_dir: absolute(&args.daemon_dir)?,
            installer_dir: args.installer_dir.as_deref().map(absolute).transpose()?,
            work,
            logs,
            wait,
            http,
            runtime,
        })
    }

    /// Everything after setup. The processes it starts are stopped as it returns.
    fn steps(&self) -> Result<(), CellError> {
        let server = self.facts("server", &self.server_dir, "kbf-server")?;
        let daemon = self.facts("daemon", &self.daemon_dir, "kbf-daemon")?;
        let why = mismatch(&server, &daemon);
        ensure(why.is_none(), why.unwrap_or_default())?;
        println!(
            "OK: the builds are compatible: the daemon speaks protocol {} to {}, the server {} to {}",
            daemon.min_protocol, daemon.protocol, server.min_protocol, server.protocol
        );
        if server.commit == daemon.commit {
            println!(
                "NOTE: the server and the daemon are one build (commit {}): this run is a \
                 self-test, not a cross-version run",
                server.commit
            );
        }
        self.installer_first(&server, &daemon)?;
        let pki = crate::pki::write(&self.work.join("pki"), NODE_ID);
        let pki = pki.or_broken(&"the certificates")?;
        let (_server, addresses) = self.start_server(&pki)?;
        self.probe(&server, &daemon, &pki, &addresses.worker)?;
        let mut daemon_process = self.start_daemon(&pki, &addresses.worker)?;
        self.hold_session(&mut daemon_process)?;
        self.node_listed(&addresses.api, daemon.protocol.min(server.protocol))
    }

    /// The facts of the build in `dir`, from its `kbf --version --json`, once its
    /// `binary` (`kbf-server` or `kbf-daemon`) has said it is that same build.
    fn facts(&self, role: &str, dir: &Path, binary: &str) -> Result<Facts, CellError> {
        let facts = self.kbf_facts(role, dir)?;
        let program = dir.join(binary);
        let printed = self.printed(&program, &["--version"], &format!("{binary}-version.txt"))?;
        let why = build_problem(&program, &printed, &facts);
        ensure(why.is_none(), why.unwrap_or_default())?;
        Ok(facts)
    }

    /// The facts the `kbf` of `dir` states, and a line saying so.
    fn kbf_facts(&self, role: &str, dir: &Path) -> Result<Facts, CellError> {
        let kbf = dir.join("kbf");
        let text = self.printed(
            &kbf,
            &["--version", "--json"],
            &format!("{role}-version.json"),
        )?;
        let facts = Facts::parse(&text).map_err(|why| {
            CellError::Failed(format!("{} --version --json: {why}", kbf.display()))
        })?;
        println!(
            "{role} {}: commit {}, protocol {} to {}, state schema {}",
            dir.display(),
            facts.commit,
            facts.min_protocol,
            facts.protocol,
            facts.state_schema
        );
        Ok(facts)
    }

    /// The installer goes first: the installer of `--installer-dir`'s release must stage
    /// both builds under test (the module docs say why that is the order).
    fn installer_first(&self, server: &Facts, daemon: &Facts) -> Result<(), CellError> {
        let Some(dir) = &self.installer_dir else {
            println!("NOTE: no --installer-dir: the order of installer and kbf is not checked");
            return Ok(());
        };
        let installer = self.kbf_facts("installer", dir)?;
        for (component, release) in [("kbf-server", server), ("kbf-daemon", daemon)] {
            let why = installer_problem(&installer, component, release);
            ensure(why.is_none(), why.unwrap_or_default())?;
        }
        println!(
            "OK: the installer of build {} can stage both builds",
            installer.commit
        );
        if apart(&installer, server) + apart(&installer, daemon) == 0 {
            println!(
                "NOTE: the installer and both builds declare the same protocols and state \
                 schema, so no order of their upgrade could fail: the order is not exercised"
            );
        }
        Ok(())
    }

    /// What `program args` prints (stdout and stderr, kept in `<logs>/<log>`). It must
    /// end with status 0 within `--wait`.
    fn printed(&self, program: &Path, args: &[&str], log: &str) -> Result<String, CellError> {
        let log = self.logs.join(log);
        let mut command = Command::new(program);
        command.args(args);
        to_log(&mut command, &log)?;
        let what = format!("{} {}", program.display(), args.join(" "));
        let mut asked = Process::spawn(command, &what)?;
        let ended = asked.wait_within(self.wait)?;
        let text = read(&log)?;
        let why = format!("{what} failed, or ran past {:?}: {text}", self.wait);
        ensure(ended.is_some_and(|s| s.success()), why)?;
        Ok(text)
    }

    /// Starts `kbf-server` and waits for its start line.
    fn start_server(&self, pki: &PkiFiles) -> Result<(Process, Addresses), CellError> {
        let mut server = Command::new(self.server_dir.join("kbf-server"));
        server
            .args(["--listen", LOOPBACK])
            .args(["--worker-listen", LOOPBACK])
            .args(["--api-listen", LOOPBACK])
            .args(["--heartbeat-interval-ms", &HEARTBEAT_MS.to_string()])
            .arg("--cold-store")
            .arg(format!(
                "file://{}",
                self.work.join("server-blobs").display()
            ))
            .arg("--meta-dir")
            .arg(self.work.join("server-meta"))
            .arg("--worker-tls-cert")
            .arg(&pki.server_cert)
            .arg("--worker-tls-key")
            .arg(&pki.server_key)
            .arg("--worker-client-ca")
            .arg(&pki.ca_cert)
            .stdin(Stdio::null())
            .stdout(self.log_file("server.out")?)
            .stderr(self.log_file("server.err")?);
        let mut process = Process::spawn(server, "kbf-server")?;
        let out = self.logs.join("server.out");
        let line = self.await_line(&mut process, &out, " reapi=", "kbf-server")?;
        println!("{line}");
        let addresses = Addresses::parse(&line).map_err(CellError::Failed)?;
        Ok((process, addresses))
    }

    /// The probe `Hello`s of step 3, over the mutual TLS a daemon uses.
    fn probe(
        &self,
        server: &Facts,
        daemon: &Facts,
        pki: &PkiFiles,
        worker: &str,
    ) -> Result<(), CellError> {
        let tls = TlsFiles {
            ca_cert: pki.ca_cert.clone(),
            cert: pki.client_cert.clone(),
            key: pki.client_key.clone(),
            server_name: Some(SERVER_NAME.to_owned()),
        };
        let tls = tls.load().or_broken(&"the certificates")?;
        let endpoint = Endpoint::from_shared(format!("https://{worker}"));
        let endpoint = endpoint.or_broken(&"the worker listener's URL")?;
        let endpoint = endpoint
            .tls_config(tls)
            .or_broken(&"the worker TLS settings")?;
        let ask = |version| {
            self.runtime
                .block_on(say_hello(&endpoint, version, self.wait))
        };

        let welcomed = ask(daemon.protocol);
        let why = welcome_problem(&welcomed, daemon.protocol, server.protocol);
        ensure(why.is_none(), why.unwrap_or_default())?;
        println!(
            "OK: a Hello at protocol {} (the daemon's) is welcomed as protocol {}",
            daemon.protocol,
            daemon.protocol.min(server.protocol)
        );
        // One above the server's newest is a daemon newer than the server: welcomed at
        // the server's newest.
        let newer = server.protocol.saturating_add(1);
        let why = welcome_problem(&ask(newer), newer, server.protocol);
        ensure(why.is_none(), why.unwrap_or_default())?;
        println!(
            "OK: a Hello at protocol {newer} (newer than the server) is welcomed as the server's \
             newest, protocol {}",
            server.protocol
        );
        // One below the oldest the server accepts (0, the wire's "not named", when it
        // accepts from 1, which it refuses all the same).
        if server.min_protocol == 1 {
            println!(
                "NOTE: the server's min_protocol is 1, so the probe below its range is \
                 protocol 0, which the wire reads as \"not named\" and any server refuses: \
                 the lower edge of the range is not exercised"
            );
        }
        let older = server.min_protocol.saturating_sub(1);
        let why = refusal_problem(&ask(older), older);
        ensure(why.is_none(), why.unwrap_or_default())?;
        println!(
            "OK: a Hello at protocol {older} is refused: FailedPrecondition, \
             protocol version {older} is not accepted"
        );
        Ok(())
    }

    /// Starts `kbf-daemon --driver fake` against the worker listener at `worker`, and
    /// waits for it to log `welcomed`.
    fn start_daemon(&self, pki: &PkiFiles, worker: &str) -> Result<Process, CellError> {
        let data = self.work.join("daemon-data");
        std::fs::create_dir_all(&data).or_broken(&data.display())?;
        let mut daemon = Command::new(self.daemon_dir.join("kbf-daemon"));
        daemon
            .args(["--driver", "fake", "--server", &format!("https://{worker}")])
            .args(["--tls-server-name", SERVER_NAME, "--node-id", NODE_ID])
            .arg("--ca-cert")
            .arg(&pki.ca_cert)
            .arg("--cert")
            .arg(&pki.client_cert)
            .arg("--key")
            .arg(&pki.client_key)
            .arg("--data-dir")
            .arg(&data)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(self.log_file("daemon.err")?);
        let mut process = Process::spawn(daemon, "kbf-daemon")?;
        let err = self.logs.join("daemon.err");
        if let Err(error) = self.await_line(&mut process, &err, "welcomed", "kbf-daemon") {
            // The daemon says why it refused a Welcome (`reason=` of its log), which the
            // facts, being the server's constants, cannot.
            let reason = refusal(&read(&err).unwrap_or_default());
            return Err(match (error, reason) {
                (CellError::Failed(what), Some(reason)) => {
                    CellError::Failed(format!("{what}; the daemon's log: {reason}"))
                }
                (error, _) => error,
            });
        }
        println!("OK: kbf-daemon was welcomed by kbf-server");
        Ok(process)
    }

    /// Step 4b: the welcomed session carries heartbeats. The server asks for one every
    /// `HEARTBEAT_MS`; the session is held for `BEATS_HELD` of them, and the daemon must
    /// stay up and neither log a gap in the acknowledgements nor that it ended the
    /// session; neither log may say a message of the other did not parse. This is the
    /// worker protocol past `Hello` and `Welcome` to its second kind of message and its
    /// acknowledgement; no work is offered, so `Start`, `Result` and the rest are not.
    fn hold_session(&self, daemon: &mut Process) -> Result<(), CellError> {
        let held = Duration::from_millis(HEARTBEAT_MS) * BEATS_HELD;
        let until = Instant::now() + held;
        while Instant::now() < until {
            let exited = daemon.wait_within(Duration::ZERO)?;
            let status = exited.map_or_else(String::new, |s| s.to_string());
            let why = format!("kbf-daemon exited ({status}) while its session was held");
            ensure(exited.is_none(), why)?;
            std::thread::sleep(POLL);
        }
        for log in ["daemon.err", "server.err"] {
            let text = plain(&read(&self.logs.join(log))?);
            let broken = text
                .lines()
                .find(|l| BROKEN_SESSION.iter().any(|b| l.contains(b)));
            let why = format!(
                "the session did not carry {BEATS_HELD} heartbeats of {HEARTBEAT_MS} ms: {log} says {:?}",
                broken.unwrap_or_default()
            );
            ensure(broken.is_none(), why)?;
        }
        println!(
            "OK: the session carried {BEATS_HELD} heartbeats of {HEARTBEAT_MS} ms, all acknowledged"
        );
        Ok(())
    }

    /// Step 5: the operator API at `api` lists the daemon's node.
    fn node_listed(&self, api: &str, session: u32) -> Result<(), CellError> {
        let url = format!("http://{api}/v1/nodes");
        let answer = self.runtime.block_on(async {
            let sent = self.http.get(url).send().await?;
            sent.error_for_status()?.bytes().await
        });
        let bytes = answer.or_broken(&"GET /v1/nodes")?;
        let view = serde_json::from_slice::<Value>(&bytes);
        let view = view.or_broken(&"the answer of GET /v1/nodes")?;
        let report = node_report(&view, session).map_err(CellError::Failed)?;
        println!("{report}");
        Ok(())
    }

    /// The first line of the file at `log` (the plain text of it) that holds `needle`,
    /// waited for while `process` runs and for at most `--wait`.
    fn await_line(
        &self,
        process: &mut Process,
        log: &Path,
        needle: &str,
        what: &str,
    ) -> Result<String, CellError> {
        let deadline = Instant::now() + self.wait;
        loop {
            if let Some(line) = plain(&read(log)?).lines().find(|l| l.contains(needle)) {
                return Ok(line.to_owned());
            }
            let exited = process.wait_within(Duration::ZERO)?;
            let status = exited.map_or_else(String::new, |s| s.to_string());
            let why = format!("{what} exited ({status}) without logging {needle:?}");
            ensure(exited.is_none(), why)?;
            let why = format!("{what} did not log {needle:?} within {:?}", self.wait);
            ensure(Instant::now() < deadline, why)?;
            std::thread::sleep(POLL);
        }
    }

    fn log_file(&self, name: &str) -> Result<std::fs::File, CellError> {
        let path = self.logs.join(name);
        std::fs::File::create(&path).or_broken(&path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `--version --json` line of a build that speaks protocols 1 to 2.
    const OLD: &str = r#"{"commit":"old","min_protocol":1,"protocol":2,"state_schema":3}"#;
    /// That of a build that speaks 2 to 3, which overlaps `OLD`'s range at 2.
    const NEW: &str = r#"{"commit":"new","min_protocol":2,"protocol":3,"state_schema":3}"#;
    /// That of a build that speaks 4 to 5, with no protocol in common with `OLD`.
    const FAR: &str = r#"{"commit":"far","min_protocol":4,"protocol":5,"state_schema":3}"#;

    fn facts(commit: &str, min_protocol: u32, protocol: u32) -> Facts {
        Facts {
            commit: commit.to_owned(),
            min_protocol,
            protocol,
            state_schema: 3,
        }
    }

    fn with_schema(facts: Facts, state_schema: u64) -> Facts {
        Facts {
            state_schema,
            ..facts
        }
    }

    /// Asserts that `text` holds every needle (one assertion each: a `&&` of the
    /// conditions would be a branch the passing test never takes the other way).
    #[track_caller]
    fn holds(text: &str, needles: &[&str]) {
        for needle in needles {
            assert!(text.contains(needle), "{needle:?} not in {text:?}");
        }
    }

    /// Catches facts read from the wrong key or type (the protocol taken for the
    /// minimum, a string number accepted, a negative or fractional one, 0 -- the wire's
    /// "not named" -- or a number past `u32` wrapped), a missing key passed with a
    /// default, and text that is not an object.
    #[test]
    fn facts_are_read_from_the_identity_line() {
        let line = r#"{"commit":"0123456789ab","min_protocol":2,"profile":"debug","protocol":5,"state_schema":3,"version":"0.1.0"}"#;
        assert_eq!(
            Facts::parse(&format!("{line}\n")),
            Ok(facts("0123456789ab", 2, 5))
        );
        let refused = |text: &str, needle: &str| {
            let e = Facts::parse(text).expect_err(text);
            assert!(e.contains(needle), "{text}: {e}");
        };
        refused("", "not one JSON object");
        refused("kbf 0.1.0+0123456789ab", "not one JSON object");
        refused(r#"{"min_protocol":1,"protocol":1}"#, "\"commit\"");
        refused(
            r#"{"commit":7,"min_protocol":1,"protocol":1}"#,
            "\"commit\"",
        );
        refused(r#"{"commit":"a","protocol":1}"#, "\"min_protocol\"");
        refused(r#"{"commit":"a","min_protocol":1}"#, "\"protocol\"");
        refused(
            r#"{"commit":"a","min_protocol":"1","protocol":1}"#,
            "\"min_protocol\"",
        );
        refused(
            r#"{"commit":"a","min_protocol":-1,"protocol":1}"#,
            "\"min_protocol\"",
        );
        refused(
            r#"{"commit":"a","min_protocol":1.5,"protocol":1}"#,
            "\"min_protocol\"",
        );
        refused(
            r#"{"commit":"a","min_protocol":0,"protocol":1}"#,
            "\"min_protocol\"",
        );
        refused(
            r#"{"commit":"a","min_protocol":1,"protocol":0}"#,
            "\"protocol\"",
        );
        refused(
            r#"{"commit":"a","min_protocol":1,"protocol":4294967296}"#,
            "\"protocol\"",
        );
        assert_eq!(
            Facts::parse(
                r#"{"commit":"a","min_protocol":1,"protocol":4294967295,"state_schema":3}"#
            ),
            Ok(facts("a", 1, u32::MAX))
        );
        // The state schema is read from its own key, as a whole number.
        for text in [
            r#"{"commit":"a","min_protocol":1,"protocol":1}"#,
            r#"{"commit":"a","min_protocol":1,"protocol":1,"state_schema":"3"}"#,
            r#"{"commit":"a","min_protocol":1,"protocol":1,"state_schema":-3}"#,
            r#"{"commit":"a","min_protocol":1,"protocol":1,"state_schema":null}"#,
        ] {
            refused(text, "\"state_schema\"");
        }
    }

    /// Asserts that an installer may stage a release that declares no more than it
    /// understands (equal, or less: an installer ahead of the kbf it stages is the order
    /// the rule asks for), and is refused a release that declares more in any one of the
    /// three numbers (each tried alone, the others equal), with a refusal that names both
    /// builds, the component and the number.
    #[test]
    fn an_installer_stages_a_release_that_declares_no_more_than_it_understands() {
        let installer = facts("inst", 2, 4);
        assert_eq!(
            installer_problem(&installer, "kbf-daemon", &installer),
            None
        );
        assert_eq!(
            installer_problem(&installer, "kbf-daemon", &facts("old", 1, 3)),
            None
        );
        let schema = |n| Facts {
            state_schema: n,
            ..facts("new", 2, 4)
        };
        assert_eq!(
            installer_problem(&installer, "kbf-daemon", &schema(3)),
            None
        );
        for (release, number) in [
            (facts("new", 2, 5), "protocol 5"),
            (facts("new", 3, 4), "min_protocol 3"),
            (schema(4), "state_schema 4"),
        ] {
            let why = installer_problem(&installer, "kbf-server", &release).expect("too old");
            holds(
                &why,
                &[
                    number,
                    "installer of build inst",
                    "kbf-server of build new",
                    "needs a newer installer",
                ],
            );
        }
    }

    /// Asserts that when a release raises several numbers past the installer's, the refusal
    /// names the first of `protocol`, `min_protocol`, `state_schema`, with the release's
    /// number and the installer's.
    #[test]
    fn the_first_number_a_release_raises_is_the_one_the_refusal_names() {
        let installer = facts("inst", 2, 4);
        let all_three = with_schema(facts("new", 3, 5), 9);
        let why = installer_problem(&installer, "kbf-server", &all_three).expect("too old");
        let named = "protocol 5; the installer understands up to 4";
        holds(&why, &[named]);
        let two = with_schema(facts("new", 3, 4), 9);
        let why = installer_problem(&installer, "kbf-server", &two).expect("too old");
        let named = "min_protocol 3; the installer understands up to 2";
        holds(&why, &[named]);
        let one = with_schema(facts("new", 2, 4), 9);
        let why = installer_problem(&installer, "kbf-server", &one).expect("too old");
        let named = "state_schema 9; the installer understands up to 3";
        holds(&why, &[named]);
    }

    /// Asserts that two builds are 0 apart only when all three numbers are the same, and
    /// that a difference in any one of them, in either direction, adds to the distance.
    #[test]
    fn builds_are_apart_by_the_sum_of_the_differences_of_their_numbers() {
        let base = facts("a", 2, 4);
        assert_eq!(apart(&base, &facts("b", 2, 4)), 0);
        assert_eq!(apart(&base, &facts("b", 2, 6)), 2);
        assert_eq!(apart(&facts("b", 2, 6), &base), 2);
        assert_eq!(apart(&base, &facts("b", 1, 4)), 1);
        assert_eq!(apart(&base, &with_schema(facts("b", 2, 4), 7)), 4);
        assert_eq!(apart(&base, &facts("b", 5, 8)), 7);
    }

    /// Asserts that two builds whose `--version --json` lines are read as they are printed
    /// are told apart by their numbers alone: builds whose protocol ranges overlap join
    /// (the daemon to the server, either way round) and an installer that understands the
    /// numbers of the release stages it; builds whose ranges do not overlap are refused,
    /// whichever of the two is the newer, and an installer is refused a release that
    /// declares a newer protocol.
    #[test]
    fn two_reported_builds_that_do_not_understand_each_other_are_refused() {
        let report = |text| Facts::parse(text).expect("a report");
        let (old, new, far) = (report(OLD), report(NEW), report(FAR));
        assert_eq!(mismatch(&old, &new), None);
        assert_eq!(mismatch(&new, &old), None);
        let why = mismatch(&old, &far).expect("a daemon that needs a newer server");
        holds(&why, &["speaks protocol 4 and newer", "up to protocol 2"]);
        let why = mismatch(&far, &old).expect("a daemon older than the server accepts");
        holds(&why, &["speaks protocol 2", "accepts protocol 4"]);
        assert_eq!(installer_problem(&new, "kbf-daemon", &old), None);
        let why = installer_problem(&old, "kbf-daemon", &new).expect("a newer release");
        holds(&why, &["protocol 3; the installer understands up to 2"]);
    }

    /// Catches the server's `min_protocol` read as exclusive or as the daemon's, a daemon
    /// newer than its server refused (the server welcomes it at its own newest; the
    /// rolling upgrade has daemons ahead of their frontend), a daemon older than the
    /// server's `min_protocol` passed, and the daemon's own oldest ignored: a server newer
    /// than the daemon's oldest is what lets the Welcome (the server's newest at most) be
    /// one the daemon takes.
    #[test]
    fn a_daemon_joins_a_server_that_welcomes_it_at_a_version_it_takes() {
        let server = facts("s", 2, 4);
        // At the server's oldest, inside, at its newest, and past it.
        for daemon in [2, 3, 4, 5, 9] {
            assert_eq!(mismatch(&server, &facts("d", 1, daemon)), None, "{daemon}");
        }
        let why = mismatch(&server, &facts("d", 1, 1)).expect("below the server's oldest");
        holds(
            &why,
            &[
                "speaks protocol 1",
                "accepts protocol 2 and newer",
                "build d",
                "build s",
            ],
        );
        // A daemon that speaks only from 5 and a server that speaks to 4: no common
        // version, and at the edge (a daemon from 4) there is one.
        let why = mismatch(&server, &facts("d", 5, 6)).expect("no common version");
        holds(
            &why,
            &[
                "speaks protocol 5 and newer",
                "up to protocol 4",
                "below the daemon's oldest",
            ],
        );
        assert_eq!(mismatch(&server, &facts("d", 4, 6)), None);
        // The server's own `protocol` below its `min_protocol` (an empty range) admits no one.
        assert!(mismatch(&facts("s", 5, 3), &facts("d", 1, 4)).is_some());
    }

    /// Catches a binary of another build than its directory's `kbf` passed (the commit
    /// compared as a prefix or not at all), a version with no `+<commit>` taken for a
    /// match, the commit read from the wrong word, and a failure that names neither the
    /// binary nor both commits.
    #[test]
    fn a_binary_must_be_the_build_its_directorys_kbf_states() {
        let facts = facts("0123456789ab", 1, 1);
        let program = Path::new("/builds/n/kbf-daemon");
        let problem = |printed: &str| build_problem(program, printed, &facts);
        assert_eq!(problem("kbf-daemon 0.1.0+0123456789ab\n"), None);
        assert_eq!(
            problem("kbf-server 0.1.0+0123456789ab reapi=x worker=y"),
            None
        );
        let why = problem("kbf-daemon 0.1.0+ffffffffffff\n").expect("another build");
        holds(
            &why,
            &[
                "/builds/n/kbf-daemon is built from commit ffffffffffff",
                "/builds/n/kbf states commit 0123456789ab",
                "mixes two builds",
            ],
        );
        for printed in [
            "kbf-daemon 0.1.0+0123456789a\n",
            "kbf-daemon 0.1.0+0123456789abc\n",
            "kbf-daemon 0.1.0\n",
            "kbf-daemon\n",
            "",
            "0.1.0+0123456789ab\n",
        ] {
            assert!(problem(printed).is_some(), "{printed:?} passed");
        }
        let why = problem("kbf-daemon 0.1.0\n").expect("no commit");
        holds(&why, &["prints no <version>+<commit>", "kbf-daemon 0.1.0"]);
    }

    /// Catches the worker and API addresses read from each other's words, a prefix match
    /// inside another word (`xapi=`), and a start line without one of them passed.
    #[test]
    fn the_start_line_names_the_worker_listener_and_the_api() {
        let line = "kbf-server 0.1.0+0123456789ab reapi=127.0.0.1:41000 worker=127.0.0.1:41001 \
                    invocations=off blobs=off api=127.0.0.1:41002 log=00000000deadbeef";
        assert_eq!(
            Addresses::parse(line),
            Ok(Addresses {
                worker: "127.0.0.1:41001".to_owned(),
                api: "127.0.0.1:41002".to_owned(),
            })
        );
        let e = Addresses::parse("kbf-server reapi=127.0.0.1:1 worker=127.0.0.1:2")
            .expect_err("no api");
        assert!(e.contains("api="), "{e}");
        let e =
            Addresses::parse("kbf-server reapi=127.0.0.1:1 xapi=127.0.0.1:3 worker=127.0.0.1:2")
                .expect_err("a word that only ends in api=");
        assert!(e.contains("api="), "{e}");
        let e = Addresses::parse("kbf-server reapi=127.0.0.1:1 api=127.0.0.1:3")
            .expect_err("no worker");
        assert!(e.contains("worker="), "{e}");
    }

    /// Catches a welcome taken for any answer at all: it must name the lesser of the
    /// version sent and the server's newest (a `Welcome` above what the daemon sent is a
    /// version it was never offered, one below what the server can speak is a needless
    /// downgrade), and a refusal is not a welcome.
    #[test]
    fn a_welcome_names_the_lesser_of_the_hello_and_the_servers_newest() {
        assert_eq!(welcome_problem(&Ok(2), 2, 4), None);
        assert_eq!(welcome_problem(&Ok(4), 4, 4), None);
        // A Hello newer than the server: welcomed at the server's newest.
        assert_eq!(welcome_problem(&Ok(4), 9, 4), None);
        let why = welcome_problem(&Ok(3), 2, 4).expect("another version");
        holds(&why, &["protocol 2", "as protocol 3", "not 2", "newest, 4"]);
        let why = welcome_problem(&Ok(9), 9, 4).expect("the Hello's own, above the server");
        holds(&why, &["protocol 9", "as protocol 9", "not 4"]);
        let why = welcome_problem(&Ok(3), 9, 4).expect("below the server's newest");
        holds(&why, &["as protocol 3", "not 4"]);
        let refused = Err(Status::failed_precondition(
            "protocol version 2 is not accepted",
        ));
        let why = welcome_problem(&refused, 2, 4).expect("refused");
        holds(&why, &["not welcomed", "FailedPrecondition"]);
    }

    /// Catches a refusal accepted from any status (the wrong code, or the right code for
    /// another reason, such as a `Hello` with no node id), a welcome taken for a refusal,
    /// and the refusal of another version taken for this one's.
    #[test]
    fn a_refusal_is_failed_precondition_naming_the_version() {
        let right = |v: u32| {
            Status::failed_precondition(format!(
                "protocol version {v} is not accepted; this server speaks 1 to 1"
            ))
        };
        assert_eq!(refusal_problem(&Err(right(4)), 4), None);
        let why = refusal_problem(&Ok(4), 4).expect("welcomed");
        holds(&why, &["welcomed", "should have been refused"]);
        for wrong in [
            Status::invalid_argument("protocol version 4 is not accepted"),
            Status::failed_precondition("Hello has no node_id"),
            right(5),
            Status::deadline_exceeded("no answer within 3s"),
        ] {
            let why = refusal_problem(&Err(wrong.clone()), 4).expect("a wrong refusal");
            assert!(
                why.contains("not FailedPrecondition saying"),
                "{wrong:?}: {why}"
            );
        }
    }

    /// Catches a node found by another id or not at all passed, a node that is not
    /// connected (or says nothing of it) passed, a protocol compared as text or against
    /// the wrong number, a `null` protocol (the server named none) taken for an absent
    /// key, and the absent key failed (a server that predates the field is noted).
    #[test]
    fn the_nodes_view_lists_the_daemon_connected_at_its_protocol() {
        let daemon = 2;
        let node = |fields: &str| -> Value {
            let text = format!(
                r#"{{"server":{{}},"nodes":[{{"node_id":"other","connected":true}},{{"node_id":"{NODE_ID}",{fields}}}]}}"#
            );
            serde_json::from_str(&text).expect("json")
        };
        let ok = node(r#""connected":true,"protocol":2"#);
        let report = node_report(&ok, daemon).expect("listed at its protocol");
        assert!(report.starts_with("OK:"), "{report}");
        holds(&report, &["at protocol 2"]);
        let report = node_report(&node(r#""connected":true"#), daemon).expect("no protocol key");
        assert!(report.starts_with("NOTE:"), "{report}");
        holds(&report, &["predates"]);
        for (fields, needle) in [
            (r#""connected":true,"protocol":3"#, "at protocol 3"),
            (r#""connected":true,"protocol":"2""#, "at protocol \"2\""),
            (r#""connected":true,"protocol":null"#, "at protocol null"),
            (r#""connected":false,"protocol":2"#, "but not connected"),
            (r#""protocol":2"#, "but not connected"),
        ] {
            let e = node_report(&node(fields), daemon).expect_err(fields);
            assert!(e.contains(needle), "{fields}: {e}");
        }
        for view in [
            r#"{"nodes":[{"node_id":"other","connected":true}]}"#,
            r#"{"nodes":[]}"#,
            r#"{"nodes":{"node_id":"cell-node-1"}}"#,
            r#"{}"#,
        ] {
            let view: Value = serde_json::from_str(view).expect("json");
            let e = node_report(&view, daemon).expect_err("not listed");
            assert!(e.contains("does not list"), "{e}");
        }
    }
}
