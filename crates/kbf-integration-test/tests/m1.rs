//! `kbf-cell m1` against fakes: every check of the M1 test passes on a cell that does
//! what M1 requires, and each one fails the run when the cell breaks it.
//!
//! The fakes are this test binary, run through links named `kbf-server`, `kbf` (a storage
//! node or a frontend, by its `--mode`), `kbf-cell`, `buck2` and `bazel` in a `--bin-dir` of the test's own: a start-up hook answers as
//! that program before the test harness starts. Each fake appends its call to
//! `<dir>/calls` and plants the defect `KBF_M1_FAKE_DEFECT` names, if it is one of its
//! own. The S3 store and the operator API are one HTTP server in the test process, with
//! the same defects.
//!
//! The real cell is checked by the `integration / m1-cell` job, which runs `kbf-cell m1`
//! against MinIO, the real server and daemon, and the pinned Bazel and buck2.

#![cfg(target_os = "linux")]

use kbf_testkit::dir::TestDir;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-cell");

/// The bytes of the fake `kbf-act` the projects' copies must hold.
const ACT: &[u8] = b"a stand-in for kbf-act\n";

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

/// The fake programs.
mod fake {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::time::Duration;

    /// The directory the fakes write `calls` to.
    pub const DIR: &str = "KBF_M1_FAKE_DIR";
    /// The defect to plant.
    pub const DEFECT: &str = "KBF_M1_FAKE_DEFECT";
    /// The daemon's log, where the fake buck2 logs the lease `//:pause` gets.
    pub const DAEMON_LOG: &str = "KBF_M1_FAKE_DAEMON_LOG";
    /// The lease `//:pause` gets.
    pub const LEASE: &str = "3.7";

    pub fn hook() {
        let mut args = std::env::args();
        let argv0 = args.next().unwrap_or_default();
        let role = Path::new(&argv0).file_name().and_then(|n| n.to_str());
        let role = role.unwrap_or_default().to_owned();
        if !matches!(
            role.as_str(),
            "kbf-server" | "kbf" | "kbf-cell" | "buck2" | "bazel"
        ) {
            return;
        }
        let args: Vec<String> = args.collect();
        record(&role, &args);
        let code = match role.as_str() {
            "kbf-server" => server(&args),
            "kbf" => kbf(&args),
            "kbf-cell" => daemon(),
            "buck2" => buck2(&args),
            _ => bazel(&args),
        };
        std::process::exit(code);
    }

    fn defect(name: &str) -> bool {
        std::env::var(DEFECT).is_ok_and(|d| d == name)
    }

    fn append(path: &str, line: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open for append");
        writeln!(file, "{line}").expect("append");
    }

    /// Appends `<role>\t<working directory>\t<arguments>` to `calls`.
    fn record(role: &str, args: &[String]) {
        let dir = std::env::var(DIR).expect("the fakes' directory");
        let cwd = std::env::current_dir().expect("working directory");
        let line = format!("{role}\t{}\t{}", cwd.display(), args.join(" "));
        append(&format!("{dir}/calls"), &line);
    }

    /// Runs until SIGINT (or a minute, so a lost one does not linger).
    fn idle() -> i32 {
        std::thread::sleep(Duration::from_secs(60));
        0
    }

    fn server(args: &[String]) -> i32 {
        // The start line of a server given the core, as the real one logs it.
        if args.iter().any(|a| a == "--scheduler-core=js") && !defect("core-line-missing") {
            eprintln!("\x1b[2mT\x1b[0m \x1b[32m INFO\x1b[0m kbf_server::start: scheduler core: js");
        }
        if !defect("server-silent") {
            // The frontend's start line has the same fields after its name.
            let name = if args.first().is_some_and(|a| a == "--mode=frontend") {
                "kbf-frontend 0"
            } else {
                "kbf-server listening"
            };
            println!("{name} reapi=127.0.0.1:8980 worker=127.0.0.1:8981");
            std::io::stdout().flush().expect("flush");
        }
        idle()
    }

    /// `kbf --mode=<mode>`: a storage node or a frontend. Like the real ones, each refuses
    /// (exit status 2) a flag that belongs to another mode, so a cell that gives a frontend
    /// the stores of `kbf-server`, or a storage node the frontend's listeners, fails.
    fn kbf(args: &[String]) -> i32 {
        let flag = |name: &str| {
            args.iter()
                .any(|a| a == name || a.starts_with(&format!("{name}=")))
        };
        let refuses = |names: &[&'static str]| -> Option<&'static str> {
            names.iter().copied().find(|n| flag(n))
        };
        match args.first().map(String::as_str) {
            Some("--mode=storage") => {
                let other = ["--worker-listen", "--state-store", "--storage", "--listen"];
                if let Some(name) = refuses(&other) {
                    eprintln!("error: unexpected argument '{name}' for kbf --mode=storage");
                    return 2;
                }
                if !defect("storage-silent") {
                    println!("kbf-storage 0 storage=127.0.0.1:8983");
                    std::io::stdout().flush().expect("flush");
                }
                idle()
            }
            Some("--mode=frontend") => {
                let other = ["--cold-store", "--meta-dir", "--storage-listen"];
                if let Some(name) = refuses(&other) {
                    eprintln!("error: unexpected argument '{name}' for kbf --mode=frontend");
                    return 2;
                }
                if !flag("--storage") {
                    eprintln!(
                        "error: the following required arguments were not provided: --storage"
                    );
                    return 2;
                }
                server(args)
            }
            _ => {
                eprintln!("error: the fake kbf runs --mode=storage or --mode=frontend");
                2
            }
        }
    }

    fn daemon() -> i32 {
        if !defect("daemon-silent") {
            eprintln!("\x1b[2mT\x1b[0m \x1b[32m INFO\x1b[0m kbf_daemon: welcomed");
        }
        idle()
    }

    /// Whether the project's `tools/kbf-act` is there and executable, as an action that
    /// runs it needs.
    fn act_runs() -> bool {
        let mode = std::fs::metadata("tools/kbf-act").map(|m| m.permissions().mode());
        mode.is_ok_and(|m| m & 0o111 != 0)
    }

    fn buck2(args: &[String]) -> i32 {
        let has = |word: &str| args.iter().any(|a| a == word);
        if has("clean") {
            return i32::from(defect("clean-fails"));
        }
        if has("kill") {
            return 0;
        }
        if !act_runs() {
            eprintln!("tools/kbf-act: Permission denied");
            return 1;
        }
        let events = args.iter().position(|a| a == "--event-log");
        let events = events.map(|i| args[i + 1].clone());
        if has("//:pause") {
            if !defect("no-lease") {
                let log = std::env::var(DAEMON_LOG).expect("the daemon's log");
                let lease = format!(
                    "\x1b[2mT\x1b[0m \x1b[32m INFO\x1b[0m kbf_daemon::lease: lease started \
                     \x1b[3mlease\x1b[0m\x1b[2m=\x1b[0m{LEASE} \
                     \x1b[3mdriver\x1b[0m\x1b[2m=\x1b[0m\"local\""
                );
                append(&log, &lease);
            }
            let secs = args
                .iter()
                .find_map(|a| a.strip_prefix("kbf_m1.sleep_secs="));
            let secs: u64 = secs.expect("a sleep").parse().expect("seconds");
            if !defect("pause-quick") {
                std::thread::sleep(Duration::from_secs(secs));
            }
            return i32::from(defect("pause-fails"));
        }
        let failing = has("kbf_m1.fail=yes");
        if let Some(events) = &events
            && !(failing && defect("no-event-log"))
        {
            std::fs::write(events, b"\x1f\x8b events").expect("event log");
        }
        if failing {
            if !defect("fail-silent") {
                eprintln!("Action failed: root//:verdict\nplanted failure");
            }
            return if defect("fail-passes") { 0 } else { 3 };
        }
        let second = events.is_some_and(|e| e.contains("second"));
        summary(
            second,
            "Commands: 5 (cached: 0, remote: 5, local: 0)",
            |hits| {
                format!(
                    "Commands: 5 (cached: {hits}, remote: {}, local: 0)",
                    5 - hits
                )
            },
        )
    }

    fn bazel(args: &[String]) -> i32 {
        match args.get(1).map(String::as_str) {
            Some("clean") => return i32::from(defect("clean-fails")),
            Some("shutdown") => return 0,
            _ => {}
        }
        if !act_runs() {
            eprintln!("tools/kbf-act: Permission denied");
            return 1;
        }
        let dir = std::env::var(DIR).expect("the fakes' directory");
        let calls = std::fs::read_to_string(format!("{dir}/calls")).expect("calls");
        let builds = calls
            .lines()
            .filter(|l| l.starts_with("bazel\t") && l.ends_with(" build //..."))
            .count();
        summary(
            builds > 1,
            "INFO: 7 processes: 4 internal, 3 remote.",
            |hits| match 3 - hits {
                0 => format!("INFO: 7 processes: 4 internal, {hits} remote cache hit."),
                ran => {
                    format!("INFO: 7 processes: 4 internal, {ran} remote, {hits} remote cache hit.")
                }
            },
        )
    }

    /// Prints a build's summary: `first` for a first build, else `rebuild(hits)`, all
    /// hits but one under the defect `second-runs`. Under `build-fails`, a first build
    /// fails instead.
    fn summary(second: bool, first: &str, rebuild: impl Fn(u64) -> String) -> i32 {
        if !second {
            if defect("build-fails") {
                eprintln!("Action failed: shout");
                return 1;
            }
            println!("{first}");
            return 0;
        }
        let all = if first.starts_with("INFO") { 3 } else { 5 };
        let hits = if defect("second-runs") { all - 1 } else { all };
        println!("{}", rebuild(hits));
        0
    }
}

/// One request the HTTP fake answered.
#[derive(Clone, Debug)]
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        let found = self.headers.iter().find(|(n, _)| n == name);
        found.map(|(_, v)| v.as_str())
    }
}

/// The S3 store (bucket `kbf-m1`) and the operator API (node `cell-node-1`).
struct Http {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
}

#[derive(Default)]
struct Counts {
    lists: AtomicUsize,
    nodes: AtomicUsize,
}

impl Http {
    fn serve(defect: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (seen, defect) = (Arc::clone(&requests), defect.to_owned());
        std::thread::spawn(move || {
            let counts = Counts::default();
            for stream in listener.incoming().flatten() {
                handle(stream, &defect, &counts, &seen);
            }
        });
        Self { addr, requests }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("requests").clone()
    }
}

fn handle(mut stream: TcpStream, defect: &str, counts: &Counts, seen: &Mutex<Vec<Request>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("request line");
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or_default().to_owned();
    let target = words.next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header");
        let Some((name, value)) = line.trim_end().split_once(':') else {
            break;
        };
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let length = headers.iter().find(|(n, _)| n == "content-length");
    let length = length.map_or(0, |(_, v)| v.parse().expect("a length"));
    let mut body = vec![0; length];
    reader.read_exact(&mut body).expect("body");
    let request = Request {
        method,
        target,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    let (status, answer) = answer(&request, defect, counts);
    seen.lock().expect("requests").push(request);
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        answer.len()
    );
    // The client may have given up already; that is its test's failure to report.
    let _ = stream.write_all(format!("{head}{answer}").as_bytes());
}

fn answer(request: &Request, defect: &str, counts: &Counts) -> (&'static str, String) {
    let path = request.target.split('?').next().unwrap_or_default();
    let has = |d: &str| defect == d;
    let placement = |state: &str, leases: &str| {
        format!(r#"{{"node_id":"cell-node-1","placement":{{"state":"{state}"{leases}}}}}"#)
    };
    match (request.method.as_str(), path) {
        ("PUT", "/kbf-m1") if has("bucket-refused") => (
            "403 Forbidden",
            "<Error><Code>AccessDenied</Code><Message>refused</Message></Error>".to_owned(),
        ),
        ("PUT", "/kbf-m1") => ("200 OK", String::new()),
        ("GET", "/kbf-m1") => {
            let n = counts.lists.fetch_add(1, Ordering::SeqCst);
            let objects = match n {
                0 => usize::from(has("bucket-not-empty")),
                _ if has("bucket-empty") => 0,
                _ => 3,
            };
            let contents: String = (0..objects)
                .map(|i| format!("<Contents><Key>cas/{i}</Key><Size>1</Size></Contents>"))
                .collect();
            let xml = format!(
                "<ListBucketResult><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>"
            );
            ("200 OK", xml)
        }
        ("GET", "/v1/nodes") => {
            let n = counts.nodes.fetch_add(1, Ordering::SeqCst);
            let state = match n {
                0 => "serving",
                _ if has("not-drained") => "draining",
                _ => "drained",
            };
            let nodes = format!(r#"{{"nodes":[{}]}}"#, placement(state, ""));
            ("200 OK", nodes)
        }
        ("POST", "/v1/nodes/cell-node-1:drain") => {
            let state = if has("not-draining") {
                "cordoned"
            } else {
                "draining"
            };
            let lease = if has("wrong-lease") {
                "3.8"
            } else {
                fake::LEASE
            };
            let leases = format!(r#","deadline_unix_ms":1,"leases":["{lease}"]"#);
            ("200 OK", placement(state, &leases))
        }
        ("POST", "/v1/nodes/cell-node-1:uncordon") => {
            let state = if has("stays-cordoned") {
                "cordoned"
            } else {
                "serving"
            };
            ("200 OK", placement(state, ""))
        }
        ("GET", "/metrics") => {
            let (legacy, js) = if has("gauge-legacy") { (1, 0) } else { (0, 1) };
            let body = format!(
                "# TYPE kbf_scheduler_core gauge\nkbf_scheduler_core{{core=\"legacy\"}} {legacy}\n\
                 kbf_scheduler_core{{core=\"js\"}} {js}\nkbf_other 7\n"
            );
            ("200 OK", body)
        }
        _ => ("404 Not Found", String::new()),
    }
}

/// A fresh directory for one run of one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("kbf-m1-tests"),
        name,
    )
}

/// One fake cell: the fakes in `<dir>/bin`, the HTTP fake, and `kbf-cell m1`'s work
/// directory `<dir>/work`.
struct Cell {
    defect: String,
    http: Http,
    /// Last: removed after the fake server is gone.
    dir: TestDir,
}

impl Cell {
    fn new(name: &str, defect: &str) -> Self {
        let dir = scratch(name);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let me = std::env::current_exe().expect("this test binary");
        for fake in ["kbf-server", "kbf", "kbf-cell", "buck2", "bazel"] {
            std::os::unix::fs::symlink(&me, bin.join(fake)).expect("link a fake");
        }
        // Not executable: `kbf-cell m1` must make the copies executable itself.
        let act = bin.join("kbf-act");
        std::fs::write(&act, ACT).expect("kbf-act");
        let mode = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(&act, mode).expect("chmod");
        Self {
            dir,
            defect: defect.to_owned(),
            http: Http::serve(defect),
        }
    }

    fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    /// `kbf-cell m1` with the fakes, the given tools (`bazel`, `buck2`) and a 1 s pause.
    fn command(&self, tools: &[&str]) -> Command {
        let bin = self.dir.join("bin");
        let mut command = Command::new(BIN);
        command
            .arg("m1")
            .arg("--bin-dir")
            .arg(&bin)
            .arg("--projects")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("m1"))
            .arg("--work")
            .arg(self.work())
            .arg("--s3-endpoint")
            .arg(format!("http://{}", self.http.addr))
            .args(["--s3-bucket", "kbf-m1", "--pause-secs", "1"])
            .arg("--api-listen")
            .arg(self.http.addr.to_string())
            .env("AWS_ACCESS_KEY_ID", "test-access")
            .env("AWS_SECRET_ACCESS_KEY", "test-secret")
            .env(fake::DIR, self.dir.path())
            .env(fake::DEFECT, &self.defect)
            .env(fake::DAEMON_LOG, self.work().join("logs/daemon.err"));
        for tool in tools {
            command.arg(format!("--{tool}")).arg(bin.join(tool));
        }
        command
    }

    fn run(&self, tools: &[&str]) -> Output {
        self.command(tools).output().expect("run kbf-cell m1")
    }

    /// The calls of the fake `role`: its working directory and its arguments.
    fn calls(&self, role: &str) -> Vec<(String, String)> {
        let calls = std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default();
        calls
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(r, _)| *r == role)
            .filter_map(|(_, rest)| rest.split_once('\t'))
            .map(|(cwd, args)| (cwd.to_owned(), args.to_owned()))
            .collect()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn both(out: &Output) -> String {
    format!(
        "status {}\n--- stdout\n{}--- stderr\n{}",
        out.status,
        text(&out.stdout),
        text(&out.stderr)
    )
}

/// Runs `kbf-cell m1` with both tools and the defect `defect`, and requires exit status
/// `code` with `why` on stderr and the logs' tails on stdout.
fn fails(defect: &str, code: i32, why: &str) -> (Cell, Output) {
    let cell = Cell::new(defect, defect);
    let out = cell.run(&["bazel", "buck2"]);
    assert_eq!(out.status.code(), Some(code), "{}", both(&out));
    assert!(
        text(&out.stderr).contains(why),
        "wanted {why:?}\n{}",
        both(&out)
    );
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("--- last lines of server.err"),
        "{}",
        both(&out)
    );
    assert!(
        stdout.contains("--- last lines of daemon.err"),
        "{}",
        both(&out)
    );
    (cell, out)
}

/// Catches: a harness that starts the server or the daemon with other flags than the
/// M1 cell needs, builds in another order or place, drains without the token or with
/// another body, leaves the projects' `kbf-act` copy unexecutable (the fake build
/// fails as the farm's action would), or passes without one of its OK lines.
#[test]
fn a_cell_that_does_everything_passes() {
    let cell = Cell::new("passes", "none");
    let out = cell.run(&["bazel", "buck2"]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let stdout = text(&out.stdout);
    for line in [
        "bucket kbf-m1 created",
        "kbf-server listening reapi=127.0.0.1:8980",
        "daemon joined",
        "OK Bazel: 3 of 3 action(s) ran on the farm, then all were remote cache hits",
        "OK Buck2: 5 of 5 action(s) ran on the farm, then all were remote cache hits",
        "OK: the server held lease 3.7 in flight while draining cell-node-1",
        "after its lease started; cell-node-1 drained, then serving again",
        "OK: the build with kbf_m1.fail=yes failed in //:verdict",
        "OK: the bucket kbf-m1 holds 3 object(s)",
    ] {
        assert!(stdout.contains(line), "no {line:?}\n{}", both(&out));
    }
    assert!(!stdout.contains("--- last lines"), "{}", both(&out));

    let work = cell.work();
    let w = work.display();
    let api = cell.http.addr;
    let server = cell.calls("kbf-server");
    let expected = format!(
        "--cold-store s3://kbf-m1?endpoint=http://{api}&conditional-put=true \
         --meta-dir {w}/server-meta --listen 127.0.0.1:8980 --worker-listen 127.0.0.1:8981 \
         --worker-tls-cert {w}/pki/server.pem --worker-tls-key {w}/pki/server.key \
         --worker-client-ca {w}/pki/ca.pem --api-listen {api} --api-token-file {w}/api-token"
    );
    assert_eq!(server.len(), 1);
    assert_eq!(server[0].1, expected);
    assert!(cell.calls("kbf").is_empty(), "no storage node or frontend");
    let daemon = cell.calls("kbf-cell");
    let expected = format!(
        "daemon --server https://127.0.0.1:8981 --cas https://127.0.0.1:8981 \
         --ca-cert {w}/pki/ca.pem --cert {w}/pki/client.pem --key {w}/pki/client.key \
         --scratch {w}/leases"
    );
    assert_eq!(daemon.len(), 1);
    assert_eq!(daemon[0].1, expected);

    let bazel = cell.calls("bazel");
    let root = format!("--output_user_root={w}/bazel-root");
    let expected: Vec<(String, String)> = ["build //...", "clean", "build //...", "shutdown"]
        .iter()
        .map(|args| (format!("{w}/projects/bazel"), format!("{root} {args}")))
        .collect();
    assert_eq!(bazel, expected);

    let buck2 = cell.calls("buck2");
    let args: Vec<&str> = buck2.iter().map(|(_, a)| a.as_str()).collect();
    assert!(
        buck2
            .iter()
            .all(|(cwd, _)| *cwd == format!("{w}/projects/buck2"))
    );
    assert_eq!(args.len(), 6, "{args:?}");
    let logs = format!("{w}/logs");
    assert_eq!(
        args[0],
        format!("build --console simple //... --event-log {logs}/buck2-first.json-lines.gz")
    );
    assert_eq!(args[1], "clean");
    assert_eq!(
        args[2],
        format!("build --console simple //... --event-log {logs}/buck2-second.json-lines.gz")
    );
    let salt = args[3]
        .strip_prefix("build --console simple -c kbf_m1.sleep_secs=1 -c kbf_m1.salt=")
        .and_then(|rest| rest.strip_suffix(" //:pause"))
        .unwrap_or_else(|| panic!("the pause build: {}", args[3]));
    assert_eq!(salt.len(), 16, "{salt}");
    assert!(
        salt.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_eq!(
        args[4],
        format!(
            "build --console simple -c kbf_m1.fail=yes --event-log \
             {logs}/buck2-failing.json-lines.gz //..."
        )
    );
    assert_eq!(args[5], "kill");
    let calls = std::fs::read_to_string(cell.dir.join("calls")).expect("calls");
    let first_buck2 = calls.find("buck2\t").expect("a buck2 call");
    assert!(
        calls.rfind("bazel\t").expect("a bazel call") < first_buck2,
        "{calls}"
    );

    // The token: 64 hex digits that only this user can read, on both writes.
    let token = std::fs::read_to_string(work.join("api-token")).expect("token");
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    let mode = std::fs::metadata(work.join("api-token"))
        .expect("token")
        .permissions();
    assert_eq!(mode.mode() & 0o777, 0o600);
    let requests = cell.http.requests();
    let writes: Vec<&Request> = requests.iter().filter(|r| r.method == "POST").collect();
    let targets: Vec<&str> = writes.iter().map(|r| r.target.as_str()).collect();
    assert_eq!(
        targets,
        [
            "/v1/nodes/cell-node-1:drain",
            "/v1/nodes/cell-node-1:uncordon"
        ]
    );
    for write in &writes {
        let bearer = format!("Bearer {token}");
        assert_eq!(write.header("authorization"), Some(bearer.as_str()));
        assert_eq!(write.header("content-type"), Some("application/json"));
    }
    assert_eq!(writes[0].body, r#"{"deadline_secs": 600}"#);
    assert_eq!(writes[1].body, "");

    // Each copy holds the project and `kbf-act`, executable.
    for project in ["bazel", "buck2"] {
        let act = work.join("projects").join(project).join("tools/kbf-act");
        assert_eq!(std::fs::read(&act).expect("the copy"), ACT);
        let mode = std::fs::metadata(&act).expect("the copy").permissions();
        assert_eq!(mode.mode() & 0o777, 0o755, "{}", act.display());
    }
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("m1/buck2/BUCK");
    let copy = work.join("projects/buck2/BUCK");
    assert_eq!(std::fs::read(copy).ok(), std::fs::read(source).ok());
    let copy = work.join("projects/buck2/platforms/defs.bzl");
    assert!(copy.is_file(), "{}", copy.display());
    for log in [
        "buck2-first.log",
        "buck2-second.log",
        "bazel-first.log",
        "buck2-pause.log",
    ] {
        assert!(work.join("logs").join(log).is_file(), "{log}");
    }
}

/// Catches: a harness that runs or starts buck2 when only Bazel is given.
#[test]
fn bazel_alone_runs_only_bazel() {
    let cell = Cell::new("bazel-only", "none");
    let out = cell.run(&["bazel"]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    assert_eq!(cell.calls("bazel").len(), 4);
    assert!(cell.calls("buck2").is_empty());
    assert!(text(&out.stdout).contains("OK: the bucket kbf-m1 holds 3 object(s)"));
}

/// Catches: a harness that runs Bazel when only buck2 is given.
#[test]
fn buck2_alone_runs_only_buck2() {
    let cell = Cell::new("buck2-only", "none");
    let out = cell.run(&["buck2"]);
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    assert!(cell.calls("bazel").is_empty());
    assert_eq!(cell.calls("buck2").len(), 6);
}

/// Catches: a run with no tool, which would check nothing and pass.
#[test]
fn no_tool_is_a_usage_error() {
    let cell = Cell::new("no-tool", "none");
    let out = cell.run(&[]);
    assert_eq!(out.status.code(), Some(2), "{}", both(&out));
    assert!(text(&out.stderr).contains("--bazel"), "{}", both(&out));
    assert!(cell.calls("kbf-server").is_empty());
}

/// Catches: a run without the store's key pair, which must not start anything.
#[test]
fn the_key_pair_is_required() {
    let cell = Cell::new("no-keys", "none");
    for name in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] {
        let mut command = cell.command(&["buck2"]);
        let out = command.env_remove(name).output().expect("run kbf-cell m1");
        assert_eq!(out.status.code(), Some(2), "{}", both(&out));
        let why = format!("{name} must be set");
        assert!(text(&out.stderr).contains(&why), "{}", both(&out));
    }
    assert!(cell.calls("kbf-server").is_empty());
}

/// Catches: a harness that goes on without the bucket.
#[test]
fn a_bucket_the_store_refuses_fails() {
    let (cell, _) = fails("bucket-refused", 2, "create the bucket");
    assert!(cell.calls("kbf-server").is_empty());
}

/// Catches: a harness that counts objects some other run left (the old script's
/// create refused an existing bucket).
#[test]
fn a_bucket_that_holds_objects_fails() {
    fails(
        "bucket-not-empty",
        1,
        "FAIL: the new bucket kbf-m1 holds 1 object(s)",
    );
}

/// Catches: a harness that goes on without the server's start line.
#[test]
fn a_server_that_does_not_start_fails() {
    fails("server-silent", 1, "FAIL: kbf-server did not start");
}

/// Catches: a harness that goes on without the daemon's `welcomed`.
#[test]
fn a_daemon_that_does_not_join_fails() {
    fails("daemon-silent", 1, "FAIL: the daemon did not join");
}

/// Catches: a harness that ignores a failed build.
#[test]
fn a_failed_build_fails() {
    fails("build-fails", 1, "FAIL: the first bazel build failed");
}

/// Catches: a harness that ignores a failed clean.
#[test]
fn a_failed_clean_fails() {
    fails("clean-fails", 1, "FAIL: bazel clean failed");
}

/// Catches: a harness that skips the M1 rule: a rebuild that ran an action again.
#[test]
fn a_rebuild_that_runs_an_action_fails() {
    let (_, out) = fails(
        "second-runs",
        1,
        "FAIL: the two bazel builds break the M1 rule",
    );
    assert!(text(&out.stdout).contains("FAIL Bazel"), "{}", both(&out));
}

/// Catches: a harness that drains with no lease in flight, or waits for one forever
/// after the build has ended.
#[test]
fn a_pause_that_starts_no_lease_fails() {
    fails(
        "no-lease",
        1,
        "FAIL: //:pause started no lease on the daemon",
    );
}

/// Catches: a drain check that reads only the state, not the lease held.
#[test]
fn a_drain_that_holds_another_lease_fails() {
    fails("wrong-lease", 1, "FAIL: drained while lease 3.7 ran");
}

/// Catches: a drain check that reads only the lease, not the state.
#[test]
fn a_drain_that_does_not_drain_fails() {
    fails("not-draining", 1, "FAIL: drained while lease 3.7 ran");
}

/// Catches: a harness that ignores a failed `//:pause`.
#[test]
fn a_failed_pause_fails() {
    fails("pause-fails", 1, "FAIL: //:pause failed");
}

/// Catches: a harness that does not time `//:pause` (a lease that ended at once was
/// not held in flight).
#[test]
fn a_pause_shorter_than_its_sleep_fails() {
    fails("pause-quick", 1, "under its 1 s sleep");
}

/// Catches: a harness that does not check the node drained once the lease ended.
#[test]
fn a_node_still_draining_after_the_lease_fails() {
    fails(
        "not-drained",
        1,
        "FAIL: after the lease, cell-node-1 is draining",
    );
}

/// Catches: a harness that does not check the uncordon.
#[test]
fn an_uncordon_that_does_not_serve_fails() {
    fails(
        "stays-cordoned",
        1,
        "FAIL: uncordoned, cell-node-1 is cordoned",
    );
}

/// Catches: a harness that does not require the planted failure to fail the build.
#[test]
fn a_failing_build_that_succeeds_fails() {
    fails(
        "fail-passes",
        1,
        "FAIL: the build with kbf_m1.fail=yes succeeded",
    );
}

/// Catches: a harness that accepts a build failing anywhere but `//:verdict`.
#[test]
fn a_failing_build_without_the_planted_failure_fails() {
    fails(
        "fail-silent",
        1,
        "FAIL: the failing build did not fail in //:verdict",
    );
}

/// Catches: a harness that does not require the failing build's event log.
#[test]
fn a_failing_build_without_an_event_log_fails() {
    fails(
        "no-event-log",
        1,
        "FAIL: the failing build wrote no event log",
    );
}

/// Catches: a harness that does not check the blobs reached the store.
#[test]
fn a_bucket_left_empty_fails() {
    fails("bucket-empty", 1, "FAIL: the bucket kbf-m1 holds no object");
}

/// `kbf-cell m1` as the js-core gate runs it: the flags for the server and the core it must
/// report.
fn js_core_command(cell: &Cell) -> Command {
    let mut command = cell.command(&["buck2"]);
    command.args([
        "--server-arg=--scheduler-core=js",
        "--server-arg=--placement-load=on",
        "--expect-scheduler-core",
        "js",
    ]);
    command
}

/// Catches: `--server-arg` flags that do not reach `kbf-server` (in order, after the cell's
/// own), and `--expect-scheduler-core` that does not read the start line and the gauge
/// both before the builds and after them.
#[test]
fn a_server_given_the_js_core_that_reports_it_passes() {
    let cell = Cell::new("js-core", "none");
    let out = js_core_command(&cell).output().expect("run kbf-cell m1");
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let ok = "OK: the server runs the js scheduler core (start line and gauge)";
    assert_eq!(text(&out.stdout).matches(ok).count(), 2, "{}", both(&out));
    let w = cell.work();
    let server = cell.calls("kbf-server");
    assert_eq!(server.len(), 1);
    let tail = format!(
        "--api-token-file {}/api-token --scheduler-core=js --placement-load=on",
        w.display()
    );
    assert!(server[0].1.ends_with(&tail), "{}", server[0].1);
    let scrapes = cell
        .http
        .requests()
        .iter()
        .filter(|r| r.target == "/metrics")
        .count();
    assert_eq!(scrapes, 2);
}

/// Catches: a run that expects a core and passes when the server never logged it (the
/// gauge alone would pass on a server that reports the core but did not start it).
#[test]
fn a_server_that_does_not_log_the_expected_core_fails() {
    let cell = Cell::new("js-core-silent", "core-line-missing");
    let out = js_core_command(&cell).output().expect("run kbf-cell m1");
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    let why = "FAIL: kbf-server did not log `scheduler core: js`";
    assert!(text(&out.stderr).contains(why), "{}", both(&out));
    assert!(cell.calls("kbf-cell").is_empty(), "no daemon starts");
}

/// Catches: a run that expects the js core and passes on a server whose gauge says legacy
/// (a server that logged the flag but ran the legacy driver).
#[test]
fn a_server_whose_gauge_says_another_core_fails() {
    let cell = Cell::new("js-core-gauge", "gauge-legacy");
    let out = js_core_command(&cell).output().expect("run kbf-cell m1");
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    let why = "FAIL: the server does not report the js core running";
    assert!(text(&out.stderr).contains(why), "{}", both(&out));
}

/// `kbf-cell m1` as the js-core gate runs it: the shipping `kbf` for the farm, the frontend's
/// flags and the core it must report.
fn shipping_command(cell: &Cell) -> Command {
    let mut command = js_core_command(cell);
    command.arg("--kbf-bin").arg(cell.dir.join("bin/kbf"));
    command
}

/// Catches: `--kbf-bin` that starts `kbf-server` anyway, starts the frontend before its
/// storage node, gives either mode another mode's flags (the fakes refuse them, as the real
/// ones do), leaves the daemon on the frontend's worker listener for its blobs, lets
/// `--server-arg` flags reach the storage node instead of the frontend, or loses the
/// two checks of the core.
#[test]
fn the_shipping_binary_runs_a_storage_node_and_a_frontend_and_passes() {
    let cell = Cell::new("kbf-bin", "none");
    let out = shipping_command(&cell).output().expect("run kbf-cell m1");
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let stdout = text(&out.stdout);
    for line in [
        "kbf-storage 0 storage=127.0.0.1:8983",
        "kbf-frontend 0 reapi=127.0.0.1:8980",
        "daemon joined",
        "OK: the server runs the js scheduler core (start line and gauge)",
    ] {
        assert!(stdout.contains(line), "no {line:?}\n{}", both(&out));
    }
    assert!(cell.calls("kbf-server").is_empty(), "{}", both(&out));
    let w = cell.work();
    let w = w.display();
    let api = cell.http.addr;
    let calls = cell.calls("kbf");
    let args: Vec<&str> = calls.iter().map(|(_, a)| a.as_str()).collect();
    let storage = format!(
        "--mode=storage --storage-listen=127.0.0.1:8983 \
         --cold-store s3://kbf-m1?endpoint=http://{api}&conditional-put=true \
         --meta-dir {w}/storage-meta --storage-tls-cert {w}/pki/storage.pem \
         --storage-tls-key {w}/pki/storage.key --storage-client-ca {w}/pki/ca.pem \
         --config={w}/kbf.toml"
    );
    let frontend = format!(
        "--mode=frontend --listen 127.0.0.1:8980 --worker-listen 127.0.0.1:8981 \
         --worker-tls-cert {w}/pki/frontend.pem --worker-tls-key {w}/pki/frontend.key \
         --worker-client-ca {w}/pki/ca.pem --api-listen {api} --api-token-file {w}/api-token \
         --storage=https://localhost:8983 --storage-client-cert {w}/pki/frontend.pem \
         --storage-client-key {w}/pki/frontend.key --storage-ca {w}/pki/ca.pem \
         --state-store=sqlite://{w}/frontend-state.db --config={w}/kbf.toml \
         --scheduler-core=js --placement-load=on"
    );
    assert_eq!(args, [storage.as_str(), frontend.as_str()]);
    let daemon = cell.calls("kbf-cell");
    assert_eq!(daemon.len(), 1);
    let blobs = "--server https://127.0.0.1:8981 --cas https://127.0.0.1:8983 --ca-cert";
    assert!(daemon[0].1.contains(blobs), "{}", daemon[0].1);
    let scrapes = cell.http.requests();
    let scrapes = scrapes.iter().filter(|r| r.target == "/metrics").count();
    assert_eq!(scrapes, 2);
}

/// Catches: a cell on the shipping binary that goes on, or reports a server failure, when
/// the storage node never starts (the frontend must not start in front of it, and the
/// storage node's own log is part of the failure's output).
#[test]
fn a_storage_node_that_does_not_start_fails() {
    let cell = Cell::new("kbf-bin-storage-silent", "storage-silent");
    let out = shipping_command(&cell).output().expect("run kbf-cell m1");
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    let why = "FAIL: kbf --mode=storage did not start";
    assert!(text(&out.stderr).contains(why), "{}", both(&out));
    assert!(text(&out.stdout).contains("--- last lines of storage.err"));
    assert_eq!(cell.calls("kbf").len(), 1, "only the storage node ran");
}

/// Catches: a frontend that never logs the core it was asked for passing on the shipping
/// binary, and a failure that names the wrong program.
#[test]
fn a_frontend_that_does_not_log_the_expected_core_fails() {
    let cell = Cell::new("kbf-bin-core-silent", "core-line-missing");
    let out = shipping_command(&cell).output().expect("run kbf-cell m1");
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    let why = "FAIL: kbf --mode=frontend did not log `scheduler core: js`";
    assert!(text(&out.stderr).contains(why), "{}", both(&out));
    assert!(cell.calls("kbf-cell").is_empty(), "no daemon starts");
}
