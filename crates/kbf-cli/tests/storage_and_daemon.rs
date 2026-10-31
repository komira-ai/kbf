//! `kbf storage-and-daemon` end to end: the node runs one action whose input it reads
//! from its own storage in-process, and stores the action's output there.
//!
//! Until the frontend exists (av2-07), the scheduler is a `kbf-server` started in this
//! test's process, the node's daemon half dialling its worker listener as any node's
//! does. That server checks an action's inputs and outputs in its own cache before it
//! schedules or accepts it, so the test stores them there too, and then damages the
//! server's copy of the input: a daemon that read its input anywhere but its own storage
//! would read bytes that fail their digest, and the action would fail.

#![cfg(target_os = "linux")]

#[path = "support/node.rs"]
mod node;

use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Parser as _;
use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, ActionResult, Command as ReapiCommand, Digest, Directory, DirectoryNode,
    ExecuteRequest, ExecuteResponse, FileNode, Platform,
};
use kbf_testkit::dir::TestDir;
use kbf_transfer::{Cas as _, TransferConfig, Transfers, digest_of};
use node::{
    PROMPT, Running, cgroup_tree, crun, files_holding, id_files, image_path, input_bytes,
    oci_runtime, pki,
};
use prost::Message as _;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

// The stand-in `crun` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// The stand-in `crun` (`node::crun`).
const FAKE_CRUN: &str = env!("CARGO_BIN_EXE_kbf-cli-fake-crun");

/// The bytes the action writes to its one output file.
const OUTPUT: &[u8] = b"a file\n";

/// A fresh directory for one test, removed when it drops (declare it before the
/// processes that write into it).
fn scratch(name: &str) -> TestDir {
    TestDir::under(node::scratch_root().join("kbf-storage-and-daemon"), name)
}

/// The scheduler: `kbf-server` in this process over its own stores in `dir`, its worker
/// listener under mutual TLS; its REAPI and worker addresses. It serves for as long as
/// `tokio` runs.
fn scheduler(tokio: &tokio::runtime::Runtime, dir: &Path) -> (SocketAddr, SocketAddr) {
    let at = |name: &str| dir.join(name).display().to_string();
    let args = kbf_server::Args::try_parse_from([
        "kbf-server".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        // The one node is this host, whatever its architecture: the default
        // (`linux/x86_64`) would leave an action on an aarch64 host with no worker.
        "--default-platform=none".to_owned(),
        format!("--worker-tls-cert={}", at("server.pem")),
        format!("--worker-tls-key={}", at("server.key")),
        format!("--worker-client-ca={}", at("ca.pem")),
        format!("--cold-store=file://{}", at("server-cold")),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--meta-dir={}", at("server-meta")),
    ])
    .expect("the server's flags");
    let never = || Ok(std::future::pending::<()>());
    let server = tokio
        .block_on(kbf_server::start(&args, never))
        .expect("the server starts");
    let addrs = (server.reapi, server.worker);
    tokio.spawn(server.serve());
    addrs
}

/// The storage listener's mutual-TLS flags, as the deployed unit gives them.
const STORAGE_TLS: [&str; 3] = [
    "--storage-tls-cert",
    "--storage-tls-key",
    "--storage-client-ca",
];

/// The node's flags: the storage half's stores in `dir` (a hot store on the data
/// directory's filesystem, so the two halves keep one cache) and a listener on loopback
/// under mutual TLS, as the deployed unit's (so a daemon half that reached its storage
/// over that listener, with no certificate, would read nothing); the daemon half's
/// container driver dialling the scheduler's `worker` listener.
fn node_flags(dir: &Path, worker: SocketAddr) -> Vec<String> {
    let at = |name: &str| dir.join(name).display().to_string();
    vec![
        "storage-and-daemon".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("{}={}", STORAGE_TLS[0], at("server.pem")),
        format!("{}={}", STORAGE_TLS[1], at("server.key")),
        format!("{}={}", STORAGE_TLS[2], at("ca.pem")),
        format!("--cold-store=file://{}", at("cold")),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--hot-store=file://{}", at("hot")),
        // Every blob its own block, flushed at once.
        "--write-own-segment-bytes=1".to_owned(),
        "--driver=container".to_owned(),
        format!("--server=https://{worker}"),
        "--tls-server-name=localhost".to_owned(),
        format!("--ca-cert={}", at("ca.pem")),
        format!("--cert={}", at("node.pem")),
        format!("--key={}", at("node.key")),
        "--node-id=node-1".to_owned(),
        "--reconnect-ms=50".to_owned(),
        format!("--data-dir={}", at("data")),
        format!("--id-files={}", id_files(dir).display()),
        // Not the host's own /etc/kbf/kbf.toml.
        kbf_testkit::config::flag(dir),
    ]
}

/// Starts `kbf storage-and-daemon` with `flags`, its container driver running the
/// stand-in `crun` in `bin`; the running node and its storage listener's address,
/// from the start line.
fn start_node(bin: &Path, flags: &[String]) -> (Running, SocketAddr) {
    let mut child = Command::new(BIN)
        .args(flags)
        .args(cgroup_tree(bin))
        .arg(oci_runtime(bin))
        .env("PATH", image_path(bin))
        .current_dir(bin)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let running = Running(child);
    let line = stdout.lines().next().expect("a start line").expect("read");
    assert!(line.starts_with("kbf-storage "), "{line}");
    let storage = line
        .split(' ')
        .find_map(|field| field.strip_prefix("storage="))
        .unwrap_or_else(|| panic!("no storage= in {line:?}"))
        .parse()
        .expect("an address");
    (running, storage)
}

/// A client of the plain-text gRPC listener at `addr`.
async fn connect(addr: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect")
}

/// A client of the node's storage listener at `addr`, under mutual TLS with the
/// storage client's certificate in `dir` (`node::pki`).
async fn connect_tls(dir: &Path, addr: SocketAddr) -> Channel {
    let read = |name: &str| std::fs::read(dir.join(name)).expect("read");
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(read("ca.pem")))
        .domain_name("localhost")
        .identity(Identity::from_pem(read("client.pem"), read("client.key")));
    Endpoint::from_shared(format!("https://{addr}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect()
        .await
        .expect("connect")
}

/// Stores `bytes` in each of `stores`; their digest.
async fn put_all(stores: &[&Transfers], bytes: &[u8]) -> Digest {
    let mut digest = None;
    for cas in stores {
        digest = Some(cas.put(bytes.to_vec()).await.expect("upload"));
    }
    digest.expect("a store")
}

/// The action that reads `input` as `in/input.bin` and writes [`OUTPUT`], stored in
/// each of `stores`; its digest.
async fn action(stores: &[&Transfers], image: &str, input: &[u8]) -> Digest {
    let file = FileNode {
        name: "input.bin".to_owned(),
        digest: Some(put_all(stores, input).await),
        ..FileNode::default()
    };
    let inner = Directory {
        files: vec![file],
        ..Directory::default()
    };
    let root = Directory {
        directories: vec![DirectoryNode {
            name: "in".to_owned(),
            digest: Some(put_all(stores, &inner.encode_to_vec()).await),
        }],
        ..Directory::default()
    };
    let command = ReapiCommand {
        arguments: vec!["true".to_owned()],
        output_paths: vec!["out/file.txt".to_owned()],
        ..ReapiCommand::default()
    };
    let action = Action {
        command_digest: Some(put_all(stores, &command.encode_to_vec()).await),
        input_root_digest: Some(put_all(stores, &root.encode_to_vec()).await),
        platform: Some(Platform {
            properties: vec![Property {
                name: "container-image".to_owned(),
                value: image.to_owned(),
            }],
        }),
        ..Action::default()
    };
    put_all(stores, &action.encode_to_vec()).await
}

/// Executes `action` until its operation is done; its result.
async fn execute(reapi: Channel, action: Digest) -> ActionResult {
    let mut ops = ExecutionClient::new(reapi)
        .execute(ExecuteRequest {
            action_digest: Some(action),
            skip_cache_lookup: true,
            ..ExecuteRequest::default()
        })
        .await
        .expect("Execute")
        .into_inner();
    let done = loop {
        let op = ops.message().await.expect("stream").expect("an update");
        if op.done {
            break op;
        }
    };
    let Some(operation::Result::Response(any)) = done.result else {
        panic!("no response: {done:?}");
    };
    let response = ExecuteResponse::decode(any.value.as_slice()).expect("response");
    response.result.expect("a result")
}

/// Changes one byte of every file under `dir` that holds `bytes`; how many it changed.
fn damage(dir: &Path, bytes: &[u8]) -> usize {
    let held = files_holding(&[dir.to_path_buf()], bytes);
    for (path, _) in &held {
        let mut file = std::fs::read(path).expect("read");
        let at = file
            .windows(64)
            .position(|run| *run == bytes[..64])
            .expect("the bytes");
        file[at] ^= 1;
        std::fs::write(path, file).expect("write");
    }
    held.len()
}

/// Waits until `input` is on the node's disk once: one inode under the hot store and
/// the data directory, the input cache's entry (the hot store's staged copy goes once
/// it is flushed, and the one cache keeps no flushed copy).
fn on_disk_once(dir: &Path, input: &[u8]) {
    let digest = digest_of(input);
    let deadline = Instant::now() + PROMPT;
    let copies = loop {
        let copies = files_holding(&[dir.join("hot"), dir.join("data")], input);
        if copies.len() == 1 || Instant::now() > deadline {
            break copies;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let [(path, size)] = &copies[..] else {
        panic!(
            "the input is on the disk {} times: {copies:?}",
            copies.len()
        );
    };
    assert!(path.starts_with(dir.join("data/cache")), "{path:?}");
    let name = format!("{}-{}", digest.hash, digest.size_bytes);
    assert!(path.ends_with(&name), "{path:?}");
    assert_eq!(*size, input.len() as u64);
}

/// Catches `kbf storage-and-daemon` not built (it exits 2 at once), a daemon half that
/// reads its inputs anywhere but its own storage in-process (over the network from the
/// scheduler, whose copy of the input is damaged here, or through its own storage
/// listener, which takes only certified callers: the action would fail), outputs that do not
/// reach the node's storage, the halves keeping two copies of an input on one disk (the
/// one cache given to neither; that both are given the same one is pinned by
/// the unit tests of `prepared`), a fill that leaves a file in the input cache's `tmp/`,
/// and SIGTERM not stopping both halves with exit 0. One action reads an input of more
/// than three chunks and writes one output file.
#[test]
fn storage_and_daemon_runs_an_action_over_its_own_storage() {
    let dir = scratch("e2e");
    pki(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let image = crun(
        &bin,
        &dir.join("data"),
        FAKE_CRUN,
        &["write-lines", "$UPPER/out/file.txt", "a file"],
    );
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let (reapi, worker) = scheduler(&tokio, &dir);
    let (running, storage) = start_node(&bin, &node_flags(&dir, worker));

    let input = input_bytes();
    let output = tokio.block_on(async {
        let reapi = connect(reapi).await;
        let server = Transfers::over(reapi.clone(), TransferConfig::default());
        let node = Transfers::over(connect_tls(&dir, storage).await, TransferConfig::default());
        let action = action(&[&server, &node], &image, &input).await;
        // The scheduler accepts a result only once its outputs are in its cache.
        let output = put_all(&[&server], OUTPUT).await;
        assert!(
            damage(&dir.join("server-cold"), &input) > 0,
            "no server copy"
        );
        let result = tokio::time::timeout(PROMPT, execute(reapi, action))
            .await
            .expect("the action ran in time");
        assert_eq!(result.exit_code, 0, "{result:?}");
        let [file] = &result.output_files[..] else {
            panic!("one output file: {result:?}");
        };
        assert_eq!(
            (file.path.as_str(), file.digest.as_ref()),
            ("out/file.txt", Some(&output))
        );
        assert!(
            node.missing(std::slice::from_ref(&output)).await.is_empty(),
            "not in the node's storage"
        );
        output
    });
    assert_eq!(output, digest_of(OUTPUT));
    on_disk_once(&dir, &input);
    let tmp = std::fs::read_dir(dir.join("data/cache/v1/tmp")).expect("tmp/");
    assert_eq!(tmp.count(), 0, "a fill left a file in tmp/");

    let pid = rustix::process::Pid::from_child(&running.0);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
    assert!(running.wait().success());
}

/// `kbf storage-and-daemon` with `flags`, `env` added to its environment; its exit
/// status, stdout and stderr.
fn refused(flags: &[String], env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let out = Command::new(BIN)
        .args(flags)
        .envs(env.iter().copied())
        .output()
        .expect("run");
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    (out.status.code(), text(out.stdout), text(out.stderr))
}

/// `flags` with every flag named in `extra` given as `extra` gives it instead.
fn replaced(flags: Vec<String>, extra: &[String]) -> Vec<String> {
    let name = |flag: &str| flag.split('=').next().unwrap_or(flag).to_owned();
    let named: Vec<String> = extra.iter().map(|flag| name(flag)).collect();
    let kept = flags
        .into_iter()
        .filter(|flag| !named.contains(&name(flag)));
    kept.chain(extra.iter().cloned()).collect()
}

/// Catches a start either half refuses that exits 0 or as the other half would, or that
/// does not say which half refused: a storage half whose listener `kbf storage` refuses
/// (plain text off loopback) exits 2 before the daemon half starts; with an `s3://` cold
/// store (no connection is made until a blob moves) the storage half starts and prints
/// its start line, and a daemon half whose driver cannot start (no subordinate ids for
/// its user) then exits 2, as `kbf-daemon` does.
#[test]
fn a_start_either_half_refuses_says_which() {
    let dir = scratch("refused");
    pki(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    crun(
        &bin,
        &dir.join("data"),
        FAKE_CRUN,
        &["write-lines", "$UPPER/out/file.txt", "a file"],
    );
    let path = image_path(&bin);
    let host = [("PATH", path.as_str())];
    // The rendered unit names `/usr/bin/crun`: this box's is the stand-in.
    let runtime = [oci_runtime(&bin)];
    let worker: SocketAddr = "127.0.0.1:1".parse().expect("address");
    let wide = ["--storage-listen=0.0.0.0:0".to_owned()];
    let plain: Vec<String> = node_flags(&dir, worker)
        .into_iter()
        .filter(|flag| !STORAGE_TLS.iter().any(|tls| flag.starts_with(tls)))
        .collect();
    let plain: Vec<String> = plain.into_iter().chain(runtime.clone()).collect();
    let (code, out, err) = refused(&replaced(plain, &wide), &host);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("kbf storage-and-daemon: storage: "), "{err}");
    assert!(out.is_empty(), "{out}");

    let s3 = [
        "--cold-store=s3://bucket/kbf/?endpoint=http://127.0.0.1:1".to_owned(),
        format!("--meta-dir={}", dir.join("s3-meta").display()),
        "--id-files=/nonexistent/ids".to_owned(),
    ];
    let flags: Vec<String> = replaced(node_flags(&dir, worker), &s3)
        .into_iter()
        .chain(runtime.clone())
        .filter(|flag| !flag.starts_with("--hot-store="))
        .collect();
    let env = [
        host[0],
        ("AWS_ACCESS_KEY_ID", "test-key"),
        ("AWS_SECRET_ACCESS_KEY", "test-secret"),
    ];
    let (code, out, err) = refused(&flags, &env);
    // 2, not 1: a user without subordinate ids is a refusal a restart would not change
    // (the unit's RestartPreventExitStatus=2 leaves it down).
    assert_eq!(code, Some(2), "{err}");
    assert!(out.starts_with("kbf-storage "), "{out}");
    assert!(err.contains("kbf storage-and-daemon: daemon: "), "{err}");

    // A cold store that cannot be made is the storage half's refusal, as `kbf storage`
    // gives it: 2, before the daemon half starts.
    let cold = ["--cold-store=file:///proc/nonexistent/cold".to_owned()];
    let flags: Vec<String> = replaced(node_flags(&dir, worker), &cold)
        .into_iter()
        .chain(runtime)
        .collect();
    let (code, out, err) = refused(&flags, &host);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("kbf storage-and-daemon: storage: "), "{err}");
    assert!(out.is_empty(), "{out}");
}

/// Catches a driver other than the container driver accepted (the daemon half is the
/// container driver's only): the native driver is a usage error, exit 2, said so, and no
/// start line.
#[test]
fn a_driver_other_than_container_is_a_usage_error() {
    let dir = scratch("native");
    let worker: SocketAddr = "127.0.0.1:1".parse().expect("address");
    let native = ["--driver=native".to_owned()];
    let flags = replaced(node_flags(&dir, worker), &native);
    let (code, out, err) = refused(&flags, &[]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("container driver only"), "{err}");
    assert!(out.is_empty(), "{out}");
}

/// Catches the daemon's helper re-execs dispatched after the flags are read, or not at
/// all: run with keeper arguments that name no mode, as a driver never runs it, the
/// binary answers as the keeper does (its usage line, exit 2), not with clap's error
/// about the flags it lacks, and starts no node (no start line on stdout).
#[test]
fn the_binary_answers_as_a_keeper_before_it_reads_its_flags() {
    let out = Command::new(BIN)
        .args(["__keeper", "groups"])
        .output()
        .expect("spawn");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.starts_with("kbf-keeper: usage: __keeper group"),
        "{stderr}"
    );
    assert!(out.stdout.is_empty(), "a node started");
}

/// Catches a SIGTERM that arrives while the node is still starting killing the process by
/// the signal's default action (kbf#1087): the node is told to register with a frontend
/// that never answers, so its start waits in the registration, with the storage half's
/// stores open. A `systemctl stop` then must stop it cleanly and exit 0, not end it by
/// SIGTERM (no exit code), which hard-kills a storage member other nodes replicate to.
/// Waits for the start's own log line (`cold store:`), logged after the process is
/// running its start, so the signal is sent while it waits and not before it runs.
#[test]
fn a_sigterm_while_the_node_is_still_starting_stops_it_cleanly() {
    use std::io::Read as _;

    let dir = scratch("sigterm-at-start");
    pki(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    crun(
        &bin,
        &dir.join("data"),
        FAKE_CRUN,
        &["write-lines", "$UPPER/out/file.txt", "a file"],
    );
    let worker: SocketAddr = "127.0.0.1:1".parse().expect("address");
    // Port 1 refuses, and the registration retries until the frontend answers.
    let flags: Vec<String> = node_flags(&dir, worker)
        .into_iter()
        .chain(["--frontend=http://127.0.0.1:1".to_owned()])
        .collect();
    let mut child = Command::new(BIN)
        .args(&flags)
        .args(cgroup_tree(&bin))
        .arg(oci_runtime(&bin))
        .env("PATH", image_path(&bin))
        .current_dir(&bin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stderr = child.stderr.take().expect("stderr");
    let running = Running(child);
    let mut seen = Vec::new();
    let mut byte = [0_u8; 1];
    let deadline = Instant::now() + PROMPT;
    while !String::from_utf8_lossy(&seen).contains("cold store:") {
        assert!(Instant::now() < deadline, "no start log line");
        let got = stderr.read(&mut byte).expect("read");
        assert_eq!(got, 1, "the node ended: {}", String::from_utf8_lossy(&seen));
        seen.push(byte[0]);
    }
    // The start is waiting in the registration: it must be still running, no start line.
    std::thread::sleep(Duration::from_millis(500));
    let pid = rustix::process::Pid::from_child(&running.0);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
    let status = running.wait();
    assert_eq!(status.code(), Some(0), "{status:?}");
}
