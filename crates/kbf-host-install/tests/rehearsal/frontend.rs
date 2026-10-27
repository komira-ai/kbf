//! The frontend container of the frontend rehearsal: what the host runs, the configuration the
//! helper is given for it, and what the rehearsal looks at while the helper replaces it (the
//! container's own answers, and Docker's record of what was done to it).
//!
//! The container is a frontend of release N on the host's network, with a SQLite state store, the
//! storage node of the host as its storage, and its operator API on loopback; the host network
//! is what lets a plain-text loopback `--storage` be reached from the container, which a
//! storage node's mutual TLS would otherwise need certificates for (the other rehearsal has a
//! farm CA for that). Its configuration is [`Setup::config`], the file the helper reads, and
//! [`Setup::docker_run`] makes the first container from the same values.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, sleep};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{capture, farm, out_dir, run};

/// The container's name.
pub const NAME: &str = "kbf-frontend-rehearsal";
/// The operator API, as the helper reaches it (the host's loopback, on the host network). It is
/// the address `kbf cluster drain` dials when it is given none, which is how the helper runs it
/// in the container.
pub const API: &str = "127.0.0.1:8982";
/// Where the host's storage node listens: the `kbf` unit's `--storage-listen`.
pub const STORAGE: &str = "127.0.0.1:8983";
/// The account the frontend image runs as.
const USER: &str = "65532:65532";
/// How many seconds a drained frontend keeps serving with `/readyz` at 503 before it stops
/// taking new work: the window in which the stand-down can be seen.
const DRAIN_LB_WAIT_SECS: u32 = 5;
/// The operator token's path inside the container.
pub const TOKEN_IN_CONTAINER: &str = "/etc/kbf/operator.token";
/// The state directory inside the container, which the image's user may write.
const STATE_IN_CONTAINER: &str = "/var/lib/kbf-server";

/// What the container is made of, on the host.
pub struct Setup {
    /// Mounted at `/etc/kbf`, read-only: the operator token.
    pub etc: PathBuf,
    /// Mounted at the state directory: the SQLite database.
    pub state: PathBuf,
}

/// A TOML array of strings (each is a basic string; the values here have nothing to escape).
fn list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("{i:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

impl Setup {
    /// The directories and the operator token, owned as the image's user needs them (a token
    /// has no group or other permission bit).
    pub fn prepare(work: &Path) -> Self {
        let dir = work.join("frontend");
        let (etc, state) = (dir.join("etc"), dir.join("state"));
        for d in [&dir, &etc, &state] {
            fs::create_dir_all(d).unwrap();
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&etc, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o750)).unwrap();
        let token = etc.join("operator.token");
        fs::write(&token, format!("{}\n", farm::OPERATOR_TOKEN)).unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        for owned in [&token, &state] {
            run("chown_frontend", Command::new("chown").arg(USER).arg(owned));
        }
        Self { etc, state }
    }

    fn mounts(&self) -> Vec<String> {
        vec![
            format!("{}:/etc/kbf:ro", self.etc.display()),
            format!("{}:{STATE_IN_CONTAINER}", self.state.display()),
        ]
    }

    /// The flags after the image.
    fn command(&self) -> Vec<String> {
        [
            "--listen=127.0.0.1:18980".to_owned(),
            "--worker-listen=127.0.0.1:18981".to_owned(),
            format!("--api-listen={API}"),
            format!("--api-token-file={TOKEN_IN_CONTAINER}"),
            format!("--storage=http://{STORAGE}"),
            format!("--state-store=sqlite://{STATE_IN_CONTAINER}/state.db"),
            format!("--drain-lb-wait={DRAIN_LB_WAIT_SECS}"),
        ]
        .to_vec()
    }

    /// `/etc/kbf/host-install-frontend.toml`, naming `drain_token_file` (a path inside the
    /// container) for the drain. No `peers`: this host has no other frontend, and a file
    /// without them is a file that guards nothing against taking down the last ready one.
    pub fn config(&self, drain_token_file: &str) -> String {
        format!(
            "[frontend]\n\
             name = {NAME:?}\n\
             restart = \"unless-stopped\"\n\
             network = \"host\"\n\
             user = {USER:?}\n\
             mounts = {}\n\
             command = {}\n\
             api = {API:?}\n\
             drain_token_file = {drain_token_file:?}\n\
             drain_secs = 60\n\
             ready_secs = 90\n",
            list(&self.mounts()),
            list(&self.command()),
        )
    }

    /// The arguments of `docker` that make the first container from `image`: the options the
    /// helper makes every container with, from [`Setup::config`]'s values.
    pub fn docker_run(&self, image: &str) -> Vec<String> {
        let mut args: Vec<String> = [
            "run",
            "--detach",
            "--name",
            NAME,
            "--restart",
            "unless-stopped",
            "--network",
            "host",
            "--user",
            USER,
        ]
        .map(str::to_owned)
        .to_vec();
        for mount in self.mounts() {
            args.extend(["--volume".to_owned(), mount]);
        }
        args.push(image.to_owned());
        args.extend(self.command());
        args
    }
}

/// `docker` with `args`, its output kept as `name`.
pub fn docker(name: &str, args: &[&str]) -> String {
    run(name, Command::new("docker").args(args))
}

/// `docker container inspect` of the container, as JSON.
pub fn inspect(name: &str) -> Value {
    let text = docker(name, &["container", "inspect", NAME]);
    let all: Value = serde_json::from_str(&text).unwrap();
    all[0].clone()
}

/// The commit of the `kbf` the container runs, asked of that binary inside it.
pub fn running_commit(name: &str) -> String {
    let text = docker(name, &["exec", NAME, "/usr/bin/kbf", "--version", "--json"]);
    let facts: Value = serde_json::from_str(text.trim()).unwrap();
    facts["commit_full"].as_str().expect("a commit").to_owned()
}

/// A `GET` of the operator API: the status and the body, `None` while nothing answers.
pub fn get(path: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(API).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {API}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    Some((status, body.to_owned()))
}

/// The JSON object in a body (a chunked body has its sizes around it).
fn json(body: &str) -> Option<Value> {
    let (from, to) = (body.find('{')?, body.rfind('}')?);
    serde_json::from_str(&body[from..=to]).ok()
}

/// What `/readyz` answers, if anything does.
pub fn ready() -> Option<u16> {
    get("/readyz").map(|(status, _)| status)
}

/// The first 12 digits of the commit `/healthz` names.
pub fn health_commit() -> Option<String> {
    let (_, body) = get("/healthz")?;
    json(&body)?["commit"].as_str().map(str::to_owned)
}

/// Whether the frontend says a drain has begun.
fn draining() -> Option<bool> {
    let (_, body) = get("/v1/frontend/drain")?;
    json(&body)?["draining"].as_bool()
}

/// Waits (a bounded number of seconds, not a measured one) until `ready` holds.
pub fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..300 {
        if ready() {
            return;
        }
        sleep(Duration::from_secs(1));
    }
    panic!("{what}: not so after 300 attempts a second apart");
}

/// What the operator API said at one look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// `/healthz`'s commit (12 digits).
    pub commit: Option<String>,
    /// `/readyz`'s status.
    pub ready: Option<u16>,
    /// `draining` of `GET /v1/frontend/drain`.
    pub draining: Option<bool>,
}

/// The operator API watched from a thread, as the helper replaces the container: every change
/// in what it says, in order.
pub struct Watch {
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<Sample>>>,
    thread: Option<JoinHandle<()>>,
}

impl Watch {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::<Sample>::new()));
        let (flag, log) = (Arc::clone(&stop), Arc::clone(&seen));
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                let sample = Sample {
                    commit: health_commit(),
                    ready: ready(),
                    draining: draining(),
                };
                let mut log = log.lock().unwrap();
                if log.last() != Some(&sample) {
                    log.push(sample);
                }
                drop(log);
                sleep(Duration::from_millis(100));
            }
        });
        Self {
            stop,
            seen,
            thread: Some(thread),
        }
    }

    /// Waits until the watch has seen the frontend of `commit` (12 digits) answer, stops
    /// watching, and returns what was seen, in order.
    pub fn finish_after_seeing(mut self, commit: &str) -> Vec<Sample> {
        wait("the watch sees the new frontend", || {
            let seen = self.seen.lock().unwrap();
            seen.iter().any(|s| s.commit.as_deref() == Some(commit))
        });
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        let mut seen = self.seen.lock().unwrap();
        std::mem::take(&mut *seen)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Docker's record of what is done to the container, one line per event (`<action> <image>`),
/// from the start of the recording.
pub struct Events {
    child: Child,
    file: PathBuf,
}

impl Events {
    pub fn start() -> Self {
        fs::create_dir_all(out_dir()).unwrap();
        let file = out_dir().join("docker_events.txt");
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let child = Command::new("docker")
            .args([
                "events",
                "--filter",
                &format!("container={NAME}"),
                "--since",
                &since.to_string(),
                "--format",
                "{{.Action}} {{.Actor.Attributes.image}}",
            ])
            .stdout(Stdio::from(fs::File::create(&file).unwrap()))
            .stderr(Stdio::null())
            .spawn()
            .expect("docker events");
        Self { child, file }
    }

    /// Stops recording and returns the lines.
    pub fn finish(mut self) -> Vec<String> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        fs::read_to_string(&self.file)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// The container's log and Docker's list of containers, written when the rehearsal ends,
/// whichever way, and printed when it ends in a failure.
pub struct Logs;

impl Drop for Logs {
    fn drop(&mut self) {
        let failing = std::thread::panicking();
        for (name, args) in [
            ("docker_ps", vec!["ps", "--all", "--no-trunc"]),
            ("docker_logs", vec!["logs", NAME]),
        ] {
            if let Ok(out) = Command::new("docker").args(&args).output() {
                capture(name, &out);
                if failing {
                    eprintln!(
                        "---- {name}\n{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
        }
    }
}
