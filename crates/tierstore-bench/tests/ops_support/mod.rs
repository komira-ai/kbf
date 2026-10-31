//! What the tests of the ops verbs share: a fake machine, a loopback S3 stub that answers as a
//! script says, and the fixtures (ops file, run configuration) built on them.
//!
//! The fixture strings that stand for private names (`SECRET-MARK`, `bucket-mark`, `host-mark`)
//! are chosen to be found: a test that writes a report greps it for them.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use serde_json::json;
use tierstore_bench::config::{ConfigArgs, RunConfig, RunFile};
use tierstore_bench::error::BenchError;
use tierstore_bench::gate::LoadSample;
use tierstore_bench::preflight::host::{Cmd, CmdOutput, Host};
use tierstore_bench::preflight::ops::OpsFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The words a result file must not contain.
pub const PRIVATE: [&str; 5] = [
    "SECRET-MARK",
    "bucket-mark",
    "host-mark",
    "KEYID-MARK",
    "/private-mark",
];

/// The test prefix of the fixtures: every test bucket and scoped account id starts with it.
pub const TEST_PREFIX: &str = "tb-";

/// What a fake machine answers a command with.
pub type Respond = Box<dyn Fn(&Cmd) -> io::Result<CmdOutput> + Send + Sync>;

/// A success with this standard output.
pub fn ok(stdout: &str) -> io::Result<CmdOutput> {
    Ok(CmdOutput {
        success: true,
        code: Some(0),
        stdout: stdout.to_owned(),
        stderr: String::new(),
    })
}

/// An exit with `code` and this standard error.
pub fn failed(code: i32, stderr: &str) -> io::Result<CmdOutput> {
    Ok(CmdOutput {
        success: false,
        code: Some(code),
        stdout: String::new(),
        stderr: stderr.to_owned(),
    })
}

/// A machine that is a table: files, directories, open ports, a load sample and a command
/// responder, with a record of the commands it was asked to run.
pub struct FakeHost {
    pub files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    pub dirs: Mutex<BTreeSet<PathBuf>>,
    pub open_ports: Mutex<BTreeSet<u16>>,
    pub load: Mutex<Option<LoadSample>>,
    pub respond: Mutex<Respond>,
    pub commands: Mutex<Vec<Cmd>>,
    /// While set, a command does not end by itself: it waits until it is told to stop (when run
    /// with `run_until`) or, as a bound for a test whose code does not stop it, for `HANG_POLLS`
    /// milliseconds of this thread, and then answers as `respond` says.
    pub hang: AtomicBool,
    /// Set when a hanging command was told to stop.
    pub stopped: AtomicBool,
}

/// How many one-millisecond waits a hanging command is held for at most.
const HANG_POLLS: u32 = 5_000;

impl FakeHost {
    /// A machine with nothing on it, whose commands all succeed with no output.
    pub fn new() -> Self {
        Self {
            files: Mutex::default(),
            dirs: Mutex::default(),
            open_ports: Mutex::default(),
            load: Mutex::new(Some(calm())),
            respond: Mutex::new(Box::new(|_| ok(""))),
            commands: Mutex::default(),
            hang: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }
    }

    pub fn file(&self, path: &str, text: &str) -> &Self {
        self.files
            .lock()
            .unwrap()
            .insert(PathBuf::from(path), text.as_bytes().to_vec());
        self
    }

    pub fn dir(&self, path: &str) -> &Self {
        self.dirs.lock().unwrap().insert(PathBuf::from(path));
        self
    }

    pub fn answer(&self, respond: impl Fn(&Cmd) -> io::Result<CmdOutput> + Send + Sync + 'static) {
        *self.respond.lock().unwrap() = Box::new(respond);
    }

    pub fn ran(&self) -> Vec<Cmd> {
        self.commands.lock().unwrap().clone()
    }

    /// Holds a hanging command; true if `stop` was set meanwhile.
    fn hold(&self, stop: Option<&AtomicBool>) -> bool {
        for _ in 0..HANG_POLLS {
            if !self.hang.load(Ordering::SeqCst) {
                return false;
            }
            if stop.is_some_and(|s| s.load(Ordering::SeqCst)) {
                self.stopped.store(true, Ordering::SeqCst);
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }
}

impl Host for FakeHost {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().contains_key(path) || self.dirs.lock().unwrap().contains(path)
    }

    fn run(&self, cmd: &Cmd) -> io::Result<CmdOutput> {
        self.commands.lock().unwrap().push(cmd.clone());
        self.hold(None);
        (self.respond.lock().unwrap())(cmd)
    }

    fn run_until(&self, cmd: &Cmd, stop: &AtomicBool) -> io::Result<Option<CmdOutput>> {
        self.commands.lock().unwrap().push(cmd.clone());
        if self.hold(Some(stop)) {
            return Ok(None);
        }
        (self.respond.lock().unwrap())(cmd).map(Some)
    }

    fn port_open(&self, port: u16) -> bool {
        self.open_ports.lock().unwrap().contains(&port)
    }

    fn load_sample(&self) -> Result<LoadSample, BenchError> {
        self.load
            .lock()
            .unwrap()
            .ok_or(BenchError::Ops("no load sample".to_owned()))
    }
}

/// A sample inside every default limit.
pub fn calm() -> LoadSample {
    LoadSample {
        loadavg_1m: 0.5,
        psi_cpu_avg10: 0.0,
        psi_io_avg10: 0.0,
        disk_util_pct: 1.0,
    }
}

/// One request the stub saw.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub query: String,
    pub head: String,
}

impl Seen {
    /// The access key id the request was signed with.
    pub fn key_id(&self) -> &str {
        let (_, rest) = self.head.split_once("Credential=").unwrap_or_default();
        rest.split('/').next().unwrap_or_default()
    }
}

/// An S3-like HTTP stub on loopback: for each request, `answer` says the status and the body.
pub struct Stub {
    pub endpoint: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

pub type Answer = Arc<dyn Fn(&Seen) -> (u16, String) + Send + Sync>;

impl Stub {
    pub async fn start(answer: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static) -> Self {
        let answer: Answer = Arc::new(answer);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let (answer, log) = (Arc::clone(&answer), Arc::clone(&log));
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0_u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if sock.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let text = String::from_utf8_lossy(&head).into_owned();
                    let line = text.lines().next().unwrap_or_default().to_owned();
                    let mut words = line.split_whitespace();
                    let method = words.next().unwrap_or_default().to_owned();
                    let target = words.next().unwrap_or_default();
                    let (path, query) = target.split_once('?').unwrap_or((target, ""));
                    let seen = Seen {
                        method,
                        path: path.to_owned(),
                        query: query.to_owned(),
                        head: text.clone(),
                    };
                    let (status, body) = answer(&seen);
                    log.lock().unwrap().push(seen);
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    sock.write_all(reply.as_bytes()).await.ok();
                    sock.shutdown().await.ok();
                });
            }
        });
        Self { endpoint, seen }
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// An S3 `<Error>` answer.
pub fn s3_error(status: u16, code: &str) -> (u16, String) {
    (
        status,
        format!("<Error><Code>{code}</Code><Message>m</Message></Error>"),
    )
}

/// An empty ListObjectsV2 answer.
pub fn empty_listing() -> (u16, String) {
    (
        200,
        "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>".to_owned(),
    )
}

/// A ListObjectsV2 answer with `n` objects.
pub fn listing_of(n: usize) -> (u16, String) {
    let items: String = (0..n)
        .map(|i| format!("<Contents><Key>k{i}</Key><Size>1</Size></Contents>"))
        .collect();
    (
        200,
        format!("<ListBucketResult>{items}<IsTruncated>false</IsTruncated></ListBucketResult>"),
    )
}

/// A ListMultipartUploads answer with `n` open uploads.
pub fn uploads_of(n: usize) -> (u16, String) {
    let items: String = (0..n)
        .map(|i| {
            format!(
                "<Upload><Key>k{i}</Key><UploadId>u{i}</UploadId><Initiated>2026-10-29T10:00:00.000Z</Initiated></Upload>"
            )
        })
        .collect();
    (
        200,
        format!(
            "<ListMultipartUploadsResult>{items}<IsTruncated>false</IsTruncated></ListMultipartUploadsResult>"
        ),
    )
}

/// Whether the request is a ListMultipartUploads.
pub fn is_uploads(seen: &Seen) -> bool {
    seen.query.contains("uploads")
}

/// The run configuration: the cold store (and the hot one, if asked) at these endpoints.
pub fn run_config(result_dir: &Path, cold: &str, hot: Option<&str>) -> RunConfig {
    let mut stores = json!({
        "cold": {"endpoint": cold, "bucket": "tb-cold-test-bucket-mark",
                 "access_key_id": "tb-KEYID-MARK-cold", "secret_access_key": "SECRET-MARK-cold"}
    });
    if let Some(hot) = hot {
        stores["hot"] = json!({"endpoint": hot, "bucket": "tb-hot-test-bucket-mark",
                 "access_key_id": "tb-KEYID-MARK-hot", "secret_access_key": "SECRET-MARK-hot"});
    }
    config_from(result_dir, &json!({ "stores": stores }))
}

/// The hot store only.
pub fn hot_only(result_dir: &Path, hot: &str) -> RunConfig {
    config_from(
        result_dir,
        &json!({"stores": {"hot": {"endpoint": hot, "bucket": "tb-hot-test-bucket-mark",
                 "access_key_id": "tb-KEYID-MARK-hot", "secret_access_key": "SECRET-MARK-hot"}}}),
    )
}

/// A configuration with no store.
pub fn no_stores(result_dir: &Path) -> RunConfig {
    config_from(result_dir, &json!({}))
}

#[derive(Parser)]
struct Wrap {
    #[command(flatten)]
    args: ConfigArgs,
}

fn config_from(result_dir: &Path, file: &serde_json::Value) -> RunConfig {
    let dir = result_dir.to_str().unwrap();
    let args = Wrap::try_parse_from(["t", "--result-dir", dir, "--test-prefix", TEST_PREFIX])
        .unwrap()
        .args;
    let file: RunFile = serde_json::from_value(file.clone()).unwrap();
    RunConfig::resolve(&args, Some(file)).unwrap()
}

/// The ops file of the fixtures, as JSON text. `/private-mark` paths stand for the node's own.
pub fn ops_json() -> serde_json::Value {
    json!({
        "tierstore": {
            "binary": "/private-mark/pinned/tierstore",
            "sha256": hex_of(b"pinned binary"),
            "version": "0.0.37",
            "plan_flags": ["--proxy-id", "--cold-endpoint", "--hot-bytes"]
        },
        "live": {
            "unit": "tierstore-live.service",
            "binary": "/private-mark/live/tierstore",
            "port": 9300,
            "dirs": ["/private-mark/live-state"]
        },
        "test_instance": {
            "unit": "tierstore-bench-test.service",
            "port": 9411,
            "dirs": ["/private-mark/bench-state", "/private-mark/bench-data"]
        },
        "live_buckets": {"cold": "live-cold-bucket-mark", "hot": "live-hot-bucket-mark"},
        "admin": {"cold": "/private-mark/admin-cold.json", "hot": "/private-mark/admin-hot.json"},
        "verifier": {"cold": "/private-mark/reader-cold.json", "hot": "/private-mark/reader-hot.json"},
        "mc": "/private-mark/mc",
        "node": {"roles": ["instance", "generator"], "link_interface": "eth-mark"},
        "cold_heal_status_file": "/private-mark/heal.json",
        "paper": {"page": "/private-mark/paper.html", "map": "/private-mark/paper-map.json"}
    })
}

pub fn ops_from(value: &serde_json::Value) -> OpsFile {
    serde_json::from_value(value.clone()).unwrap()
}

/// SHA-256 of `bytes` as hex.
pub fn hex_of(bytes: &[u8]) -> String {
    tierstore_bench::preflight::live::sha256_hex(bytes)
}

/// A help text in clap's form with the plan's flags, `--hot` required.
pub const HELP_HOT_REQUIRED: &str = "\
tierstore: an S3 server over a hot and a cold tier.

Usage: tierstore [OPTIONS] --proxy-id <PROXY_ID> --hot <HOT> --cold <COLD> --hot-bytes <HOT_BYTES>

Options:
      --listen <LISTEN>
          The address served. Needs `--hot-endpoint` for an s3 hot tier.
      --proxy-id <PROXY_ID>
          The id of this proxy.
      --hot <HOT>
          The hot tier.
      --cold <COLD>
          The cold tier.
      --cold-endpoint <COLD_ENDPOINT>
          The endpoint of the cold tier.
      --hot-bytes <HOT_BYTES>
          The bytes the hot tier holds.
  -h, --help
          Print help
";

/// A `systemctl show` of the live unit, volatile fields included.
pub fn show_text(main_pid: u32, started: &str) -> String {
    format!(
        "Id=tierstore-live.service\nLoadState=loaded\nActiveState=active\nSubState=running\n\
UnitFileState=enabled\nFragmentPath=/private-mark/units/live.service\nDropInPaths=\nEnvironment=\n\
User=tierstore\nGroup=tierstore\nWorkingDirectory=\nMemoryMax=infinity\nCPUQuotaPerSecUSec=infinity\n\
Restart=always\nMainPID={main_pid}\nNRestarts=0\n\
ExecStart={{ path=/private-mark/live/tierstore ; argv[]=/private-mark/live/tierstore --proxy-id live --hot-bytes 100 --cold-endpoint http://host-mark:9000 ; ignore_errors=no ; start_time=[{started}] ; stop_time=[n/a] ; pid={main_pid} ; code=(null) ; status=0/0 }}\n"
    )
}

/// An `mc admin info --json` answer: this version, and drives healing or not.
pub fn info_json(version: &str, healing: bool) -> String {
    json!({"status": "success", "info": {"servers": [
        {"version": version, "drives": [{"state": "ok", "healing": false}, {"state": "ok", "healing": healing}]}
    ]}})
    .to_string()
}

/// The time of the file-mode tests, milliseconds since the Unix epoch: their clock is fixed here.
pub const NOW_MS: u64 = 1_790_000_000_000;

/// A heal status file of the node's writer, stamped `read_unix_ms`: healing, 12.5 percent, two
/// drives (one healing) and a store version.
pub fn status_file(read_unix_ms: u64) -> String {
    json!({
        "read_unix_ms": read_unix_ms,
        "healing": true,
        "percent_done": 12.5,
        "drives": [
            {"id": "d-0123abcd", "state": "ok", "healing": true},
            {"id": "d-89abcdef", "state": "ok", "healing": false}
        ],
        "version": "RELEASE.file-mark"
    })
    .to_string()
}

/// The ops file of `base` without a cold admin key: file mode for the cold role.
pub fn file_mode(ops: &OpsFile) -> OpsFile {
    let mut ops = ops.clone();
    ops.admin.remove(&tierstore_bench::role::Role::Cold);
    ops
}

pub const CPUINFO: &str = "processor\t: 0\nmodel name\t: x\n\nprocessor\t: 1\nmodel name\t: x\n";
pub const MEMINFO: &str = "MemTotal:       16384000 kB\nMemFree:        1 kB\n";
pub const OS_RELEASE: &str =
    "NAME=\"Debian GNU/Linux\"\nPRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n";

pub const PAPER: &str = "<html>\n<span class=\"tbd\">[x]</span>\ntext\nNot measured: y\nend\n";
pub const PAPER_MAP: &str = r#"[{"paper_line": 2, "cell": "a"}, {"paper_line": 4, "cell": "b"}]"#;

/// A machine on which every preflight check passes, and the commands that go with it. The cold
/// store reports a healing drive, which the status file puts at 6 percent.
pub fn healthy_host() -> FakeHost {
    let host = FakeHost::new();
    host.file("/private-mark/pinned/tierstore", "pinned binary")
        .file("/private-mark/live/tierstore", "live binary")
        .file("/proc/cpuinfo", CPUINFO)
        .file("/proc/meminfo", MEMINFO)
        .file("/etc/os-release", OS_RELEASE)
        .file("/proc/sys/kernel/osrelease", "6.1.0-mark\n")
        .file("/sys/class/net/eth-mark/speed", "10000\n")
        .file(
            "/private-mark/admin-cold.json",
            r#"{"access_key_id": "ADMIN-KEYID-MARK", "secret_access_key": "ADMIN-SECRET-MARK"}"#,
        )
        .file(
            "/private-mark/admin-hot.json",
            r#"{"access_key_id": "ADMIN-KEYID-MARK", "secret_access_key": "ADMIN-SECRET-MARK"}"#,
        )
        .file("/private-mark/heal.json", r#"{"percent_done": 6.4}"#)
        .file("/private-mark/paper.html", PAPER)
        .file("/private-mark/paper-map.json", PAPER_MAP);
    host.answer(|cmd| {
        let program = cmd.program.to_str().unwrap();
        let args = cmd.args.join(" ");
        match program {
            "/private-mark/pinned/tierstore" => ok(HELP_HOT_REQUIRED),
            "/private-mark/mc" => ok(&info_json("RELEASE.mark", true)),
            _ if args.ends_with("--value") => ok("not-found\n"),
            _ => ok(&show_text(4242, "Thu 2026-10-29 10:00:00 UTC")),
        }
    });
    host
}

/// The fixtures of a preflight: the machine, the run configuration, the ops file and the
/// scratch directory that holds them.
pub fn base(name: &str) -> (kbf_testkit::dir::TestDir, FakeHost, RunConfig, OpsFile) {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let config = run_config(
        dir.path(),
        "http://host-mark:9000",
        Some("http://host-mark:9001"),
    );
    (dir, healthy_host(), config, ops_from(&ops_json()))
}
