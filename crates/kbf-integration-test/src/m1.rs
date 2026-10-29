//! `kbf-cell m1`: the M1 exit test. One integration cell, two pinned build tools, two
//! builds each.
//!
//! The cell: an S3 store (MinIO in CI), one `kbf-server` (in-memory metadata, blobs in
//! the store, REAPI on 127.0.0.1:8980, the worker listener on 127.0.0.1:8981 over
//! mutual TLS, which also serves the daemon its blobs, the operator API on
//! `--api-listen`) and one daemon (`kbf-cell daemon`: the daemon library with the
//! test-only local runtime). The sample projects under `--projects` are copied into
//! `--work`, each with `kbf-act` from `--bin-dir` as its `tools/kbf-act`: the `shout`
//! action runs it (`kbf-act upper`), so the farm must keep that input file executable.
//!
//! What is checked, in order; the first failure ends the run with exit status 1, after
//! printing the last lines of the server's and the daemon's logs:
//! 1. the bucket is created and holds no object;
//! 2. `kbf-server` prints its ` reapi=` start line and the daemon logs `welcomed`, each
//!    within 10 seconds;
//! 3. for each tool given: the project is built remote-only, cleaned, and built again,
//!    and [`check_logs`] requires that the first build ran every action on the farm
//!    and the second answered every one of them from the action cache. buck2 also
//!    writes each build's event log (`--event-log`, gzipped JSON lines: the input of
//!    kbf-baseline, whose test captures are copied from this job's logs);
//! 4. with buck2, one more build holds a lease in flight: `//:pause` sleeps for
//!    `--pause-secs` under a fresh salt (so it runs; see the BUCK file), and while it
//!    runs the node is drained through the operator API. The drain must answer
//!    `draining` and list exactly the lease the daemon started; the build must take at
//!    least the sleep (less half a second); the node must then be `drained`, and an
//!    uncordon returns it to `serving`;
//! 5. with buck2, `//...` is built once more with `//:verdict` made to fail on the farm
//!    (`-c kbf_m1.fail=yes`): that build must fail, print `planted failure`, and leave
//!    its event log;
//! 6. the bucket holds objects: the blobs went to the store.
//!
//! **The shipping binary.** With `--kbf-bin <kbf>` the cell starts the farm as it ships
//! instead of `kbf-server`: a storage node (`kbf --mode=storage`: the S3 cold store, the
//! metadata, a listener under mutual TLS on 127.0.0.1:8983) and then the frontend
//! (`kbf --mode=frontend`: REAPI, the worker listener and the operator API on the same
//! addresses as above, `--storage` the node, a SQLite state store in `--work`). A frontend
//! holds no blobs and its worker listener serves none, so the daemon is given the storage
//! node for `--cas`. The two start lines, the checks and the logs are the same (the
//! frontend's go to `server.out` and `server.err`, the storage node's to `storage.out` and
//! `storage.err`). Without the flag nothing above changes.
//!
//! `--server-arg` adds flags to the `kbf-server` (or the `kbf --mode=frontend`) the cell starts, and
//! `--expect-scheduler-core <core>` makes the run also require, after the server starts
//! and again after the builds, that the server logged `scheduler core: <core>` and that
//! its `/metrics` says `kbf_scheduler_core{core="<core>"} 1`, so a run given
//! `--server-arg=--scheduler-core=js` cannot pass on a server that ran another core.
//!
//! Every log is under `<work>/logs`. The store's key pair comes from
//! `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, never the command line.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{Capabilities, KeyPrefix, ObjectStore as _, PageSize};
use serde_json::Value;

use crate::cell::{
    CellError, OrBroken as _, Process, absolute, copy_tree, ensure, leases_started, logged,
    print_tail, print_tails, random_hex, read, status, to_log, wait_for_line,
};
use crate::pki::PkiFiles;
use crate::summary::{Tool, check_logs};

/// The REAPI listener, as the sample projects' `.buckconfig` and `.bazelrc` name it.
const REAPI: &str = "127.0.0.1:8980";
/// The worker listener.
const WORKER: &str = "127.0.0.1:8981";
/// The storage node's listener, with `--kbf-bin`.
const STORAGE: &str = "127.0.0.1:8983";
/// The node id the daemon's certificate names.
const NODE_ID: &str = "cell-node-1";
/// How much shorter than its sleep `//:pause` may seem: the daemon logs `lease started`
/// before it fetches the inputs and runs the sleep, and the poll sees the line within a
/// tenth of a second or so.
const SLACK: Duration = Duration::from_millis(500);
/// How many lines of each log a failed run prints.
const TAIL: usize = 60;

/// `kbf-cell m1` flags.
#[derive(Clone, Debug, clap::Args)]
pub struct M1Args {
    /// The directory holding the built `kbf-server`, `kbf-cell` and `kbf-act`.
    #[arg(long)]
    pub bin_dir: PathBuf,
    /// The directory holding the sample projects `bazel/` and `buck2/`
    /// (`crates/kbf-integration-test/m1`). Each is built in a copy under `--work`.
    #[arg(long)]
    pub projects: PathBuf,
    /// The cell's directory: certificates, the API token, the projects' copies, the
    /// daemon's leases, and `logs/`.
    #[arg(long)]
    pub work: PathBuf,
    /// The S3 store, `http://host:port`.
    #[arg(long)]
    pub s3_endpoint: String,
    /// The bucket to create. It must hold no object yet.
    #[arg(long)]
    pub s3_bucket: String,
    /// Where the server's operator API listens; the drain is sent there.
    #[arg(long, default_value = "127.0.0.1:8982")]
    pub api_listen: String,
    /// How long `//:pause` sleeps, in seconds.
    #[arg(long, default_value_t = 5)]
    pub pause_secs: u64,
    /// The pinned Bazel to build the Bazel sample with.
    #[arg(long, required_unless_present = "buck2")]
    pub bazel: Option<PathBuf>,
    /// The pinned buck2 to build the buck2 sample with.
    #[arg(long)]
    pub buck2: Option<PathBuf>,
    /// Start the farm with this `kbf` (a storage node and a frontend, see the module
    /// docs) instead of `kbf-server`.
    #[arg(long)]
    pub kbf_bin: Option<PathBuf>,
    /// A flag for the frontend (`kbf-server`, or `kbf --mode=frontend` with `--kbf-bin`),
    /// after the cell's own; once for each, in the `=` form
    /// (`--server-arg=--scheduler-core=js`).
    #[arg(long = "server-arg", allow_hyphen_values = true)]
    pub server_args: Vec<String>,
    /// The scheduler core the server must report running (`legacy` or `js`): its start
    /// line and its `kbf_scheduler_core` gauge are checked, see the module docs.
    #[arg(long)]
    pub expect_scheduler_core: Option<String>,
}

/// Runs the M1 test (the module docs say what it checks). Prints what it does on
/// stdout.
///
/// # Errors
/// [`CellError::Failed`] when a check fails, [`CellError::Broken`] when the cell cannot
/// be set up or driven.
pub fn run(args: &M1Args) -> Result<(), CellError> {
    let cell = Cell::new(args)?;
    // The cell's processes are stopped when `steps` returns, before the tails print.
    let result = cell.steps();
    if result.is_err() {
        print_tails(&cell.logs, TAIL);
        if cell.kbf_bin.is_some() {
            print_tail(&cell.logs, "storage.err", TAIL);
        }
    }
    result
}

/// What one run works with. Paths are absolute: the build tools run in the copies.
struct Cell {
    bin_dir: PathBuf,
    projects: PathBuf,
    work: PathBuf,
    logs: PathBuf,
    s3_endpoint: String,
    bucket: String,
    store: S3Store,
    api_listen: String,
    http: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    pause: Duration,
    bazel: Option<PathBuf>,
    buck2: Option<PathBuf>,
    server_args: Vec<String>,
    expect_core: Option<String>,
    kbf_bin: Option<PathBuf>,
}

impl Cell {
    fn new(args: &M1Args) -> Result<Self, CellError> {
        let key = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| unset("AWS_ACCESS_KEY_ID"))?;
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY");
        let secret = secret.map_err(|_| unset("AWS_SECRET_ACCESS_KEY"))?;
        let store = S3Store::new(S3Config {
            endpoint: args.s3_endpoint.clone(),
            region: "us-east-1".to_owned(),
            bucket: args.s3_bucket.clone(),
            credentials: Credentials::new(key, secret),
            capabilities: Capabilities {
                conditional_put: true,
                object_lock: false,
            },
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            min_body_rate: None,
        });
        let store = store.or_broken(&"the store")?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build();
        let http = http.or_broken(&"the HTTP client")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let runtime = runtime.or_broken(&"the async runtime")?;
        let work = absolute(&args.work)?;
        let logs = work.join("logs");
        std::fs::create_dir_all(&logs).or_broken(&logs.display())?;
        Ok(Self {
            bin_dir: absolute(&args.bin_dir)?,
            projects: absolute(&args.projects)?,
            work,
            logs,
            s3_endpoint: args.s3_endpoint.clone(),
            bucket: args.s3_bucket.clone(),
            store,
            api_listen: args.api_listen.clone(),
            http,
            runtime,
            pause: Duration::from_secs(args.pause_secs),
            bazel: args.bazel.as_deref().map(absolute).transpose()?,
            buck2: args.buck2.as_deref().map(absolute).transpose()?,
            server_args: args.server_args.clone(),
            expect_core: args.expect_scheduler_core.clone(),
            kbf_bin: args.kbf_bin.as_deref().map(absolute).transpose()?,
        })
    }

    /// Everything after setup. The processes it starts are stopped as it returns.
    fn steps(&self) -> Result<(), CellError> {
        self.create_bucket()?;
        let pki = crate::pki::write(&self.work.join("pki"), NODE_ID);
        let pki = pki.or_broken(&"the certificates")?;
        let token = self.write_token()?;
        let _farm = self.start_farm(&pki)?;
        self.check_core()?;
        let _daemon = self.start_daemon(&pki)?;
        if let Some(bazel) = &self.bazel {
            self.bazel_sample(bazel)?;
        }
        if let Some(buck2) = &self.buck2 {
            self.buck2_sample(buck2, &token)?;
        }
        // Still the same core after the builds (a server that fell back would say so here).
        self.check_core()?;
        let held = self.objects()?;
        let bucket = &self.bucket;
        ensure(held > 0, format!("the bucket {bucket} holds no object"))?;
        println!("OK: the bucket {bucket} holds {held} object(s)");
        Ok(())
    }

    /// Creates the bucket (or finds it, owned by this key pair) and requires it empty,
    /// so that the objects counted at the end are this run's.
    fn create_bucket(&self) -> Result<(), CellError> {
        let created = self.runtime.block_on(self.store.create_bucket());
        created.or_broken(&"create the bucket")?;
        let held = self.objects()?;
        let bucket = &self.bucket;
        let why = format!("the new bucket {bucket} holds {held} object(s)");
        ensure(held == 0, why)?;
        println!("bucket {bucket} created");
        Ok(())
    }

    /// How many objects the first page of the bucket's listing holds.
    fn objects(&self) -> Result<usize, CellError> {
        let all = KeyPrefix::new("").or_broken(&"the empty prefix")?;
        let listed = self.store.list(&all, None, PageSize::MAX);
        let page = self.runtime.block_on(listed);
        let page = page.or_broken(&"list the bucket")?;
        Ok(page.objects.len())
    }

    /// Writes the operator API's write token, 64 hex digits, to a file only this user
    /// can read, and returns it.
    fn write_token(&self) -> Result<String, CellError> {
        let token = random_hex(32)?;
        let path = self.work.join("api-token");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true).mode(0o600);
        let written = options
            .open(&path)
            .and_then(|mut f| f.write_all(token.as_bytes()));
        written.or_broken(&path.display())?;
        Ok(token)
    }

    /// The processes of the farm, the frontend first (they are stopped in this order).
    fn start_farm(&self, pki: &PkiFiles) -> Result<Vec<Process>, CellError> {
        match &self.kbf_bin {
            None => Ok(vec![self.start_server(pki)?]),
            Some(kbf) => self.start_kbf(kbf, pki),
        }
    }

    /// The farm as it ships: `kbf --mode=storage`, then `kbf --mode=frontend` in front of it.
    fn start_kbf(&self, kbf: &Path, pki: &PkiFiles) -> Result<Vec<Process>, CellError> {
        // An empty settings file, so that no host file is read.
        let settings = self.work.join("kbf.toml");
        std::fs::write(&settings, "").or_broken(&settings.display())?;
        let mut storage = Command::new(kbf);
        storage
            .arg("--mode=storage")
            .arg(format!("--storage-listen={STORAGE}"))
            .arg("--cold-store")
            .arg(format!(
                "s3://{}?endpoint={}&conditional-put=true",
                self.bucket, self.s3_endpoint
            ))
            .arg("--meta-dir")
            .arg(self.work.join("storage-meta"))
            .arg("--storage-tls-cert")
            .arg(&pki.storage_cert)
            .arg("--storage-tls-key")
            .arg(&pki.storage_key)
            .arg("--storage-client-ca")
            .arg(&pki.ca_cert)
            .arg(format!("--config={}", settings.display()))
            .stdout(self.log_file("storage.out")?)
            .stderr(self.log_file("storage.err")?);
        let storage = Process::spawn(storage, "kbf --mode=storage")?;
        let out = self.logs.join("storage.out");
        let why = "kbf --mode=storage did not start";
        let line = wait_for_line(&out, "kbf-storage ", why)?;
        println!("{line}");

        let mut frontend = Command::new(kbf);
        frontend
            .arg("--mode=frontend")
            .args(["--listen", REAPI, "--worker-listen", WORKER])
            .arg("--worker-tls-cert")
            .arg(&pki.frontend_cert)
            .arg("--worker-tls-key")
            .arg(&pki.frontend_key)
            .arg("--worker-client-ca")
            .arg(&pki.ca_cert)
            .args(["--api-listen", &self.api_listen])
            .arg("--api-token-file")
            .arg(self.work.join("api-token"))
            .arg(format!("--storage=https://localhost:{}", port_of(STORAGE)))
            .arg("--storage-client-cert")
            .arg(&pki.frontend_cert)
            .arg("--storage-client-key")
            .arg(&pki.frontend_key)
            .arg("--storage-ca")
            .arg(&pki.ca_cert)
            .arg(format!(
                "--state-store=sqlite://{}",
                self.work.join("frontend-state.db").display()
            ))
            .arg(format!("--config={}", settings.display()))
            .args(&self.server_args)
            .stdout(self.log_file("server.out")?)
            .stderr(self.log_file("server.err")?);
        let frontend = Process::spawn(frontend, "kbf --mode=frontend")?;
        let out = self.logs.join("server.out");
        let line = wait_for_line(&out, " reapi=", "kbf --mode=frontend did not start")?;
        println!("{line}");
        Ok(vec![frontend, storage])
    }

    fn start_server(&self, pki: &PkiFiles) -> Result<Process, CellError> {
        let mut server = Command::new(self.bin_dir.join("kbf-server"));
        server
            .arg("--cold-store")
            .arg(format!(
                "s3://{}?endpoint={}&conditional-put=true",
                self.bucket, self.s3_endpoint
            ))
            .arg("--meta-dir")
            .arg(self.work.join("server-meta"))
            .args(["--listen", REAPI, "--worker-listen", WORKER])
            .arg("--worker-tls-cert")
            .arg(&pki.server_cert)
            .arg("--worker-tls-key")
            .arg(&pki.server_key)
            .arg("--worker-client-ca")
            .arg(&pki.ca_cert)
            .args(["--api-listen", &self.api_listen])
            .arg("--api-token-file")
            .arg(self.work.join("api-token"))
            .args(&self.server_args)
            .stdout(self.log_file("server.out")?)
            .stderr(self.log_file("server.err")?);
        let server = Process::spawn(server, "kbf-server")?;
        let out = self.logs.join("server.out");
        let line = wait_for_line(&out, " reapi=", "kbf-server did not start")?;
        println!("{line}");
        Ok(server)
    }

    /// With `--expect-scheduler-core`: requires the server's log to hold the start line of
    /// that core and its `/metrics` to say that core runs.
    fn check_core(&self) -> Result<(), CellError> {
        let Some(core) = &self.expect_core else {
            return Ok(());
        };
        let line = format!("scheduler core: {core}");
        let err = self.logs.join("server.err");
        let program = if self.kbf_bin.is_some() {
            "kbf --mode=frontend"
        } else {
            "kbf-server"
        };
        wait_for_line(&err, &line, &format!("{program} did not log `{line}`"))?;
        let url = format!("http://{}/metrics", self.api_listen);
        let body = self.runtime.block_on(async {
            let answer = self.http.get(url).send().await?.error_for_status()?;
            answer.bytes().await
        });
        let body = body.or_broken(&"the metrics")?;
        let body = String::from_utf8_lossy(&body);
        let running = format!("kbf_scheduler_core{{core=\"{core}\"}} 1");
        let gauge: Vec<&str> = body
            .lines()
            .filter(|l| l.starts_with("kbf_scheduler_core{"))
            .collect();
        let why = format!("the server does not report the {core} core running: {gauge:?}");
        ensure(gauge.contains(&running.as_str()), why)?;
        println!("OK: the server runs the {core} scheduler core (start line and gauge)");
        Ok(())
    }

    fn start_daemon(&self, pki: &PkiFiles) -> Result<Process, CellError> {
        let worker = format!("https://{WORKER}");
        // A frontend's worker listener serves no blobs: the daemon reads and writes them
        // at the storage node.
        let cas = match &self.kbf_bin {
            None => worker.clone(),
            Some(_) => format!("https://{STORAGE}"),
        };
        let mut daemon = Command::new(self.bin_dir.join("kbf-cell"));
        daemon
            .args(["daemon", "--server", &worker, "--cas", &cas])
            .arg("--ca-cert")
            .arg(&pki.ca_cert)
            .arg("--cert")
            .arg(&pki.client_cert)
            .arg("--key")
            .arg(&pki.client_key)
            .arg("--scratch")
            .arg(self.work.join("leases"))
            .stderr(self.log_file("daemon.err")?);
        let daemon = Process::spawn(daemon, "the daemon")?;
        let err = self.logs.join("daemon.err");
        wait_for_line(&err, "welcomed", "the daemon did not join")?;
        println!("daemon joined");
        Ok(daemon)
    }

    /// The Bazel sample: two builds and the check, then Bazel's server is shut down.
    fn bazel_sample(&self, bazel: &Path) -> Result<(), CellError> {
        let mut root = OsString::from("--output_user_root=");
        root.push(self.work.join("bazel-root"));
        let bazel = Program {
            name: "bazel",
            path: bazel,
            dir: self.project("bazel")?,
            prefix: vec![root],
            event_logs: false,
        };
        self.two_builds(Tool::Bazel, &bazel, &["build", "//..."])?;
        let shut = status(bazel.command().arg("shutdown"))?;
        ensure(shut, "bazel shutdown failed".to_owned())
    }

    /// The buck2 sample: two builds and the check, a lease held in flight, a build
    /// that fails; then buck2's daemon is stopped.
    fn buck2_sample(&self, buck2: &Path, token: &str) -> Result<(), CellError> {
        let buck2 = Program {
            name: "buck2",
            path: buck2,
            dir: self.project("buck2")?,
            prefix: Vec::new(),
            event_logs: true,
        };
        let build = ["build", "--console", "simple", "//..."];
        self.two_builds(Tool::Buck2, &buck2, &build)?;
        self.lease_in_flight(&buck2, token)?;
        self.failing_build(&buck2)?;
        let killed = status(buck2.command().arg("kill"))?;
        ensure(killed, "buck2 kill failed".to_owned())
    }

    /// Copies the sample project `name` into the work directory, with `kbf-act` as its
    /// `tools/kbf-act`, executable; returns the copy.
    fn project(&self, name: &str) -> Result<PathBuf, CellError> {
        let copy = self.work.join("projects").join(name);
        copy_tree(&self.projects.join(name), &copy)?;
        let tools = copy.join("tools");
        std::fs::create_dir_all(&tools).or_broken(&tools.display())?;
        let act = tools.join("kbf-act");
        std::fs::copy(self.bin_dir.join("kbf-act"), &act).or_broken(&act.display())?;
        let executable = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&act, executable).or_broken(&act.display())?;
        Ok(copy)
    }

    /// Builds with `build`, cleans (which stops buck2's daemon too, so the rebuild
    /// starts with no memory of the first), builds again, and applies the M1 rule to
    /// the two builds' summaries.
    fn two_builds(
        &self,
        tool: Tool,
        program: &Program<'_>,
        build: &[&str],
    ) -> Result<(), CellError> {
        let first = self.build_once(program, build, "first")?;
        let cleaned = status(program.command().arg("clean"))?;
        ensure(cleaned, format!("{} clean failed", program.name))?;
        let second = self.build_once(program, build, "second")?;
        let (report, passed) = check_logs(tool, &first, &second);
        print!("{report}");
        let name = program.name;
        ensure(
            passed,
            format!("the two {name} builds break the M1 rule (above)"),
        )
    }

    /// One build, its console in `<tool>-<round>.log` (and printed); returns the log.
    fn build_once(
        &self,
        program: &Program<'_>,
        build: &[&str],
        round: &str,
    ) -> Result<String, CellError> {
        let name = program.name;
        let mut command = program.command();
        command.args(build);
        if program.event_logs {
            let events = self.logs.join(format!("{name}-{round}.json-lines.gz"));
            command.arg("--event-log").arg(events);
        }
        let log = self.logs.join(format!("{name}-{round}.log"));
        let built = logged(&mut command, &log)?;
        let text = read(&log)?;
        print!("{text}");
        ensure(built, format!("the {round} {name} build failed"))?;
        Ok(text)
    }

    /// Builds `//:pause` with a fresh salt, drains the node while its lease runs, and
    /// checks what the server showed.
    fn lease_in_flight(&self, buck2: &Program<'_>, token: &str) -> Result<(), CellError> {
        let secs = self.pause.as_secs();
        let salt = random_hex(8)?;
        let daemon_log = self.logs.join("daemon.err");
        let before = leases_started(&read(&daemon_log)?).len();
        let log = self.logs.join("buck2-pause.log");
        let mut command = buck2.command();
        command
            .args(["build", "--console", "simple"])
            .args(["-c", &format!("kbf_m1.sleep_secs={secs}")])
            .args(["-c", &format!("kbf_m1.salt={salt}"), "//:pause"]);
        to_log(&mut command, &log)?;
        let mut build = Process::spawn(command, "the //:pause build")?;
        let lease = build.lease(&daemon_log, before)?;
        let start = Instant::now();
        let node = text_at(&self.nodes()?, "/nodes/0/node_id");
        let answer = self.write(&node, "drain", r#"{"deadline_secs": 600}"#, token)?;
        let state = text_at(&answer, "/placement/state");
        let held = leases_of(&answer);
        let why = format!("drained while lease {lease} ran, the server answered {answer}");
        let drained_it = (state.as_str(), held.as_str()) == ("draining", lease.as_str());
        ensure(drained_it, why)?;
        println!("OK: the server held lease {lease} in flight while draining {node}");
        let paused = build.wait()?.success();
        ensure(paused, format!("//:pause failed:\n{}", read(&log)?))?;
        let elapsed = start.elapsed();
        let ms = elapsed.as_millis();
        let why =
            format!("//:pause ended {ms} ms after its lease started, under its {secs} s sleep");
        ensure(elapsed + SLACK >= self.pause, why)?;
        let state = text_at(&self.nodes()?, "/nodes/0/placement/state");
        let why = format!("after the lease, {node} is {state}, not drained");
        ensure(state == "drained", why)?;
        let answer = self.write(&node, "uncordon", "", token)?;
        let state = text_at(&answer, "/placement/state");
        ensure(state == "serving", format!("uncordoned, {node} is {state}"))?;
        println!(
            "OK: //:pause ended {ms} ms after its lease started; {node} drained, then serving again"
        );
        Ok(())
    }

    /// `//...` again with `-c kbf_m1.fail=yes`, so `//:verdict` is a new action that
    /// exits 3 on the farm while the rest are cache hits. The build must fail, naming
    /// the planted failure, and leave its event log.
    fn failing_build(&self, buck2: &Program<'_>) -> Result<(), CellError> {
        let log = self.logs.join("buck2-failing.log");
        let events = self.logs.join("buck2-failing.json-lines.gz");
        let mut command = buck2.command();
        command
            .args(["build", "--console", "simple", "-c", "kbf_m1.fail=yes"])
            .arg("--event-log")
            .arg(&events)
            .arg("//...");
        let built = logged(&mut command, &log)?;
        let text = read(&log)?;
        let why = format!("the build with kbf_m1.fail=yes succeeded:\n{text}");
        ensure(!built, why)?;
        let why = format!("the failing build did not fail in //:verdict:\n{text}");
        ensure(text.contains("planted failure"), why)?;
        let size = std::fs::metadata(&events).map_or(0, |m| m.len());
        ensure(size > 0, "the failing build wrote no event log".to_owned())?;
        println!("OK: the build with kbf_m1.fail=yes failed in //:verdict");
        Ok(())
    }

    /// `GET /v1/nodes`.
    fn nodes(&self) -> Result<Value, CellError> {
        let url = format!("http://{}/v1/nodes", self.api_listen);
        self.json(self.http.get(url))
    }

    /// `POST /v1/nodes/<node>:<verb>` with the JSON `body`.
    fn write(&self, node: &str, verb: &str, body: &str, token: &str) -> Result<Value, CellError> {
        let url = format!("http://{}/v1/nodes/{node}:{verb}", self.api_listen);
        let request = self.http.post(url).bearer_auth(token);
        let request = request.header(reqwest::header::CONTENT_TYPE, "application/json");
        self.json(request.body(body.to_owned()))
    }

    /// The JSON answer to `request`; an error status is an error.
    fn json(&self, request: reqwest::RequestBuilder) -> Result<Value, CellError> {
        let answer = self.runtime.block_on(async {
            let answer = request.send().await?.error_for_status()?;
            answer.bytes().await
        });
        let bytes = answer.or_broken(&"the operator API")?;
        serde_json::from_slice::<Value>(&bytes).or_broken(&"the operator API's answer")
    }

    fn log_file(&self, name: &str) -> Result<File, CellError> {
        let path = self.logs.join(name);
        File::create(&path).or_broken(&path.display())
    }
}

/// A build tool, run in a project's copy.
struct Program<'a> {
    name: &'static str,
    path: &'a Path,
    dir: PathBuf,
    /// Arguments before the command (Bazel's startup options).
    prefix: Vec<OsString>,
    /// Whether builds write an event log (buck2's `--event-log`).
    event_logs: bool,
}

impl Program<'_> {
    fn command(&self) -> Command {
        let mut command = Command::new(self.path);
        command.current_dir(&self.dir).args(&self.prefix);
        command
    }
}

/// The string at `pointer` in `value`, or empty.
fn text_at(value: &Value, pointer: &str) -> String {
    let text = value.pointer(pointer).and_then(Value::as_str);
    text.unwrap_or_default().to_owned()
}

/// A placement answer's leases, `,`-joined.
fn leases_of(answer: &Value) -> String {
    let leases = answer
        .pointer("/placement/leases")
        .and_then(Value::as_array);
    let ids: Vec<&str> = leases
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    ids.join(",")
}

fn unset(name: &str) -> CellError {
    CellError::Broken(format!("{name} must be set (the store's key pair)"))
}

/// The port of a `host:port` address.
fn port_of(address: &str) -> &str {
    address.rsplit(':').next().unwrap_or(address)
}
