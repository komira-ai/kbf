//! A `kbf frontend` process in front of one `kbf storage-and-daemon` process, end to end:
//! one action, executed through the frontend's REAPI listener, run by the node's daemon
//! half (its container driver over a stand-in `crun`, `kbf-cli-fake-crun`, which runs the
//! action as a plain process, and a cgroup tree of plain directories), its outputs, stdout
//! and stderr stored in the node's storage in-process and read back through the frontend;
//! and the node's one cache across a crash.
//!
//! The node has no `--frontend`, so it is the whole farm's storage, a map of itself: the
//! frontend reaches it through `--storage`, and the node's daemon half dials the frontend's
//! worker listener under mutual TLS. Both are children of the test.

#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, ActionResult, Command as ReapiCommand, Digest, Directory, ExecuteRequest,
    ExecuteResponse, Platform, Tree,
};
use kbf_testkit::dir::TestDir;
use kbf_transfer::{Cas as _, TransferConfig, Transfers, fetch};
use prost::Message as _;
use tonic::transport::{Channel, Endpoint};

#[path = "support/node.rs"]
mod node;

use node::{
    PROMPT, Running, cgroup_tree, crun, files_holding, id_files, image_path, input_bytes,
    oci_runtime, pki, worker_addr,
};

// The stand-in `crun` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

/// The `kbf` binary.
const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// The stand-in `crun` (`node::crun`).
const FAKE_CRUN: &str = env!("CARGO_BIN_EXE_kbf-cli-fake-crun");

/// A fresh directory for one test, removed when it drops (declare it before the
/// processes that write into it).
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        node::scratch_root().join("kbf-storage-and-daemon-behind-frontend"),
        name,
    )
}

/// The addresses of one farm, each a loopback address of its own (`node::worker_addr`),
/// so that a node that starts again binds the same ones.
struct Addrs {
    /// The node's storage listener, plain text on loopback.
    storage: SocketAddr,
    /// The frontend's worker listener, under mutual TLS.
    worker: SocketAddr,
    /// The node's `GET /metrics`.
    metrics: SocketAddr,
}

impl Addrs {
    fn new() -> Self {
        Self {
            storage: worker_addr(),
            worker: worker_addr(),
            metrics: worker_addr(),
        }
    }
}

/// The flags of `kbf storage-and-daemon` in `dir`: a cold store and a hot store (the write
/// tier, holding the metadata log), the daemon half's container driver dialling the
/// frontend's worker listener, `extra` after them.
fn node_flags(dir: &Path, addrs: &Addrs, extra: &[String]) -> Vec<String> {
    let at = |name: &str| dir.join(name).display().to_string();
    let mut flags = vec![
        "storage-and-daemon".to_owned(),
        format!("--storage-listen={}", addrs.storage),
        format!("--metrics-listen={}", addrs.metrics),
        format!("--cold-store=file://{}", at("cold")),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--hot-store=file://{}", at("hot")),
        "--driver=container".to_owned(),
        format!("--server=https://{}", addrs.worker),
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
    ];
    flags.extend_from_slice(extra);
    flags
}

/// Starts `kbf storage-and-daemon` with `flags`, its container driver running the
/// stand-in `crun` in `bin`; waits for its start line.
fn start_node(bin: &Path, flags: &[String]) -> Running {
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
    running
}

/// Starts `kbf frontend` in `dir` in front of the node at `addrs.storage`, its REAPI
/// listener plain text on a free loopback port, its worker listener on `addrs.worker`
/// under mutual TLS; the frontend and its REAPI address, from its start line.
fn start_frontend(dir: &Path, addrs: &Addrs) -> (Running, SocketAddr) {
    let at = |name: &str| dir.join(name).display().to_string();
    let mut child = Command::new(BIN)
        .args([
            "frontend".to_owned(),
            "--listen=127.0.0.1:0".to_owned(),
            // The node is this host, whatever its architecture: the default
            // (`linux/x86_64`) would leave an action on an aarch64 host with no worker.
            "--default-platform=none".to_owned(),
            format!("--worker-listen={}", addrs.worker),
            format!("--worker-tls-cert={}", at("frontend.pem")),
            format!("--worker-tls-key={}", at("frontend.key")),
            format!("--worker-client-ca={}", at("ca.pem")),
            format!("--storage=http://{}", addrs.storage),
            // The default is Postgres on the host's socket; these tests need no server.
            format!("--state-store=sqlite://{}", at("state.db")),
            kbf_testkit::config::flag(dir),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let running = Running(child);
    let line = stdout.lines().next().expect("a start line").expect("read");
    assert!(line.starts_with("kbf-frontend "), "{line}");
    let reapi = line
        .split(' ')
        .find_map(|field| field.strip_prefix("reapi="))
        .unwrap_or_else(|| panic!("no reapi= in {line:?}"))
        .parse()
        .expect("an address");
    (running, reapi)
}

/// A client of the plain-text gRPC listener at `addr`.
async fn connect(addr: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect")
}

/// Stores `bytes` through `cas`.
async fn put(cas: &Transfers, bytes: Vec<u8>) -> Digest {
    cas.put(bytes).await.expect("upload")
}

/// Executes `action` until its operation is done; returns its result.
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

/// Stops `running` as an operator does, with SIGTERM; its exit status.
fn terminate(running: Running) -> std::process::ExitStatus {
    let pid = rustix::process::Pid::from_child(&running.0);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
    running.wait()
}

/// Catches an action's outputs not reaching the node's storage, or not readable through
/// the frontend, and SIGTERM not stopping both halves of the node with exit 0. One action, run by the
/// container driver over the stand-in `crun`, writes a file, a directory holding a file,
/// stdout and stderr; each is read back through the frontend and is what the action wrote,
/// and the stdout is in the node's own storage, read over its listener.
#[test]
fn an_actions_outputs_are_stored_by_the_node_and_read_back_through_the_frontend() {
    let dir = scratch("e2e");
    pki(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let steps = [
        "mkdir",
        "$UPPER/out/dir",
        ";",
        "write-lines",
        "$UPPER/out/file.txt",
        "a file",
        ";",
        "write-lines",
        "$UPPER/out/dir/nested.txt",
        "nested",
        ";",
        "echo",
        "to stdout",
        ";",
        "echo",
        "--stderr",
        "to stderr",
    ];
    let image = crun(&bin, &dir.join("data"), FAKE_CRUN, &steps);
    let addrs = Addrs::new();
    let node = start_node(&bin, &node_flags(&dir, &addrs, &[]));
    let (_frontend, reapi) = start_frontend(&dir, &addrs);

    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let channel = connect(reapi).await;
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let command = ReapiCommand {
            arguments: vec!["true".to_owned()],
            output_paths: vec!["out/file.txt".to_owned(), "out/dir".to_owned()],
            ..ReapiCommand::default()
        };
        let action = Action {
            command_digest: Some(put(&cas, command.encode_to_vec()).await),
            input_root_digest: Some(put(&cas, Directory::default().encode_to_vec()).await),
            platform: Some(Platform {
                properties: vec![Property {
                    name: "container-image".to_owned(),
                    value: image,
                }],
            }),
            ..Action::default()
        };
        let action = put(&cas, action.encode_to_vec()).await;
        let result = tokio::time::timeout(PROMPT, execute(channel, action))
            .await
            .expect("the action ran in time");
        let read = |d: Option<Digest>| {
            let cas = &cas;
            async move { fetch(cas, &d.expect("a digest")).await.expect("read back") }
        };
        assert_eq!(result.exit_code, 0, "{result:?}");
        assert_eq!(read(result.stdout_digest.clone()).await, b"to stdout\n");
        assert_eq!(read(result.stderr_digest.clone()).await, b"to stderr\n");
        let [file] = &result.output_files[..] else {
            panic!("one output file: {result:?}");
        };
        assert_eq!(file.path, "out/file.txt");
        assert_eq!(read(file.digest.clone()).await, b"a file\n");
        let [tree] = &result.output_directories[..] else {
            panic!("one output directory: {result:?}");
        };
        assert_eq!(tree.path, "out/dir");
        let tree = Tree::decode(read(tree.tree_digest.clone()).await.as_slice()).expect("a Tree");
        let root = tree.root.expect("a root");
        assert_eq!(root.files.len(), 1, "{root:?}");
        assert_eq!(root.files[0].name, "nested.txt");
        assert_eq!(read(root.files[0].digest.clone()).await, b"nested\n");

        // The node's own storage holds them, asked over its listener and not the frontend's.
        let storage = Transfers::over(connect(addrs.storage).await, TransferConfig::default());
        let stdout = result.stdout_digest.expect("a stdout digest");
        let stored = [stdout, file.digest.clone().expect("a digest")];
        assert!(storage.missing(&stored).await.is_empty(), "not in the node");
    });
    // Its connections closed, so the frontend's drain does not wait for them.
    drop(tokio);

    let status = terminate(node);
    assert!(status.success(), "{status}");
}

/// Runs one action whose input root holds `input` as `in/input.bin`; the action
/// succeeds.
fn run_with_input(reapi: SocketAddr, image: &str, input: &[u8]) {
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let channel = connect(reapi).await;
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let file = kbf_proto::reapi::FileNode {
            name: "input.bin".to_owned(),
            digest: Some(put(&cas, input.to_vec()).await),
            ..kbf_proto::reapi::FileNode::default()
        };
        let inner = Directory {
            files: vec![file],
            ..Directory::default()
        };
        let root = Directory {
            directories: vec![kbf_proto::reapi::DirectoryNode {
                name: "in".to_owned(),
                digest: Some(put(&cas, inner.encode_to_vec()).await),
            }],
            ..Directory::default()
        };
        let command = ReapiCommand {
            arguments: vec!["true".to_owned()],
            ..ReapiCommand::default()
        };
        let action = Action {
            command_digest: Some(put(&cas, command.encode_to_vec()).await),
            input_root_digest: Some(put(&cas, root.encode_to_vec()).await),
            platform: Some(Platform {
                properties: vec![Property {
                    name: "container-image".to_owned(),
                    value: image.to_owned(),
                }],
            }),
            ..Action::default()
        };
        let action = put(&cas, action.encode_to_vec()).await;
        let result = tokio::time::timeout(PROMPT, execute(channel, action))
            .await
            .expect("the action ran in time");
        assert_eq!(result.exit_code, 0, "{result:?}");
    });
}

/// `GET /metrics` on the node at `addr`: each sample by name.
fn metrics(addr: SocketAddr) -> std::collections::BTreeMap<String, u64> {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).expect("request");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("answer");
    let samples = answer.lines().filter(|line| !line.starts_with('#'));
    samples
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect()
}

/// Waits until the write tier holds no block: everything uploaded is flushed. Returns
/// the samples then.
fn flushed(addr: SocketAddr) -> std::collections::BTreeMap<String, u64> {
    let deadline = std::time::Instant::now() + PROMPT;
    loop {
        let samples = metrics(addr);
        if samples.get("kbf_write_tier_used_bytes") == Some(&0) {
            return samples;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "not flushed: {samples:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Asserts `input` is on the node's disk once: one inode under the hot store and the
/// data directory holds it, the input cache's entry, of the input's size; and the one
/// cache's fill counts it once, beside no flushed copy.
fn on_disk_once(dir: &Path, input: &[u8], samples: &std::collections::BTreeMap<String, u64>) {
    let digest = kbf_transfer::digest_of(input);
    let copies = files_holding(&[dir.join("hot"), dir.join("data")], input);
    let [(path, size)] = &copies[..] else {
        panic!(
            "the input is on the disk {} times: {copies:?}",
            copies.len()
        );
    };
    let name = format!("{}-{}", digest.hash, digest.size_bytes);
    assert!(path.starts_with(dir.join("data/cache")), "{path:?}");
    assert!(path.ends_with(&name), "{path:?}");
    assert_eq!(*size, input.len() as u64);
    let sample = |name: &str| {
        *samples
            .get(name)
            .unwrap_or_else(|| panic!("no {name}: {samples:?}"))
    };
    assert_eq!(
        sample("kbf_write_tier_flushed_copy_bytes"),
        0,
        "{samples:?}"
    );
    assert_eq!(sample("kbf_local_cache_input_bytes"), input.len() as u64);
    assert_eq!(
        sample("kbf_local_cache_storage_bytes"),
        sample("kbf_write_tier_used_bytes") + sample("kbf_write_tier_flushed_copy_bytes")
    );
    assert_eq!(sample("kbf_local_cache_fill_bytes"), input.len() as u64);
}

/// Catches the input cache and local storage keeping two copies of a blob on one disk
/// (the "two directories" defect: the hot store keeps flushed copies beside the input
/// cache's entries), a fill metric that counts a blob twice or leaves the hot store
/// out, and a restart after a crash that rebuilds the one index with a duplicate or a
/// leak. A client uploads an input of 3 MiB through the frontend (every blob its own
/// block on the node's hot store, so each is flushed at once); the action that reads it
/// runs; once the hot store is flushed, one inode under the hot store and the data
/// directory holds the input (the input cache's entry, which the lease linked) and the
/// node's `/metrics` counts it once. The node is then killed (SIGKILL) and started again
/// behind the same frontend, and the same action runs again: still one inode, counted
/// once.
#[test]
fn an_input_is_on_the_nodes_disk_once() {
    let dir = scratch("one-cache");
    pki(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let image = crun(&bin, &dir.join("data"), FAKE_CRUN, &["echo", "ran"]);
    let addrs = Addrs::new();
    let flags = node_flags(&dir, &addrs, &["--write-own-segment-bytes=1".to_owned()]);
    let input = input_bytes();
    let node = start_node(&bin, &flags);
    let (_frontend, reapi) = start_frontend(&dir, &addrs);
    run_with_input(reapi, &image, &input);
    on_disk_once(&dir, &input, &flushed(addrs.metrics));

    let pid = rustix::process::Pid::from_child(&node.0);
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("SIGKILL");
    drop(node);
    let node = start_node(&bin, &flags);
    run_with_input(reapi, &image, &input);
    on_disk_once(&dir, &input, &flushed(addrs.metrics));
    let status = terminate(node);
    assert!(status.success(), "{status}");
}
