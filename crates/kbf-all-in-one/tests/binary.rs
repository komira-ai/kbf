//! The `kbf-all-in-one` binary end to end: one action, executed through the REAPI
//! listener of the server half, run by the daemon half's container driver over a
//! stand-in `podman` (`kbf-fake-podman`, which runs the action as a plain process) and a
//! cgroup tree of plain directories, its outputs, stdout and stderr stored through the
//! in-process data plane and read back over REAPI. And the starts each half refuses.

#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, ActionResult, Command as ReapiCommand, Digest, Directory, ExecuteRequest,
    ExecuteResponse, Platform, Tree,
};
use kbf_transfer::{Cas as _, TransferConfig, Transfers, fetch};
use prost::Message as _;
use tonic::transport::{Channel, Endpoint};

// The stand-in `podman` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

const BIN: &str = env!("CARGO_BIN_EXE_kbf-all-in-one");

/// How long the test waits for what should happen promptly.
const PROMPT: Duration = Duration::from_secs(60);

/// The per-architecture manifest the stand-in image store holds.
const MANIFEST: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-all-in-one")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// `--id-files`: a passwd naming this test's user `kbf`, with 65,536 subordinate ids.
fn id_files(dir: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt as _;

    let ids = dir.join("ids");
    std::fs::create_dir_all(&ids).expect("mkdir");
    let passwd = ids.join("passwd");
    std::fs::write(&passwd, "").expect("write");
    let uid = std::fs::metadata(&passwd).expect("stat").uid();
    std::fs::write(&passwd, format!("kbf:x:{uid}:{uid}::/:/usr/bin/false\n")).expect("write");
    for file in ["subuid", "subgid"] {
        std::fs::write(ids.join(file), "kbf:100000:65536\n").expect("write");
    }
    ids
}

/// The stand-in `podman` in `bin`, and `kbf-act` beside it, with an image store holding
/// [`MANIFEST`] as image `img1`; the stand-in runs `steps` as each action. Returns the
/// image as an action names it.
fn podman(bin: &Path, steps: &[&str]) -> String {
    let state = bin.join("state");
    std::fs::create_dir_all(&state).expect("mkdir");
    let fake = env!("CARGO_BIN_EXE_kbf-fake-podman");
    std::os::unix::fs::symlink(fake, bin.join("podman")).expect("link podman");
    kbf_testkit::act::link_self(&bin.join(kbf_testkit::act::NAME)).expect("link kbf-act");
    std::fs::write(state.join("image-id"), "img1\n").expect("image id");
    std::fs::write(state.join("nonce"), format!("{}:", std::process::id())).expect("nonce");
    let knob: String = steps.iter().map(|s| format!("{s}\n")).collect();
    std::fs::write(state.join("action"), knob).expect("action");
    let digest = format!("sha256:{}", kbf_transfer::digest_of(MANIFEST).hash);
    let store = state.join("store/fake-images/img1");
    std::fs::create_dir_all(&store).expect("mkdir store");
    let file = kbf_driver_container::image::manifest_file(&digest);
    std::fs::write(store.join(file), MANIFEST).expect("manifest");
    format!("docker://registry.test/tools/busybox@{digest}")
}

/// A cgroup v2 mount of plain directories at `bin/cgroup`, where the stand-in `podman`
/// looks, holding the daemon's unit; the flags that point the daemon at it.
fn cgroup_tree(bin: &Path) -> Vec<String> {
    let root = bin.join("cgroup");
    let unit = root.join("kbf.slice/kbf-all-in-one.service");
    std::fs::create_dir_all(&unit).expect("mkdir");
    let plant = |path: PathBuf, text: &str| std::fs::write(path, text).expect("plant");
    plant(
        root.join("cgroup.controllers"),
        "cpuset cpu io memory pids\n",
    );
    plant(unit.join("cgroup.controllers"), "cpu memory pids\n");
    plant(unit.join("cgroup.procs"), "1\n");
    plant(unit.join("cpuset.cpus.effective"), "0\n");
    let own = bin.join("self-cgroup");
    plant(own.clone(), "0::/kbf.slice/kbf-all-in-one.service\n");
    vec![
        format!("--cgroup-root={}", root.display()),
        format!("--self-cgroup={}", own.display()),
    ]
}

/// The flags of a one-node farm in `dir`: the REAPI listener on a free port, a cold
/// store and a hot store (the write tier, holding the metadata log), and the daemon
/// half's container driver. No certificate, key or server address: the halves talk by
/// calls.
fn farm_flags(dir: &Path) -> Vec<String> {
    let at = |name: &str| dir.join(name).display().to_string();
    vec![
        "--listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", at("cold")),
        format!("--hot-store=file://{}", at("hot")),
        "--node-id=node-1".to_owned(),
        "--reconnect-ms=50".to_owned(),
        "--driver=container".to_owned(),
        format!("--data-dir={}", at("data")),
    ]
}

/// Kills the process when dropped, so a failed test leaves nothing running. Holds the
/// read end of its stdout, so a line it writes later never meets a closed pipe.
struct Running {
    child: Child,
    _stdout: Option<BufReader<std::process::ChildStdout>>,
}

impl Running {
    /// Waits for the process to end on its own, for at most [`PROMPT`].
    fn wait(mut self) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + PROMPT;
        loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return status;
            }
            assert!(std::time::Instant::now() < deadline, "still running");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Stores `bytes` through the REAPI listener.
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

/// Starts the binary on a one-node farm in `dir` whose stand-in `podman` runs `steps` as
/// each action; returns it once it has printed its start line, with the REAPI address
/// that line names and the image an action names.
fn farm(dir: &Path, steps: &[&str]) -> (Running, SocketAddr, String) {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let image = podman(&bin, steps);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(BIN)
        .args(farm_flags(dir))
        .arg(format!("--id-files={}", id_files(dir).display()))
        .args(cgroup_tree(&bin))
        .env("PATH", path)
        // The stand-in finds its state beside the link it was run through, which the
        // driver runs by the name `PATH` finds (argv0 `podman`): from here.
        .current_dir(&bin)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a start line");
    let running = Running {
        child,
        _stdout: Some(stdout),
    };
    let reapi: SocketAddr = line
        .trim_end()
        .split(' ')
        .find_map(|field| field.strip_prefix("reapi="))
        .unwrap_or_else(|| panic!("no reapi= in {line:?}"))
        .parse()
        .expect("an address");
    (running, reapi, image)
}

/// The TCP ports process `pid` listens on: each socket among its descriptors
/// (`/proc/<pid>/fd`) that `/proc/<pid>/net/tcp` or `tcp6` lists in state LISTEN.
fn listening_ports(pid: u32) -> std::collections::BTreeSet<u16> {
    let mut sockets = std::collections::BTreeSet::new();
    for fd in std::fs::read_dir(format!("/proc/{pid}/fd")).expect("list fds") {
        let Ok(target) = std::fs::read_link(fd.expect("an fd").path()) else {
            continue; // closed since it was listed
        };
        let target = target.to_string_lossy();
        if let Some(inode) = target
            .strip_prefix("socket:[")
            .and_then(|t| t.strip_suffix(']'))
        {
            sockets.insert(inode.to_owned());
        }
    }
    let mut ports = std::collections::BTreeSet::new();
    for table in ["tcp", "tcp6"] {
        let text = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")).expect("read");
        for row in text.lines().skip(1) {
            let fields: Vec<&str> = row.split_whitespace().collect();
            let (Some(local), Some(state), Some(inode)) =
                (fields.get(1), fields.get(3), fields.get(9))
            else {
                continue;
            };
            // 0A is TCP_LISTEN.
            if *state != "0A" || !sockets.contains(*inode) {
                continue;
            }
            let port = local.rsplit(':').next().expect("a port");
            ports.insert(u16::from_str_radix(port, 16).expect("a hex port"));
        }
    }
    ports
}

/// Catches a worker listener bound by the binary (no remote daemon may reach an
/// all-in-one process: its daemon half's control plane is calls into the server half),
/// or any other listener but the REAPI one: once the start line is out, the process
/// listens on exactly one TCP port, the one the line names as `reapi=`.
#[test]
fn the_binary_listens_on_the_reapi_port_alone() {
    let dir = scratch("listeners");
    let (running, reapi, _) = farm(&dir, &["echo", "unused"]);
    let ports = listening_ports(running.child.id());
    assert_eq!(
        ports,
        std::collections::BTreeSet::from([reapi.port()]),
        "listening on {ports:?}; reapi={reapi}"
    );
}

/// Catches the daemon half's outputs not reaching the server half's CAS, or reaching it
/// other than through the in-process plane (the binary is given no `--cas`, so a daemon
/// half wired to the transfer engine refuses to start), the helper re-execs not
/// dispatched before the flags are read (each action's keeper is this binary, run with
/// arguments the command line refuses), and SIGTERM not stopping both halves with exit
/// 0. One action, run by the container driver over the stand-in `podman`, writes a file,
/// a directory holding a file, stdout and stderr; each is read back over REAPI and is
/// what the action wrote.
#[test]
fn an_actions_outputs_go_through_the_in_process_plane() {
    let dir = scratch("e2e");
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
    let (running, reapi, image) = farm(&dir, &steps);

    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let channel = Endpoint::from_shared(format!("http://{reapi}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
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
    });
    // Its connections closed, so the server half's drain does not wait for them.
    drop(tokio);

    let pid = rustix::process::Pid::from_child(&running.child);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
    let status = running.wait();
    assert!(status.success(), "{status}");
}

/// Catches the daemon's helper re-execs dispatched after the flags are read, or not at
/// all: run with keeper arguments that name no mode, as a driver never runs it, the
/// binary answers as the keeper does (its usage line, exit 2), not with clap's error
/// about the flags it lacks, and starts no server (no start line on stdout).
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
    assert!(out.stdout.is_empty(), "a server started");
}

/// Runs the binary with a one-node farm's flags in a fresh directory, `extra` taking the
/// place of any it names again, and returns its exit status and stderr; fails if it is
/// still running after [`PROMPT`].
fn refused(name: &str, extra: &[&str]) -> (Option<i32>, String) {
    let dir = scratch(name);
    let name = |flag: &str| flag.split('=').next().unwrap_or(flag).to_owned();
    let named: Vec<String> = extra.iter().map(|flag| name(flag)).collect();
    let farm = farm_flags(&dir);
    let stderr = dir.join("stderr");
    let child = Command::new(BIN)
        .args(farm.iter().filter(|flag| !named.contains(&name(flag))))
        .args(extra)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).expect("stderr file"))
        .spawn()
        .expect("run");
    // A start that is not refused serves until killed: it fails here, not by hanging.
    let running = Running {
        child,
        _stdout: None,
    };
    let status = running.wait();
    let stderr = std::fs::read_to_string(&stderr).expect("read stderr");
    (status.code(), stderr)
}

/// Catches a start either half refuses that exits 0 or as the other half would, or
/// that does not say which half refused: a server whose flags its own start refuses
/// (a cold store that cannot be made) exits 2, as `kbf-server` does; a daemon whose
/// driver cannot start (no subordinate ids for its user) exits 1, as `kbf-daemon` does;
/// a driver other than the container driver is a usage error, 2.
#[test]
fn a_start_either_half_refuses_says_which() {
    let (code, stderr) = refused(
        "server-refuses",
        &["--cold-store=file:///proc/nonexistent/cold"],
    );
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("kbf-all-in-one: server: "), "{stderr}");
    let (code, stderr) = refused("daemon-refuses", &["--id-files=/nonexistent/ids"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("kbf-all-in-one: daemon: "), "{stderr}");
    let (code, stderr) = refused("native", &["--driver=native"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("container driver only"), "{stderr}");
}

/// Catches a node identity, a dial or a worker listener taken on the command line (the
/// daemon half dials no server and presents no certificate; the process admits no
/// remote daemon), or refused without saying which flag and why: each is a usage error,
/// exit 2, before anything starts (no start line).
#[test]
fn node_identity_and_worker_listener_flags_are_refused() {
    for (flag, says) in [
        (
            "--server=https://127.0.0.1:8981",
            "--server: the daemon half opens its sessions",
        ),
        (
            "--ca-cert=ca.pem",
            "--ca-cert: the daemon half opens its sessions",
        ),
        ("--cert=node.pem", "presents no certificate"),
        ("--key=node.key", "--key: the daemon half"),
        ("--tls-server-name=localhost", "--tls-server-name: "),
        (
            "--worker-listen=127.0.0.1:8981",
            "--worker-listen: kbf-all-in-one binds no worker listener",
        ),
        ("--worker-tls-cert=server.pem", "--worker-tls-cert: "),
        ("--worker-tls-key=server.key", "--worker-tls-key: "),
        ("--worker-client-ca=ca.pem", "--worker-client-ca: "),
        (
            "--worker-deny-list=deny.json",
            "--worker-deny-list: kbf-all-in-one binds no worker listener",
        ),
    ] {
        let (code, stderr) = refused("node-identity", &[flag]);
        assert_eq!(code, Some(2), "{flag}: {stderr}");
        assert!(stderr.contains(says), "{flag}: {stderr}");
    }
}
