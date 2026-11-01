//! The farm the agent's rehearsal runs against: a throwaway farm CA and the certificates it
//! issues, the real frontend and the real coordinator as processes, and the calls the rehearsal
//! makes to them.

use std::fs;
use std::net::UdpSocket;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use kbf_ca::{Authority, NodeCert, Policy, PublicKey, SystemEntropy, UpdaterCert};
use kbf_types::{HostRole, Role};
use rcgen::{KeyPair, PublicKeyData};
use serde_json::Value;

use super::{answers, ids_of, out_dir, run};

/// The node id of the host.
pub const NODE: &str = "host-1";
/// The operator token of the frontend and the coordinator (32 or more visible characters).
pub const OPERATOR_TOKEN: &str = "rehearsal-operator-token-0123456789abcdef";
/// The frontend's listeners, all on loopback.
pub const FRONTEND_REAPI: &str = "127.0.0.1:18980";
pub const FRONTEND_WORKER: &str = "127.0.0.1:18981";
pub const FRONTEND_API: &str = "127.0.0.1:18982";
pub const FRONTEND_ENROLL: &str = "127.0.0.1:18984";
/// The port of the storage node, on the host's own address (an advertised address is never
/// loopback).
pub const STORAGE_PORT: u16 = 18983;
/// The coordinator's listeners: the updaters' (mutual TLS) and CI's.
pub const HEARTBEAT: &str = "127.0.0.1:18443";
pub const CONTROL: &str = "127.0.0.1:18444";
/// How long the rehearsal waits for one thing before it stops.
pub const PATIENCE: Duration = Duration::from_secs(300);

fn unix_now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(since.as_secs()).unwrap()
}

/// The address this host dials out on: the storage node advertises it, and the frontend
/// dials the node there.
pub fn primary_address() -> String {
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    // A documentation address: connecting a datagram socket sends nothing, it only picks the
    // address the route to it leaves from.
    socket.connect("192.0.2.1:9").unwrap();
    socket.local_addr().unwrap().ip().to_string()
}

fn pem_of(label: &str, der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

/// Writes `text` to `path` with `mode`, owned by `account` when one is given.
pub fn put(path: &Path, text: &str, mode: u32, account: Option<&str>) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    if let Some(account) = account {
        let (uid, gid) = ids_of(account);
        std::os::unix::fs::chown(path, Some(uid), Some(gid)).unwrap();
    }
}

/// A directory only `account` (or root) can enter.
pub fn private_dir(path: &Path, account: Option<&str>) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    if let Some(account) = account {
        let (uid, gid) = ids_of(account);
        std::os::unix::fs::chown(path, Some(uid), Some(gid)).unwrap();
    }
}

/// The farm CA, in the directory the frontend opens as its own (`--ca-dir`).
pub struct Pki {
    ca: Authority,
}

fn key_pair() -> (KeyPair, PublicKey) {
    let pair = KeyPair::generate().unwrap();
    let public = PublicKey::from_spki(&pair.subject_public_key_info()).unwrap();
    (pair, public)
}

impl Pki {
    /// A new CA in `dir`, which must not exist: `ca.key` and `ca.pem`, as the frontend's own
    /// development signer makes them.
    pub fn new(dir: &Path) -> Self {
        let ca = Authority::init_dir(dir, Policy::default(), unix_now(), &mut SystemEntropy)
            .expect("a CA directory");
        Self { ca }
    }

    pub fn ca_pem(&self) -> String {
        self.ca.bundle().to_pem()
    }

    /// Writes `<stem>.pem` and `<stem>.key` in `dir` for the certificate `issue` makes.
    fn write(
        dir: &Path,
        stem: &str,
        account: Option<&str>,
        issue: impl FnOnce(&PublicKey) -> Vec<u8>,
    ) -> (PathBuf, PathBuf) {
        let (pair, public) = key_pair();
        let der = issue(&public);
        let (cert, key) = (
            dir.join(format!("{stem}.pem")),
            dir.join(format!("{stem}.key")),
        );
        put(&cert, &pem_of("CERTIFICATE", &der), 0o644, account);
        put(&key, &pair.serialize_pem(), 0o600, account);
        (cert, key)
    }

    /// The frontend's certificate: the worker listener's, and the client certificate it
    /// dials the storage node with.
    pub fn frontend(&self, dir: &Path) -> (PathBuf, PathBuf) {
        Self::write(dir, "frontend", None, |key| {
            self.ca
                .issue_frontend(
                    &["localhost", "127.0.0.1"],
                    key,
                    unix_now(),
                    &mut SystemEntropy,
                )
                .unwrap()
                .der
        })
    }

    /// The coordinator's certificate (role `coordinator`).
    pub fn coordinator(&self, dir: &Path) -> (PathBuf, PathBuf) {
        Self::write(dir, "coordinator", None, |key| {
            self.ca
                .issue_coordinator(
                    &["localhost", "127.0.0.1"],
                    key,
                    unix_now(),
                    &mut SystemEntropy,
                )
                .unwrap()
                .der
        })
    }

    /// The storage node's certificate, naming the address it advertises, owned by `account`.
    pub fn storage(&self, dir: &Path, advertise: &str, account: &str) -> (PathBuf, PathBuf) {
        Self::write(dir, "storage", Some(account), |key| {
            let node_id = NODE.parse().unwrap();
            let cert = NodeCert {
                node_id: &node_id,
                role: Role::Storage,
                key,
                advertise: Some(advertise),
            };
            self.ca
                .issue_node(&cert, unix_now(), &mut SystemEntropy)
                .unwrap()
                .der
        })
    }

    /// The daemon half's certificate (role `daemon`) for the host `NODE`, owned by `account`.
    pub fn daemon(&self, dir: &Path, account: &str) -> (PathBuf, PathBuf) {
        Self::write(dir, "daemon", Some(account), |key| {
            let node_id = NODE.parse().unwrap();
            let cert = NodeCert {
                node_id: &node_id,
                role: Role::Daemon,
                key,
                advertise: None,
            };
            self.ca
                .issue_node(&cert, unix_now(), &mut SystemEntropy)
                .unwrap()
                .der
        })
    }

    /// The updater's certificate of the storage host `node`, as `<node>.pem` and `<node>.key`
    /// in `dir`, for a stand-in host that is no account's.
    pub fn storage_host(&self, dir: &Path, node: &str) -> (PathBuf, PathBuf) {
        Self::write(dir, node, None, |key| {
            let node_id = node.parse().unwrap();
            let cert = UpdaterCert {
                node_id: &node_id,
                host_role: &HostRole::STORAGE,
                key,
            };
            self.ca
                .issue_updater(&cert, unix_now(), &mut SystemEntropy)
                .unwrap()
                .der
        })
    }

    /// The updater's certificate for the host `NODE`, which runs storage, owned by `account`.
    pub fn updater(&self, dir: &Path, account: &str) -> (PathBuf, PathBuf) {
        Self::write(dir, "updater", Some(account), |key| {
            let node_id = NODE.parse().unwrap();
            let cert = UpdaterCert {
                node_id: &node_id,
                host_role: &HostRole::STORAGE,
                key,
            };
            self.ca
                .issue_updater(&cert, unix_now(), &mut SystemEntropy)
                .unwrap()
                .der
        })
    }
}

/// A process of the farm, killed when this drops, with its output in a file of the
/// rehearsal's output directory.
pub struct Proc {
    pub name: String,
    pub log: PathBuf,
    child: Child,
}

impl Proc {
    /// Starts `program` with `args` and `env`, its standard output and error in
    /// `<name>.log`.
    pub fn start(name: &str, program: &Path, args: &[String], env: &[(&str, &str)]) -> Self {
        fs::create_dir_all(out_dir()).unwrap();
        let log = out_dir().join(format!("{name}.log"));
        let file = fs::File::create(&log).unwrap();
        let child = Command::new(program)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap_or_else(|e| panic!("{name}: {} did not start: {e}", program.display()));
        Self {
            name: name.to_owned(),
            log,
            child,
        }
    }

    /// Waits until something accepts a connection at `addr`, and fails when the process ends
    /// first or [`PATIENCE`] passes.
    pub fn wait_for(&mut self, addr: &str) {
        let until = Instant::now() + PATIENCE;
        while !answers(addr) {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "{} ended ({status}) before it listened at {addr}",
                    self.name
                );
            }
            assert!(
                Instant::now() < until,
                "{} did not listen at {addr}",
                self.name
            );
            sleep(Duration::from_millis(250));
        }
    }

    /// Fails when the process has ended.
    pub fn alive(&mut self) {
        if let Some(status) = self.child.try_wait().unwrap() {
            panic!("{} ended ({status})", self.name);
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The real frontend of release N, on the farm CA, with the storage node at
/// `storage_address` as its first member. Its enrollment listener is on, which the operator
/// API's enrollment routes (the adoption of a hand-provisioned node) need.
pub fn start_frontend(
    kbf: &Path,
    dir: &Path,
    ca_dir: &Path,
    cert: &(PathBuf, PathBuf),
    ca_pem: &Path,
    storage_address: &str,
) -> Proc {
    let token = dir.join("api-token");
    put(&token, &format!("{OPERATOR_TOKEN}\n"), 0o600, None);
    let state = dir.join("state.db");
    let path = |p: &Path| p.display().to_string();
    let args: Vec<String> = [
        "--mode=frontend".to_owned(),
        format!("--listen={FRONTEND_REAPI}"),
        format!("--worker-listen={FRONTEND_WORKER}"),
        format!("--worker-tls-cert={}", path(&cert.0)),
        format!("--worker-tls-key={}", path(&cert.1)),
        format!("--worker-client-ca={}", path(ca_pem)),
        format!("--api-listen={FRONTEND_API}"),
        format!("--api-token-file={}", path(&token)),
        format!("--storage=https://{storage_address}"),
        format!("--storage-ca={}", path(ca_pem)),
        format!("--storage-client-cert={}", path(&cert.0)),
        format!("--storage-client-key={}", path(&cert.1)),
        format!("--state-store=sqlite://{}", path(&state)),
        format!("--enroll-listen={FRONTEND_ENROLL}"),
        format!("--ca-dir={}", path(ca_dir)),
        "--ca-dev-file-signer".to_owned(),
        "--frontend-name=localhost".to_owned(),
        "--enroll-allow-unenforced-deny".to_owned(),
        "--auto-promote".to_owned(),
        "--drain-lb-wait=0".to_owned(),
    ]
    .to_vec();
    let mut proc = Proc::start("frontend", kbf, &args, &[]);
    proc.wait_for(FRONTEND_API);
    proc
}

/// A call to the frontend's operator API or the coordinator's control listener, by `curl`:
/// the answer's body. `ca` is the PEM file the https address is checked against.
pub fn call(name: &str, method: &str, url: &str, ca: Option<&Path>, body: Option<&str>) -> String {
    let mut cmd = Command::new("curl");
    cmd.args(["--silent", "--show-error", "--fail-with-body", "-X", method]);
    cmd.args(["-H", &format!("Authorization: Bearer {OPERATOR_TOKEN}")]);
    if let Some(ca) = ca {
        cmd.arg("--cacert").arg(ca);
    }
    if let Some(body) = body {
        cmd.args(["-H", "Content-Type: application/json", "--data", body]);
    }
    run(name, cmd.arg(url))
}

/// Records the host as the frontend's node, a storage node and a daemon, with the storage
/// address it advertises: the node was provisioned by hand, with certificates the operator
/// issued, so it never enrolled.
pub fn adopt(advertise: &str) {
    let url = format!("http://{FRONTEND_API}/v1/enroll/nodes/{NODE}:adopt");
    let body = format!(r#"{{"roles":["storage","daemon"],"advertise":"{advertise}"}}"#);
    call("adopt", "POST", &url, None, Some(&body));
}

/// The frontend's storage members.
pub fn members() -> Value {
    let url = format!("http://{FRONTEND_API}/v1/members");
    let text = call("members", "GET", &url, None, None);
    serde_json::from_str(&text).unwrap()
}

/// The oidc allow-list of the coordinator, as the farm's operator writes it for the release
/// workflow's gamma job (`docs/deploy/kbf-updater.md`, "Turn on the gamma stage"), for the
/// issuer at `issuer` and the control address `audience`.
pub fn allow_list(issuer: &str, audience: &str) -> String {
    format!(
        "[remote_operator]\n\
         issuer = \"{issuer}\"\n\
         audience = \"{audience}\"\n\
         repository_id = \"{REPOSITORY_ID}\"\n\
         repository = \"{}\"\n\
         job_workflow_ref = [\"{}/.github/workflows/release.yml@refs/heads/main\"]\n\
         refs = [\"refs/heads/main\"]\n\
         environments = [\"release\"]\n\
         event_names = [\"workflow_run\"]\n",
        super::REPO,
        super::REPO,
    )
}

/// [`allow_list`] with the services the release workflow's token may set: `services` as the
/// allow-list's key writes them.
pub fn allow_list_services(issuer: &str, audience: &str, services: &[&str]) -> String {
    let names: Vec<String> = services.iter().map(|s| format!("\"{s}\"")).collect();
    format!(
        "{}services = [{}]\n",
        allow_list(issuer, audience),
        names.join(", ")
    )
}

/// The numeric id of the repository in the allow-list and the tokens.
pub const REPOSITORY_ID: &str = "4242";

/// The real coordinator, with the farm CA's certificate for itself and `issuer`'s keys
/// trusted through `trust` (the issuer's certificate, the one root `SSL_CERT_FILE` names).
pub fn start_coordinator(
    binary: &Path,
    dir: &Path,
    cert: &(PathBuf, PathBuf),
    ca_pem: &Path,
    oidc: &Path,
    trust: &Path,
) -> Proc {
    start_coordinator_with(binary, dir, cert, ca_pem, oidc, trust, &[])
}

/// [`start_coordinator`] with more flags: `extra` follows the ones every rehearsal gives.
pub fn start_coordinator_with(
    binary: &Path,
    dir: &Path,
    cert: &(PathBuf, PathBuf),
    ca_pem: &Path,
    oidc: &Path,
    trust: &Path,
    extra: &[String],
) -> Proc {
    let token = dir.join("operator-token");
    put(&token, OPERATOR_TOKEN, 0o600, None);
    let path = |p: &Path| p.display().to_string();
    let mut args = vec![
        format!("--heartbeat-listen={HEARTBEAT}"),
        format!("--control-listen={CONTROL}"),
        format!("--tls-cert={}", path(&cert.0)),
        format!("--tls-key={}", path(&cert.1)),
        format!("--client-ca={}", path(ca_pem)),
        format!("--state-file={}", path(&dir.join("desired.json"))),
        format!("--oidc-config={}", path(oidc)),
        format!("--operator-token-file={}", path(&token)),
    ];
    args.extend(extra.iter().cloned());
    let trust = path(trust);
    let mut proc = Proc::start("coordinator", binary, &args, &[("SSL_CERT_FILE", &trust)]);
    proc.wait_for(CONTROL);
    proc
}

/// The coordinator's status, as the operator reads it.
pub fn status(ca: &Path) -> Value {
    let url = format!(
        "https://localhost:{}/v1/status",
        CONTROL.rsplit(':').next().unwrap()
    );
    let text = call("status", "GET", &url, Some(ca), None);
    serde_json::from_str(&text).unwrap()
}

/// The status of the host `NODE`, if the coordinator has heard from it.
pub fn host_status(ca: &Path) -> Option<Value> {
    status(ca)["hosts"]
        .as_array()
        .and_then(|hosts| hosts.iter().find(|h| h["node_id"] == NODE).cloned())
}

/// What the frontend tells the host's updater of the host's storage, on the worker listener
/// with the updater's certificate alone and no token: the answer the agent judges the host's
/// health by.
pub fn storage_health(ca: &Path, cert: &Path, key: &Path) -> Value {
    let worker = FRONTEND_WORKER.rsplit(':').next().unwrap();
    let url = format!("https://localhost:{worker}/v1/nodes/{NODE}/storage-health");
    let text = run(
        "storage_health",
        Command::new("curl")
            .args(["--silent", "--show-error", "--fail-with-body"])
            .arg("--cacert")
            .arg(ca)
            .arg("--cert")
            .arg(cert)
            .arg("--key")
            .arg(key)
            .arg(url),
    );
    serde_json::from_str(&text).unwrap()
}

/// The frontend's operator API view of the host's node (`GET /v1/nodes/{id}`): `None` while the
/// frontend does not know it as a daemon (404) or does not answer.
pub fn node() -> Option<Value> {
    let out = Command::new("curl")
        .args(["--silent", "--max-time", "5"])
        .args(["-H", &format!("Authorization: Bearer {OPERATOR_TOKEN}")])
        .arg(format!("http://{FRONTEND_API}/v1/nodes/{NODE}"))
        .output()
        .ok()?;
    let node: Value = serde_json::from_slice(&out.stdout).ok()?;
    (node["node_id"] == NODE).then_some(node)
}

/// What the frontend said of the node at one look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// `placement.state`: `serving`, `cordoned`, `draining`, `drained` or `drain_paused`.
    pub placement: String,
    /// Whether the daemon's newest stream is open.
    pub connected: bool,
    /// The daemon's version as it last reported it: `<semver>+<12 hex digits of the commit>`.
    pub version: String,
}

impl Seen {
    fn of(node: &Value) -> Self {
        Self {
            placement: node["placement"]["state"].as_str().unwrap_or("").to_owned(),
            connected: node["connected"] == true,
            version: node["software"]["daemon_version"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
        }
    }

    /// Whether placement may offer the node leases.
    pub fn serving(&self) -> bool {
        self.placement == "serving"
    }

    /// Whether the daemon runs the build of `commit`.
    pub fn runs(&self, commit: &str) -> bool {
        self.version.ends_with(&format!("+{}", &commit[..12]))
    }
}

/// The node as the frontend shows it now.
pub fn seen() -> Option<Seen> {
    node().map(|n| Seen::of(&n))
}

/// What the frontend's log records of the node `NODE`, one entry per line that names it.
///
/// The frontend writes these lines itself as it acts, in the order it acts: `operator action`
/// when a node is cordoned, drained or given back (on the operator API and on the worker
/// listener the agent uses alike; a drain the frontend answers "no such node" writes none),
/// `worker registered` when a daemon's stream registers the node, and `worker stream ended`
/// when a daemon's stream ends (its connection closes as its process stops). The `operator action`
/// line is the legacy scheduler core's, the default; the js core does not write it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeEvent {
    /// A daemon registered as the node.
    Registered,
    /// A daemon's stream ended.
    StreamEnded,
    /// The node was cordoned without a drain.
    Cordon,
    /// The node was drained.
    Drain,
    /// The node was given back to placement.
    Uncordon,
}

/// The events of `NODE` in `log`, the frontend's standard output and error, in order. The
/// colour codes of a terminal log are dropped first.
pub fn node_events(log: &str) -> Vec<NodeEvent> {
    let node = format!("worker={NODE}");
    log.lines()
        .map(without_colour)
        .filter(|line| line.split_whitespace().any(|w| w == node))
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            if line.contains("worker registered") {
                Some(NodeEvent::Registered)
            } else if line.contains("worker stream ended") {
                Some(NodeEvent::StreamEnded)
            } else if !line.contains("operator action") {
                None
            } else if words.iter().any(|w| w.starts_with("action=Drain(")) {
                Some(NodeEvent::Drain)
            } else if words.contains(&"action=Uncordon") {
                Some(NodeEvent::Uncordon)
            } else if words.contains(&"action=Cordon") {
                Some(NodeEvent::Cordon)
            } else {
                None
            }
        })
        .collect()
}

/// `line` without its ANSI escape sequences (`ESC [ ... <letter>`).
fn without_colour(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The frontend's own record of the node from [`Watch::start`] on: the part of its log written
/// since then, read when the watch stops. Nothing is sampled, so no event is missed however
/// briefly the node was in a state.
pub struct Watch {
    log: PathBuf,
    from: usize,
}

impl Watch {
    /// Starts at the end of `log` as it is now.
    pub fn start(log: &Path) -> Self {
        let from = fs::read(log).unwrap().len();
        Self {
            log: log.to_owned(),
            from,
        }
    }

    /// The node's events the log gained since [`Watch::start`], in order.
    pub fn stop(self) -> Vec<NodeEvent> {
        let log = fs::read(&self.log).unwrap();
        node_events(&String::from_utf8_lossy(&log[self.from..]))
    }
}
