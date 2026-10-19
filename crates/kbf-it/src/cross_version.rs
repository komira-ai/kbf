//! `kbf-cell cross-version`: a frontend from one set of binaries and a daemon from
//! another, over the real worker protocol on loopback.
//!
//! `--server-dir` holds the frontend's build and `--daemon-dir` the daemon's. Each holds
//! `kbf` (its `--version --json` states the protocol range of that build; `kbf-server`
//! and `kbf-daemon` print no such JSON) and, for the server, `kbf-server`; for the
//! daemon, `kbf-daemon`. The two may be one directory: that is the self-test, a build
//! against itself, which runs wherever a tip build exists. A directory of an older
//! release in either place is the cross-version test; nothing here knows which it got.
//!
//! What is checked, in order; the first failure ends the run with exit status 1, after
//! printing the last lines of the server's and the daemon's logs:
//! 1. Both `kbf --version --json` answer within `--wait` with a `commit`, a
//!    `min_protocol` and a `protocol` (positive whole numbers), and the daemon's
//!    `protocol` lies in the server's `min_protocol..=protocol`: the range the server
//!    accepts (a release that raised `min_protocol` past a shipped daemon fails here,
//!    before anything starts);
//! 2. `kbf-server` (a cold store and a metadata log in the cell's directory, every
//!    listener on 127.0.0.1 with a port the system picks) prints its ` reapi=` start
//!    line, which names the worker listener and the operator API; it must not exit
//!    before;
//! 3. a probe that sends the first message a daemon sends, a `Hello`, over the same
//!    mutual TLS as a daemon: at the daemon's protocol it is welcomed *as that
//!    protocol* (a shipped daemon ends a session whose `Welcome` names another version);
//!    at one below the server's `min_protocol` and one above its `protocol` it is
//!    refused, FAILED_PRECONDITION naming the version as not accepted;
//! 4. `kbf-daemon --driver fake` is started with the cell's certificates and logs
//!    `welcomed`, before it exits, within `--wait`;
//! 5. `GET /v1/nodes` lists the daemon's node connected and, when the entry carries a
//!    `protocol`, that is the daemon's. A server that lists none (one that predates the
//!    field) is noted, not failed.
//!
//! Every wait of a step is bounded by `--wait`, so a process that hangs fails the run
//! with the step's name instead of the job's timeout. Every log is under
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
        Ok(Self {
            commit: commit.to_owned(),
            min_protocol: number("min_protocol")?,
            protocol: number("protocol")?,
        })
    }
}

/// Why a daemon with `daemon`'s protocol cannot join a server of `server`'s build, or
/// `None` when the server accepts it.
#[must_use]
pub fn mismatch(server: &Facts, daemon: &Facts) -> Option<String> {
    let accepted = server.min_protocol..=server.protocol;
    (!accepted.contains(&daemon.protocol)).then(|| {
        format!(
            "the daemon (build {}) speaks protocol {}, but the server (build {}) accepts {} to {}",
            daemon.commit, daemon.protocol, server.commit, server.min_protocol, server.protocol
        )
    })
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

/// Why `answer` is not a welcome at `version`, or `None` when it is one: a `Hello` at a
/// protocol the server accepts is answered by a `Welcome` that names that protocol, since
/// a shipped daemon ends a session whose `Welcome` names another.
fn welcome_problem(answer: &Answer, version: u32) -> Option<String> {
    match answer {
        Ok(named) if *named == version => None,
        Ok(named) => Some(format!(
            "a Hello at protocol {version} was welcomed as protocol {named}"
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
/// `protocol`, when the entry has the key, must be the daemon's; an entry without the
/// key is a server that predates it.
fn node_report(view: &Value, daemon: &Facts) -> Result<String, String> {
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
             field), so the daemon's protocol {} is not checked there",
            daemon.protocol
        )),
        Some(named) if named.as_u64() == Some(u64::from(daemon.protocol)) => {
            Ok(format!("OK: {listed}, at protocol {}", daemon.protocol))
        }
        Some(named) => Err(format!(
            "GET /v1/nodes lists {NODE_ID} at protocol {named}, but its daemon speaks {}",
            daemon.protocol
        )),
    }
}

/// What one run works with. Paths are absolute.
struct Cell {
    server_dir: PathBuf,
    daemon_dir: PathBuf,
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
            work,
            logs,
            wait,
            http,
            runtime,
        })
    }

    /// Everything after setup. The processes it starts are stopped as it returns.
    fn steps(&self) -> Result<(), CellError> {
        let server = self.facts("server", &self.server_dir)?;
        let daemon = self.facts("daemon", &self.daemon_dir)?;
        let why = mismatch(&server, &daemon);
        ensure(why.is_none(), why.unwrap_or_default())?;
        println!(
            "OK: the daemon's protocol {} is in the server's {} to {}",
            daemon.protocol, server.min_protocol, server.protocol
        );
        let pki = crate::pki::write(&self.work.join("pki"), NODE_ID);
        let pki = pki.or_broken(&"the certificates")?;
        let (_server, addresses) = self.start_server(&pki)?;
        self.probe(&server, &daemon, &pki, &addresses.worker)?;
        let _daemon = self.start_daemon(&pki, &addresses.worker)?;
        self.node_listed(&addresses.api, &daemon)
    }

    /// The facts of the build in `dir`, from its `kbf --version --json`.
    fn facts(&self, role: &str, dir: &Path) -> Result<Facts, CellError> {
        let log = self.logs.join(format!("{role}-version.json"));
        let mut command = Command::new(dir.join("kbf"));
        command.args(["--version", "--json"]);
        to_log(&mut command, &log)?;
        let mut asked = Process::spawn(command, "kbf --version --json")?;
        let ended = asked.wait_within(self.wait)?;
        let text = read(&log)?;
        let what = format!("{} --version --json", dir.join("kbf").display());
        let why = format!("{what} failed, or ran past {:?}: {text}", self.wait);
        ensure(ended.is_some_and(|s| s.success()), why)?;
        let facts =
            Facts::parse(&text).map_err(|why| CellError::Failed(format!("{what}: {why}")))?;
        println!(
            "{role} {}: commit {}, protocol {} to {}",
            dir.display(),
            facts.commit,
            facts.min_protocol,
            facts.protocol
        );
        Ok(facts)
    }

    /// Starts `kbf-server` and waits for its start line.
    fn start_server(&self, pki: &PkiFiles) -> Result<(Process, Addresses), CellError> {
        let mut server = Command::new(self.server_dir.join("kbf-server"));
        server
            .args(["--listen", LOOPBACK])
            .args(["--worker-listen", LOOPBACK])
            .args(["--api-listen", LOOPBACK])
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
        let why = welcome_problem(&welcomed, daemon.protocol);
        ensure(why.is_none(), why.unwrap_or_default())?;
        println!(
            "OK: a Hello at protocol {} (the daemon's) is welcomed as that protocol",
            daemon.protocol
        );
        // One below the range (0, the wire's "not named", when the server accepts from
        // 1, which it refuses all the same) and one above it.
        let outside = [
            server.min_protocol.saturating_sub(1),
            server.protocol.saturating_add(1),
        ];
        for version in outside {
            let refused = ask(version);
            let why = refusal_problem(&refused, version);
            ensure(why.is_none(), why.unwrap_or_default())?;
            println!(
                "OK: a Hello at protocol {version} is refused: FailedPrecondition, \
                 protocol version {version} is not accepted"
            );
        }
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
        self.await_line(&mut process, &err, "welcomed", "kbf-daemon")?;
        println!("OK: kbf-daemon was welcomed by kbf-server");
        Ok(process)
    }

    /// Step 5: the operator API at `api` lists the daemon's node.
    fn node_listed(&self, api: &str, daemon: &Facts) -> Result<(), CellError> {
        let url = format!("http://{api}/v1/nodes");
        let answer = self.runtime.block_on(async {
            let sent = self.http.get(url).send().await?;
            sent.error_for_status()?.bytes().await
        });
        let bytes = answer.or_broken(&"GET /v1/nodes")?;
        let view = serde_json::from_slice::<Value>(&bytes);
        let view = view.or_broken(&"the answer of GET /v1/nodes")?;
        let report = node_report(&view, daemon).map_err(CellError::Failed)?;
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

    fn facts(commit: &str, min_protocol: u32, protocol: u32) -> Facts {
        Facts {
            commit: commit.to_owned(),
            min_protocol,
            protocol,
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
            Facts::parse(r#"{"commit":"a","min_protocol":1,"protocol":4294967295}"#),
            Ok(facts("a", 1, u32::MAX))
        );
    }

    /// Catches a range that is exclusive at either end, a comparison against the
    /// daemon's `min_protocol` or the server's range read backwards, and an empty server
    /// range (min above protocol) that admits anything.
    #[test]
    fn a_daemon_joins_a_server_that_accepts_its_protocol() {
        let server = facts("s", 2, 4);
        for daemon in [2, 3, 4] {
            assert_eq!(mismatch(&server, &facts("d", 1, daemon)), None, "{daemon}");
        }
        for daemon in [1, 5] {
            let why = mismatch(&server, &facts("d", 1, daemon)).expect("refused");
            let speaks = format!("speaks protocol {daemon}");
            holds(&why, &[&speaks, "accepts 2 to 4", "build d", "build s"]);
        }
        assert!(mismatch(&facts("s", 5, 3), &facts("d", 1, 4)).is_some());
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

    /// Catches a welcome taken for any answer at all: it must name the version sent (a
    /// `Welcome` naming the server's newest is what a shipped daemon ends the session
    /// over), and a refusal is not a welcome.
    #[test]
    fn a_welcome_names_the_version_the_hello_sent() {
        assert_eq!(welcome_problem(&Ok(2), 2), None);
        let why = welcome_problem(&Ok(3), 2).expect("another version");
        holds(&why, &["protocol 2", "as protocol 3"]);
        let refused = Err(Status::failed_precondition(
            "protocol version 2 is not accepted",
        ));
        let why = welcome_problem(&refused, 2).expect("refused");
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
        let daemon = facts("d", 1, 2);
        let node = |fields: &str| -> Value {
            let text = format!(
                r#"{{"server":{{}},"nodes":[{{"node_id":"other","connected":true}},{{"node_id":"{NODE_ID}",{fields}}}]}}"#
            );
            serde_json::from_str(&text).expect("json")
        };
        let ok = node(r#""connected":true,"protocol":2"#);
        let report = node_report(&ok, &daemon).expect("listed at its protocol");
        assert!(report.starts_with("OK:"), "{report}");
        holds(&report, &["at protocol 2"]);
        let report = node_report(&node(r#""connected":true"#), &daemon).expect("no protocol key");
        assert!(report.starts_with("NOTE:"), "{report}");
        holds(&report, &["predates"]);
        for (fields, needle) in [
            (r#""connected":true,"protocol":3"#, "at protocol 3"),
            (r#""connected":true,"protocol":"2""#, "at protocol \"2\""),
            (r#""connected":true,"protocol":null"#, "at protocol null"),
            (r#""connected":false,"protocol":2"#, "but not connected"),
            (r#""protocol":2"#, "but not connected"),
        ] {
            let e = node_report(&node(fields), &daemon).expect_err(fields);
            assert!(e.contains(needle), "{fields}: {e}");
        }
        for view in [
            r#"{"nodes":[{"node_id":"other","connected":true}]}"#,
            r#"{"nodes":[]}"#,
            r#"{"nodes":{"node_id":"cell-node-1"}}"#,
            r#"{}"#,
        ] {
            let view: Value = serde_json::from_str(view).expect("json");
            let e = node_report(&view, &daemon).expect_err("not listed");
            assert!(e.contains("does not list"), "{e}");
        }
    }
}
