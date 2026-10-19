//! `kbf-cell cross-version` against fakes: every check of the cross-version test passes
//! on a pair of builds that fits, and each one fails the run when a build breaks it.
//!
//! The fakes are this test binary, run through links named `kbf`, `kbf-server` and
//! `kbf-daemon` in a `--server-dir` and a `--daemon-dir` of the test's own: a start-up
//! hook answers as the program its argv0 names before the test harness starts. A
//! directory's `facts.json` is what its fake `kbf --version --json` prints, and the fake
//! server accepts the range that file states, as a build does. The fake server is a real
//! gRPC `Worker` over mutual TLS and a small HTTP answer for `GET /v1/nodes`; the fake
//! daemon only logs `welcomed`. Each fake appends its call to `<dir>/calls` and plants
//! the defect `KBF_XV_FAKE_DEFECT` names, if it is one of its own.
//!
//! The real builds are checked by the `ci / cross-version` job, which runs `kbf-cell
//! cross-version` on the tip's `kbf-server`, `kbf-daemon` and `kbf` as both directories.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// The fakes' start-up hook: before the harness reads its arguments, this binary
// answers as the program its argv0 names, if it names one.
const _: () = {
    extern "C" fn fake_hook() {
        fake::hook();
    }
    #[used]
    #[unsafe(link_section = ".init_array")]
    static FAKE_HOOK: extern "C" fn() = fake_hook;
};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-cell");

/// The fake programs.
mod fake {
    use std::io::Write as _;
    use std::net::SocketAddr;
    use std::path::Path;
    use std::pin::Pin;
    use std::time::Duration;

    use futures::{Stream, StreamExt as _};
    use kbf_proto::worker::worker_server::{Worker, WorkerServer};
    use kbf_proto::worker::{
        DaemonMessage, ServerMessage, Welcome, daemon_message, server_message,
    };
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tonic::transport::server::TcpIncoming;
    use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
    use tonic::{Request, Response, Status, Streaming};

    /// The directory the fakes write `calls` to.
    pub const DIR: &str = "KBF_XV_FAKE_DIR";
    /// The defect to plant.
    pub const DEFECT: &str = "KBF_XV_FAKE_DEFECT";
    /// What the fake operator API says of the node's `protocol` (JSON; unset: no key).
    pub const NODE_PROTOCOL: &str = "KBF_XV_FAKE_NODE_PROTOCOL";

    pub fn hook() {
        let mut args = std::env::args();
        let argv0 = args.next().unwrap_or_default();
        let program = Path::new(&argv0);
        let role = program.file_name().and_then(|n| n.to_str());
        let role = role.unwrap_or_default().to_owned();
        if !matches!(role.as_str(), "kbf" | "kbf-server" | "kbf-daemon") {
            return;
        }
        let dir = program.parent().expect("a directory").to_owned();
        let args: Vec<String> = args.collect();
        // This runs before the Rust runtime does, which is what sets SIGPIPE to ignored:
        // without it, tonic's vectored write to a connection the client already closed
        // (the refusal, then a client that drops the channel) ends the server silently.
        // SAFETY: `signal` with `SIG_IGN` installs no handler.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
        record(&role, &dir, &args);
        let code = match role.as_str() {
            "kbf" => kbf(&dir, &args),
            "kbf-server" => server(&dir, &args),
            _ => daemon(&args),
        };
        std::process::exit(code);
    }

    fn defect() -> String {
        std::env::var(DEFECT).unwrap_or_default()
    }

    /// Appends `<role>\t<directory name>\t<arguments>` to `calls`.
    fn record(role: &str, dir: &Path, args: &[String]) {
        let calls = std::env::var(DIR).expect("the fakes' directory");
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{calls}/calls"))
            .expect("open for append");
        writeln!(file, "{role}\t{name}\t{}", args.join(" ")).expect("append");
    }

    /// Runs for 30 s (so a lost SIGINT does not linger): a process that hangs.
    fn hang() -> i32 {
        std::thread::sleep(Duration::from_secs(30));
        0
    }

    fn kbf(dir: &Path, args: &[String]) -> i32 {
        if args != ["--version", "--json"] {
            eprintln!("the fake kbf was given {args:?}");
            return 3;
        }
        match defect().as_str() {
            "facts-hang" => return hang(),
            "facts-fail" => {
                eprintln!("kbf: broken");
                return 1;
            }
            _ => {}
        }
        print!(
            "{}",
            std::fs::read_to_string(dir.join("facts.json")).expect("facts.json")
        );
        0
    }

    /// The value after the flag `name`.
    fn flag(args: &[String], name: &str) -> Option<String> {
        let at = args.iter().position(|a| a == name)?;
        args.get(at + 1).cloned()
    }

    fn server(dir: &Path, args: &[String]) -> i32 {
        match defect().as_str() {
            "server-exits" => return 4,
            "server-silent" => return hang(),
            _ => {}
        }
        let wanted = [
            "--listen",
            "--worker-listen",
            "--api-listen",
            "--cold-store",
            "--meta-dir",
            "--worker-tls-cert",
            "--worker-tls-key",
            "--worker-client-ca",
        ];
        let given: Vec<Option<String>> = wanted.iter().map(|w| flag(args, w)).collect();
        let [
            Some(_),
            Some(worker),
            Some(api),
            Some(_),
            Some(_),
            Some(cert),
            Some(key),
            Some(ca),
        ] = given.as_slice()
        else {
            eprintln!("the fake kbf-server was given {args:?}, not all of {wanted:?}");
            return 3;
        };
        let read = |path: &String| std::fs::read_to_string(path).expect("read a TLS file");
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(read(cert), read(key)))
            .client_ca_root(Certificate::from_pem(read(ca)));
        let facts = std::fs::read_to_string(dir.join("facts.json")).expect("facts.json");
        let facts: serde_json::Value = serde_json::from_str(&facts).expect("facts are JSON");
        let number = |key: &str| u32::try_from(facts[key].as_u64().expect(key)).expect("u32");
        let worker_service = FakeWorker {
            range: (number("min_protocol"), number("protocol")),
            defect: defect(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let worker = worker.parse::<SocketAddr>().expect("worker address");
            let incoming = TcpIncoming::bind(worker).expect("bind the worker listener");
            let worker = incoming.local_addr().expect("worker address");
            let api = TcpListener::bind(api).await.expect("bind the API");
            let api_address = api.local_addr().expect("API address");
            // The start line, with the listeners the defect names wrong or leaves out.
            let (reported, api_word) = match defect().as_str() {
                "server-wrong-worker" => ("127.0.0.1:1".to_owned(), format!(" api={api_address}")),
                "server-no-api" => (worker.to_string(), String::new()),
                _ => (worker.to_string(), format!(" api={api_address}")),
            };
            println!("kbf-server 0.0.0+fake reapi=127.0.0.1:0 worker={reported}{api_word}");
            std::io::stdout().flush().expect("flush");
            tokio::spawn(answer_nodes(api, nodes_body()));
            Server::builder()
                .tls_config(tls)
                .expect("server TLS")
                .add_service(WorkerServer::new(worker_service))
                .serve_with_incoming(incoming)
                .await
                .expect("serve");
        });
        0
    }

    /// The answer of `GET /v1/nodes`: the daemon's node, as the defect says.
    fn nodes_body() -> String {
        let protocol =
            std::env::var(NODE_PROTOCOL).map_or(String::new(), |p| format!(",\"protocol\":{p}"));
        let node = |connected: bool| {
            format!("{{\"node_id\":\"cell-node-1\",\"connected\":{connected}{protocol}}}")
        };
        let nodes = match defect().as_str() {
            "api-empty" => String::new(),
            "api-disconnected" => node(false),
            _ => node(true),
        };
        format!("{{\"server\":{{}},\"nodes\":[{nodes}]}}")
    }

    /// Answers every request on `listener` with `body` as JSON.
    async fn answer_nodes(listener: TcpListener, body: String) {
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await.expect("read the request");
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).await.expect("answer");
        }
    }

    /// The `Worker` service of a server that accepts `range`.
    struct FakeWorker {
        range: (u32, u32),
        defect: String,
    }

    #[tonic::async_trait]
    impl Worker for FakeWorker {
        type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;

        async fn session(
            &self,
            request: Request<Streaming<DaemonMessage>>,
        ) -> Result<Response<Self::SessionStream>, Status> {
            let mut inbound = request.into_inner();
            let first = inbound.message().await?.and_then(|m| m.message);
            let Some(daemon_message::Message::Hello(hello)) = first else {
                return Err(Status::invalid_argument("the first message must be Hello"));
            };
            let version = hello.protocol_version;
            let (oldest, newest) = self.range;
            match self.defect.as_str() {
                "hang-hello" => std::future::pending::<()>().await,
                "silent-stream" => return Ok(Response::new(futures::stream::empty().boxed())),
                _ => {}
            }
            let inside = (oldest..=newest).contains(&version);
            if !inside && self.defect != "accepts-all" {
                return Err(match self.defect.as_str() {
                    "refuse-invalid" => Status::invalid_argument(format!(
                        "protocol version {version} is not accepted; this server speaks {oldest} to {newest}"
                    )),
                    "refuse-unnamed" => Status::failed_precondition("this server is full"),
                    _ => Status::failed_precondition(format!(
                        "protocol version {version} is not accepted; this server speaks {oldest} to {newest}"
                    )),
                });
            }
            let named = if self.defect == "welcome-newest" {
                newest
            } else {
                version
            };
            let welcome = ServerMessage {
                message: Some(server_message::Message::Welcome(Welcome {
                    protocol_version: named,
                    heartbeat_interval_ms: 1000,
                    ..Welcome::default()
                })),
            };
            let stream = futures::stream::once(std::future::ready(Ok(welcome)));
            Ok(Response::new(
                stream.chain(futures::stream::pending()).boxed(),
            ))
        }
    }

    fn daemon(args: &[String]) -> i32 {
        match defect().as_str() {
            "daemon-exits" => return 5,
            "daemon-silent" => {
                eprintln!("WARN kbf_daemon: cannot reach a server; retrying");
                return hang();
            }
            _ => {}
        }
        let server = flag(args, "--server").unwrap_or_default();
        let wanted = [
            ("--driver", "fake"),
            ("--node-id", "cell-node-1"),
            ("--tls-server-name", "localhost"),
        ];
        let flags_ok = wanted
            .iter()
            .all(|(n, v)| flag(args, n).as_deref() == Some(*v));
        let files_ok = ["--ca-cert", "--cert", "--key", "--data-dir"]
            .iter()
            .all(|n| flag(args, n).is_some());
        if !(flags_ok && files_ok && server.starts_with("https://127.0.0.1:")) {
            eprintln!("the fake kbf-daemon was given {args:?}");
            return 3;
        }
        eprintln!("\x1b[32m INFO\x1b[0m kbf_daemon: welcomed server={server}");
        hang()
    }
}

/// A fresh directory for one run of one test.
fn scratch(name: &str) -> PathBuf {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-cross-version-tests")
        .join(format!("{name}-{}-{call}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

/// The facts a build states: its `kbf --version --json` line.
fn facts(min_protocol: u32, protocol: u32) -> String {
    format!(
        "{{\"commit\":\"0123456789ab\",\"min_protocol\":{min_protocol},\"profile\":\"debug\",\
         \"protocol\":{protocol},\"state_schema\":3,\"version\":\"0.1.0\"}}\n"
    )
}

/// What one run of `kbf-cell cross-version` did.
struct Run {
    code: Option<i32>,
    text: String,
    calls: Vec<String>,
    elapsed: Duration,
}

impl Run {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// The fakes that were started, in order, as `role` or `role@directory`.
    fn started(&self) -> Vec<&str> {
        self.calls
            .iter()
            .map(|c| c.split('\t').next().unwrap_or_default())
            .collect()
    }
}

/// One fake cell: the fakes in `<dir>/server` and `<dir>/daemon` (or both in
/// `<dir>/server`), and `kbf-cell cross-version`'s work directory `<dir>/work`.
struct Cell {
    dir: PathBuf,
    server_dir: PathBuf,
    daemon_dir: PathBuf,
    defect: String,
    node_protocol: Option<String>,
    wait: u64,
}

impl Cell {
    /// A server accepting `server.0..=server.1` and a daemon speaking `daemon`.
    fn new(name: &str, server: (u32, u32), daemon: u32, defect: &str) -> Self {
        let dir = scratch(name);
        let (server_dir, daemon_dir) = (dir.join("server"), dir.join("daemon"));
        let me = std::env::current_exe().expect("this test binary");
        for (path, links, text) in [
            (
                &server_dir,
                ["kbf", "kbf-server"],
                facts(server.0, server.1),
            ),
            (&daemon_dir, ["kbf", "kbf-daemon"], facts(1, daemon)),
        ] {
            std::fs::create_dir_all(path).expect("a build directory");
            std::fs::write(path.join("facts.json"), text).expect("facts.json");
            for link in links {
                std::os::unix::fs::symlink(&me, path.join(link)).expect("link a fake");
            }
        }
        Self {
            dir,
            server_dir,
            daemon_dir,
            defect: defect.to_owned(),
            node_protocol: None,
            wait: 20,
        }
    }

    /// One build directory for both, as the self-test has it: it holds `kbf`,
    /// `kbf-server` and `kbf-daemon`, stating `min..=protocol`.
    fn one_directory(name: &str, min: u32, protocol: u32, defect: &str) -> Self {
        let mut cell = Self::new(name, (min, protocol), protocol, defect);
        let both = cell.dir.join("both");
        std::fs::create_dir_all(&both).expect("the build directory");
        std::fs::write(both.join("facts.json"), facts(min, protocol)).expect("facts.json");
        let me = std::env::current_exe().expect("this test binary");
        for link in ["kbf", "kbf-server", "kbf-daemon"] {
            std::os::unix::fs::symlink(&me, both.join(link)).expect("link a fake");
        }
        cell.server_dir = both.clone();
        cell.daemon_dir = both;
        cell
    }

    /// The fake operator API says the node speaks `json` (a JSON value).
    fn node_protocol(mut self, json: &str) -> Self {
        self.node_protocol = Some(json.to_owned());
        self
    }

    /// Every wait of the harness is `secs` seconds.
    fn wait(mut self, secs: u64) -> Self {
        self.wait = secs;
        self
    }

    /// Runs `kbf-cell cross-version`, ended by force after 45 s: a harness that does
    /// not bound a wait would otherwise hang the test.
    fn run(&self) -> Run {
        let out = self.dir.join("out.txt");
        let file = std::fs::File::create(&out).expect("the output file");
        let mut command = Command::new(BIN);
        command
            .arg("cross-version")
            .arg("--server-dir")
            .arg(&self.server_dir)
            .arg("--daemon-dir")
            .arg(&self.daemon_dir)
            .arg("--work")
            .arg(self.dir.join("work"))
            .args(["--wait", &self.wait.to_string()])
            .env(fake::DIR, &self.dir)
            .env(fake::DEFECT, &self.defect)
            .stdin(std::process::Stdio::null())
            .stdout(file.try_clone().expect("clone the output file"))
            .stderr(file);
        if let Some(json) = &self.node_protocol {
            command.env(fake::NODE_PROTOCOL, json);
        }
        let start = Instant::now();
        let mut child = command.spawn().expect("start kbf-cell");
        let code = loop {
            if let Some(status) = child.try_wait().expect("poll kbf-cell") {
                break status.code();
            }
            if start.elapsed() > Duration::from_secs(45) {
                child.kill().expect("end kbf-cell");
                child.wait().expect("reap kbf-cell");
                panic!(
                    "kbf-cell cross-version did not end within 45 s ({})",
                    self.defect
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let elapsed = start.elapsed();
        let text = std::fs::read_to_string(&out).expect("the output");
        let calls = std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default();
        let calls = calls.lines().map(str::to_owned).collect();
        Run {
            code,
            text,
            calls,
            elapsed,
        }
    }
}

/// Asserts the run failed (exit `code`) and its output holds every needle.
#[track_caller]
fn fails(run: &Run, code: i32, needles: &[&str]) {
    assert_eq!(run.code, Some(code), "{}", run.text);
    for needle in needles {
        assert!(
            run.text.contains(needle),
            "{needle:?} not in:\n{}",
            run.text
        );
    }
}

/// Catches a step left out or run in another order, a daemon started with other flags
/// than the real one takes (the fake exits 3 on those), a server given no listener or
/// TLS flag, the probes sent inside the range or not around it (the fake server refuses
/// exactly outside what its facts state), and a run that prints no proof of a step: the
/// output names each. Server 2 to 4 and a daemon at 3: the probes are 3, 1 and 5.
#[test]
fn a_pair_that_fits_passes_every_step() {
    let run = Cell::new("fits", (2, 4), 3, "").run();
    assert!(run.ok(), "{}", run.text);
    for line in [
        "the daemon's protocol 3 is in the server's 2 to 4",
        "a Hello at protocol 3 (the daemon's) is welcomed as that protocol",
        "a Hello at protocol 1 is refused",
        "a Hello at protocol 5 is refused",
        "kbf-daemon was welcomed by kbf-server",
        // The fake API names no `protocol`, as a server that predates the field.
        "NOTE: GET /v1/nodes lists cell-node-1, connected; its entry carries no `protocol`",
    ] {
        assert!(run.text.contains(line), "{line:?} not in:\n{}", run.text);
    }
    assert_eq!(run.started(), ["kbf", "kbf", "kbf-server", "kbf-daemon"]);
    let call = |at: usize| run.calls[at].as_str();
    assert!(
        call(0).starts_with("kbf\tserver\t--version --json"),
        "{}",
        call(0)
    );
    assert!(
        call(1).starts_with("kbf\tdaemon\t--version --json"),
        "{}",
        call(1)
    );
    assert!(
        call(2).contains("--worker-listen 127.0.0.1:0"),
        "{}",
        call(2)
    );
    assert!(
        call(3).contains("--driver fake --server https://127.0.0.1:"),
        "{}",
        call(3)
    );
}

/// Catches the self-test shape (one directory as both) refused or mixed up with two, and
/// a node protocol the operator API names being compared to the wrong number. A server
/// of one protocol: the probes outside it are 0 (the wire's "not named") and 2.
#[test]
fn one_directory_is_the_self_test_and_a_named_node_protocol_is_checked() {
    let run = Cell::one_directory("self", 1, 1, "")
        .node_protocol("1")
        .run();
    assert!(run.ok(), "{}", run.text);
    for line in [
        "a Hello at protocol 0 is refused",
        "a Hello at protocol 2 is refused",
        "OK: GET /v1/nodes lists cell-node-1, connected, at protocol 1",
    ] {
        assert!(run.text.contains(line), "{line:?} not in:\n{}", run.text);
    }
    assert!(
        run.calls.iter().all(|c| c.contains("\tboth\t")),
        "{:?}",
        run.calls
    );
}

/// Catches a daemon above or below what the server accepts started anyway (the check of
/// the two builds' facts is the release that raised `min_protocol` past a shipped daemon,
/// or a daemon from a newer build than its server), either end of the range read as
/// exclusive, and a failure that names neither build's range. Nothing starts: the facts
/// are all the run has asked for.
#[test]
fn a_daemon_outside_the_servers_range_fails_before_anything_starts() {
    for (daemon, message) in [
        (
            4,
            "speaks protocol 4, but the server (build 0123456789ab) accepts 2 to 3",
        ),
        (
            1,
            "speaks protocol 1, but the server (build 0123456789ab) accepts 2 to 3",
        ),
    ] {
        let run = Cell::new("range", (2, 3), daemon, "").run();
        fails(&run, 1, &[message]);
        assert_eq!(run.started(), ["kbf", "kbf"], "{}", run.text);
    }
    // The edges are in.
    for daemon in [2, 3] {
        let run = Cell::new("edge", (2, 3), daemon, "").run();
        assert!(run.ok(), "{daemon}: {}", run.text);
    }
}

/// Catches a facts line that is not read (the run goes on with no range), a `kbf` that
/// exits non-zero taken for one that answered, a `kbf` that never answers waited for
/// without end (the run must fail within its wait, naming the build's command), and a
/// directory with no `kbf` taken for a failed check (it is a cell that cannot be driven:
/// exit 2).
#[test]
fn a_kbf_that_fails_or_hangs_or_is_missing_fails_the_run() {
    let mut cell = Cell::new("garbage", (1, 1), 1, "");
    std::fs::write(cell.server_dir.join("facts.json"), "kbf 0.1.0\n").expect("facts.json");
    fails(&cell.run(), 1, &["not one JSON object"]);

    cell = Cell::new("facts-fail", (1, 1), 1, "facts-fail");
    fails(
        &cell.run(),
        1,
        &["--version --json failed, or ran past", "kbf: broken"],
    );

    let run = Cell::new("facts-hang", (1, 1), 1, "facts-hang")
        .wait(2)
        .run();
    fails(&run, 1, &["--version --json failed, or ran past 2s"]);
    assert!(run.elapsed < Duration::from_secs(30), "{:?}", run.elapsed);

    cell = Cell::new("no-kbf", (1, 1), 1, "");
    std::fs::remove_file(cell.daemon_dir.join("kbf")).expect("remove the link");
    fails(&cell.run(), 2, &["kbf --version --json"]);
}

/// Catches a server that exits before its start line taken for a slow one (the run
/// must say it exited, at once, not wait out the deadline), a server that never prints
/// the line waited for without end, a start line without the operator API's address
/// (the run needs it) passed, and a start line whose worker address nothing listens at:
/// the probe's connection fails and the run says so.
#[test]
fn a_server_that_does_not_start_fails_the_run() {
    let run = Cell::new("server-exits", (1, 1), 1, "server-exits").run();
    fails(
        &run,
        1,
        &["kbf-server exited", "without logging \" reapi=\""],
    );
    assert!(run.elapsed < Duration::from_secs(15), "{:?}", run.elapsed);

    let run = Cell::new("server-silent", (1, 1), 1, "server-silent")
        .wait(2)
        .run();
    fails(&run, 1, &["kbf-server did not log \" reapi=\" within 2s"]);

    let run = Cell::new("no-api", (1, 1), 1, "server-no-api").run();
    fails(&run, 1, &["names no api=<address>"]);
    assert_eq!(run.started(), ["kbf", "kbf", "kbf-server"]);

    let run = Cell::new("wrong-worker", (1, 1), 1, "server-wrong-worker")
        .wait(5)
        .run();
    fails(&run, 1, &["was not welcomed", "Unavailable", "connect"]);
    assert_eq!(run.started(), ["kbf", "kbf", "kbf-server"]);
}

/// Catches the wrong refusals: a server that welcomes a protocol it should refuse (the
/// range is not enforced), one that refuses with another code, one that refuses with
/// the right code for another reason, a `Welcome` that names the server's newest
/// protocol instead of the one the Hello sent (a shipped daemon ends the session over
/// that), a server that never answers a Hello (the run must give up within its wait, not
/// hang) and one that ends the stream without a `Welcome`. In each the daemon is never
/// started: the probes come first.
#[test]
fn a_server_that_refuses_wrongly_fails_the_run() {
    for (defect, message) in [
        (
            "accepts-all",
            "was welcomed (as protocol 0); it is outside the range",
        ),
        ("refuse-invalid", "refused with InvalidArgument"),
        (
            "refuse-unnamed",
            "refused with FailedPrecondition \"this server is full\"",
        ),
        ("welcome-newest", "was welcomed as protocol 3"),
        ("hang-hello", "no answer within 2s"),
        ("silent-stream", "the first answer was not a Welcome: None"),
    ] {
        // A server accepting 1 to 3 and a daemon at 2 for the welcome, and the probe at
        // 0 for the refusals: `welcome-newest` names 3 to the daemon's 2.
        let run = Cell::new(defect, (1, 3), 2, defect).wait(2).run();
        fails(&run, 1, &[message]);
        assert_eq!(run.started(), ["kbf", "kbf", "kbf-server"], "{defect}");
    }
}

/// Catches a daemon that exits before it is welcomed taken for one still starting (the
/// run must say it exited, at once), a daemon that never logs `welcomed` waited for
/// without end (fails at the deadline, with the daemon's last log lines printed), and
/// the logs of a failed run not printed.
#[test]
fn a_daemon_that_is_never_welcomed_fails_the_run() {
    let run = Cell::new("daemon-exits", (1, 1), 1, "daemon-exits").run();
    fails(
        &run,
        1,
        &["kbf-daemon exited", "without logging \"welcomed\""],
    );
    assert!(run.elapsed < Duration::from_secs(15), "{:?}", run.elapsed);

    let run = Cell::new("daemon-silent", (1, 1), 1, "daemon-silent")
        .wait(2)
        .run();
    fails(
        &run,
        1,
        &[
            "kbf-daemon did not log \"welcomed\" within 2s",
            "--- last lines of daemon.err",
            "cannot reach a server; retrying",
            "--- last lines of server.err",
        ],
    );
    assert_eq!(run.started(), ["kbf", "kbf", "kbf-server", "kbf-daemon"]);
}

/// Catches an operator API that lists no such node, or one that is not connected,
/// passed; a node protocol of another number, of another type, or `null` (the server
/// named none) passed; and the daemon's protocol compared with the server's.
#[test]
fn a_node_the_operator_api_shows_wrongly_fails_the_run() {
    for (defect, protocol, message) in [
        ("api-empty", None, "does not list cell-node-1"),
        ("api-disconnected", None, "but not connected"),
        ("", Some("3"), "at protocol 3, but its daemon speaks 2"),
        (
            "",
            Some("\"2\""),
            "at protocol \"2\", but its daemon speaks 2",
        ),
        (
            "",
            Some("null"),
            "at protocol null, but its daemon speaks 2",
        ),
    ] {
        let mut cell = Cell::new("nodes", (1, 3), 2, defect);
        if let Some(json) = protocol {
            cell = cell.node_protocol(json);
        }
        let run = cell.run();
        fails(&run, 1, &[message]);
        assert_eq!(run.started(), ["kbf", "kbf", "kbf-server", "kbf-daemon"]);
    }
    let run = Cell::new("nodes-ok", (1, 3), 2, "")
        .node_protocol("2")
        .run();
    assert!(run.ok(), "{}", run.text);
    assert!(
        run.text
            .contains("OK: GET /v1/nodes lists cell-node-1, connected, at protocol 2"),
        "{}",
        run.text
    );
}

/// Catches `--wait 0` accepted (every step would fail at once, or never wait).
#[test]
fn a_wait_of_zero_is_refused() {
    let cell = Cell::new("wait-zero", (1, 1), 1, "").wait(0);
    let run = cell.run();
    fails(&run, 2, &["--wait", "0"]);
    assert!(run.calls.is_empty(), "{:?}", run.calls);
}

/// The self-test on the real tip, by hand: `kbf-cell cross-version` with the directory of
/// the built `kbf`, `kbf-server` and `kbf-daemon` as both. Build them first (`cargo build
/// -p kbf-server -p kbf-node -p kbf-cli -p kbf-it --bins`); the `ci / cross-version` job
/// runs the same command, so this is the way to see its run before a push.
#[test]
#[ignore = "needs the built kbf, kbf-server and kbf-daemon beside kbf-cell"]
fn the_tip_runs_against_itself() {
    let bin = std::path::Path::new(BIN)
        .parent()
        .expect("the binaries' directory");
    let out = Command::new(BIN)
        .arg("cross-version")
        .arg("--server-dir")
        .arg(bin)
        .arg("--daemon-dir")
        .arg(bin)
        .arg("--work")
        .arg(scratch("tip"))
        .output()
        .expect("start kbf-cell");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    println!("{text}");
    assert!(out.status.success(), "{text}");
}
