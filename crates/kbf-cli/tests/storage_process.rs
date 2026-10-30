//! `kbf storage` as a separate process (docs/design/architecture-v2.md, section 13 P2):
//! the REAPI conformance suite (`kbf-cas-conformance`) run by the daemon's transfer
//! engine over real gRPC to a child process, and the storage client
//! ([`kbf_storage::service::RemoteStorage`]) over the same connection. Nothing here is
//! in-process: the store, its metadata log and its listener are the child's.

use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use kbf_cas_conformance::Subject;
use kbf_storage::Storage as _;
use kbf_storage::service::RemoteStorage;
use kbf_testkit::dir::TestDir;
use kbf_transfer::{Cas as _, TransferConfig, Transfers};
use tonic::transport::Endpoint;

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// A running `kbf storage`; killed when dropped.
struct Node {
    child: Child,
    addr: String,
}

impl Node {
    /// Stops the node as an operator does, with SIGTERM, and waits for its exit status
    /// (it flushes what it has measured on the way out, which a kill would lose). A node
    /// that has not exited in ten seconds is killed.
    fn stop(&mut self) -> Option<std::process::ExitStatus> {
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
        for _ in 0..200 {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Starts `kbf storage` over new stores under `dir`, plain text on a free loopback port,
/// and waits for its start line.
fn start(dir: &Path, extra: &[&str]) -> Node {
    std::fs::create_dir_all(dir).expect("directory");
    let cold = format!("--cold-store=file://{}", dir.join("cold").display());
    let meta = format!("--meta-dir={}", dir.join("meta").display());
    let mut flags = vec![cold.as_str(), "--cold-store-floor-bytes=1", meta.as_str()];
    flags.extend_from_slice(extra);
    spawn(dir, &flags, &[])
}

/// `kbf storage --storage-listen=127.0.0.1:0` and these flags, with these variables in
/// its environment, and the empty settings file of `dir` as its `--config` (it reads no
/// `/etc/kbf/kbf.toml` of the host).
fn spawn(dir: &Path, flags: &[&str], env: &[(&str, &str)]) -> Node {
    std::fs::create_dir_all(dir).expect("directory");
    let mut child = Command::new(BIN)
        .args(["storage", "--storage-listen=127.0.0.1:0"])
        .args(flags)
        .arg(kbf_testkit::config::flag(dir))
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn kbf storage");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("a start line");
    let addr = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix("storage="))
        .unwrap_or_else(|| panic!("no storage= in the start line {line:?}"))
        .to_owned();
    assert!(line.starts_with("kbf-storage "), "{line}");
    Node { child, addr }
}

/// One `kbf storage` process per [`Subject::fresh`], each over empty stores.
struct Process {
    // The nodes come first: fields drop in order, and the directory they write in must
    // outlive them.
    nodes: Mutex<Vec<Node>>,
    scratch: TestDir,
}

impl Process {
    fn new() -> Self {
        Self {
            nodes: Mutex::new(Vec::new()),
            scratch: TestDir::under(env!("CARGO_TARGET_TMPDIR"), "storage-process"),
        }
    }
}

impl Subject for Process {
    type Cas = Transfers;

    async fn fresh(&self) -> Arc<Transfers> {
        let mut nodes = self.nodes.lock().expect("lock");
        let node = start(&self.scratch.join(format!("node-{}", nodes.len())), &[]);
        let endpoint = Endpoint::from_shared(format!("http://{}", node.addr)).expect("endpoint");
        nodes.push(node);
        Arc::new(Transfers::new(endpoint, TransferConfig::default()))
    }

    fn scratch(&self) -> &Path {
        self.scratch.path()
    }
}

kbf_cas_conformance::tests!(Process::new());

/// Catches a storage client that cannot talk to the process that serves it: blobs and
/// the action cache over `RemoteStorage` against `kbf storage`, as the frontend will
/// use them (the conformance suite above is the daemon's side).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_client_reads_what_it_wrote_in_another_process() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "storage-client");
    let node = start(&dir, &[]);
    let channel = Endpoint::from_shared(format!("http://{}", node.addr))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let remote = RemoteStorage::new(channel, 0);
    let blob = Bytes::from_static(b"stored by one process, read from another");
    let digest = remote.put(blob.clone()).await.expect("put");
    assert_eq!(remote.read_blob(&digest).await.expect("read"), blob);
    let other = kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [9; 32], 3);
    assert_eq!(
        remote
            .find_missing(&[digest, other])
            .await
            .expect("missing"),
        [other]
    );
}

/// Catches a storage node that starts without a store (it must exit 2 saying which flag
/// is missing, as `kbf-server` does), and one that accepts a state store (it must exit 2
/// before opening anything: storage nodes reach the state store only through a frontend).
#[test]
fn a_refused_start_exits_2_and_a_state_store_is_refused() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "storage-refused");
    // Every start but the first gets the empty settings file of the test's own, not the host's.
    let run = |args: &[&str]| {
        let config = kbf_testkit::config::flag(&dir);
        Command::new(BIN)
            .args(args)
            .arg(config)
            .output()
            .expect("run kbf")
    };
    // Named and missing, the settings file is the refusal (exit 2, the file named).
    let out = Command::new(BIN)
        .args(["storage", "--config", "/nonexistent/kbf.toml"])
        .output()
        .expect("run kbf");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("/nonexistent/kbf.toml"), "{err}");
    assert!(out.stdout.is_empty());
    // Given flags, they are the start, and it names the one it lacks.
    let out = run(&["storage", "--storage-listen=127.0.0.1:0"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("kbf storage: --cold-store is required"),
        "{err}"
    );
    assert!(out.stdout.is_empty());
    let out = run(&[
        "storage",
        "--state-store=sqlite:///x",
        "--cold-store=file:///x",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("kbf storage refuses --state-store: "), "{err}");
    assert!(out.stdout.is_empty());
}

/// Catches the two store shapes of `kbf storage` that the conformance runs above do not
/// start: an `s3://` cold store (no connection is made until a blob moves, so a node
/// starts against an endpoint nobody listens on; its key pair is the standard variables)
/// and a hot store (the write tier's flusher runs beside the listener); and a node that
/// does not exit 0 on SIGTERM.
#[test]
fn the_s3_and_hot_store_shapes_start_and_stop_on_sigterm() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "storage-shapes");
    let meta = format!("--meta-dir={}", dir.join("s3-meta").display());
    let keys = [("AWS_ACCESS_KEY_ID", "a"), ("AWS_SECRET_ACCESS_KEY", "b")];
    let mut s3 = spawn(
        &dir,
        &[
            "--cold-store=s3://bucket/kbf/?endpoint=http://127.0.0.1:1",
            &meta,
        ],
        &keys,
    );
    assert!(s3.stop().is_some_and(|status| status.success()));
    let cold = format!("--cold-store=file://{}", dir.join("cold").display());
    let hot = format!("--hot-store=file://{}", dir.join("hot").display());
    std::fs::create_dir_all(dir.join("hot")).expect("hot directory");
    let mut tiered = spawn(&dir, &[&cold, "--cold-store-floor-bytes=1", &hot], &[]);
    assert!(tiered.stop().is_some_and(|status| status.success()));
}

/// Catches a `kbf storage` that serves plain text off loopback (it would admit Write and
/// ActionCommit from anyone): on a wildcard IPv4 and IPv6 address and on a LAN one, with
/// no TLS flags, the process exits 2 with one `kbf storage:` line that names the address and the
/// three flags, prints no start line, and opens no store (the meta directory is not
/// created); and with the TLS flags it gets past that check (to the missing certificate
/// file, not to a start). Loopback without TLS is every other test here.
#[test]
fn plain_text_off_loopback_is_a_refused_start() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "storage-off-loopback");
    let cold = format!("--cold-store=file://{}", dir.join("cold").display());
    let meta = format!("--meta-dir={}", dir.join("meta").display());
    // A node that wrongly starts serves until it is killed: wait a while, then kill it
    // and let the assertions below say it did not exit 2.
    let run = |listen: &str, more: &[&str]| {
        let mut child = Command::new(BIN)
            .args([
                "storage",
                &cold,
                "--cold-store-floor-bytes=1",
                &meta,
                &format!("--storage-listen={listen}"),
            ])
            .args(more)
            .arg(kbf_testkit::config::flag(&dir))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kbf storage");
        for _ in 0..200 {
            if child.try_wait().expect("wait").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        child.wait_with_output().expect("output")
    };
    for listen in ["0.0.0.0:0", "[::]:0", "192.0.2.10:0"] {
        let out = run(listen, &[]);
        assert_eq!(out.status.code(), Some(2), "{listen}");
        let err = String::from_utf8_lossy(&out.stderr);
        let refusals = err
            .lines()
            .filter(|l| l.starts_with("kbf storage: "))
            .count();
        assert_eq!(refusals, 1, "{err}");
        for want in [
            "kbf storage: --storage-listen ",
            listen,
            "--storage-tls-cert",
            "--storage-tls-key",
            "--storage-client-ca",
        ] {
            assert!(err.contains(want), "{listen}: {err}");
        }
        assert!(out.stdout.is_empty(), "{listen}");
    }
    assert!(!dir.join("meta").exists(), "a refused start opened a store");
    let nowhere = dir.join("none.pem").display().to_string();
    let out = run(
        "0.0.0.0:0",
        &[
            &format!("--storage-tls-cert={nowhere}"),
            &format!("--storage-tls-key={nowhere}"),
            &format!("--storage-client-ca={nowhere}"),
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    // The startup self-checks name all three files at once, not the first the TLS load
    // meets, and the refusal is about them, not the listener's address.
    for flag in [
        "--storage-tls-cert",
        "--storage-tls-key",
        "--storage-client-ca",
    ] {
        assert!(
            err.lines().any(|l| l.contains(flag)
                && l.contains("none.pem")
                && l.contains("does not exist")),
            "{flag}: {err}"
        );
    }
    assert_eq!(err.lines().count(), 3, "{err}");
    assert!(!err.contains("--storage-listen"), "{err}");
}

/// The start-up pass's last line (`kbf-server`'s `startup_pass`).
const PASS_DONE: &str = "start-up pass over the cold store directory done";

/// What a round of eviction that evicted logs (`kbf-front`'s `Cache::evict_round`).
const EVICTED: &str = "evicted the least recently used objects";

/// Catches the start-up pass or the evictor not run over a storage node's `file://` cold
/// store (defects: `kbf_server::start`'s `file://` arm passes no pass; the evictor loop
/// removed): a single-node farm is such a node's main use, and the pass deletes its
/// orphans and forgets its lost objects, the evictor keeps its disk from filling. Without
/// a hot store (uploads go straight to the cold store), on a fake 16 MiB filesystem
/// holding only the object files with a 4 MiB floor and no grace window (the server's
/// hidden test flags, which `kbf storage` takes as `kbf-server` does), the pass logs its
/// last line, twenty uploads of 1 MiB each succeed (one that finds the directory short of
/// room is tried again after a round), the evictor logs a round and the first blob is
/// reported missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_up_pass_and_the_evictor_run_over_a_file_cold_store() {
    const MIB: usize = 1 << 20;
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "storage-pass");
    let mut child = Command::new(BIN)
        .args(["storage", "--storage-listen=127.0.0.1:0"])
        .arg(format!(
            "--cold-store=file://{}",
            dir.join("cold").display()
        ))
        .arg(format!("--meta-dir={}", dir.join("meta").display()))
        .arg(format!("--cold-store-size-bytes={}", 16 * MIB))
        .arg(format!("--cold-store-floor-bytes={}", 4 * MIB))
        .arg("--cold-store-evict-grace-ms=0")
        .arg(kbf_testkit::config::flag(&dir))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kbf storage");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let (seen, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            if line.contains(PASS_DONE) || line.contains(EVICTED) {
                let _ = seen.send(line);
            }
        }
    });
    let wait = std::time::Duration::from_secs(60);
    let line = stdout.lines().next().expect("a start line").expect("read");
    let addr = line
        .split(' ')
        .find_map(|field| field.strip_prefix("storage="))
        .unwrap_or_else(|| panic!("no storage= in {line:?}"))
        .to_owned();
    let mut node = Node { child, addr };
    let line = lines.recv_timeout(wait).expect("the pass's last line");
    assert!(line.contains(PASS_DONE), "{line}");

    let endpoint = Endpoint::from_shared(format!("http://{}", node.addr)).expect("endpoint");
    let cas = Transfers::new(endpoint, TransferConfig::default());
    let mut blobs = Vec::new();
    // A write that finds the directory short of room is refused as unavailable until a
    // round of eviction has made room; the client tries again, as any does.
    let deadline = std::time::Instant::now() + wait;
    for i in 0..20_u8 {
        let digest = loop {
            match cas.put(vec![i; MIB]).await {
                Ok(digest) => break digest,
                Err(e) if std::time::Instant::now() < deadline => {
                    eprintln!("blob {i}: {e}; trying again");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => panic!("blob {i} was not stored in time: {e}"),
            }
        };
        blobs.push(digest);
    }
    let line = lines.recv_timeout(wait).expect("an eviction round");
    assert!(line.contains(EVICTED), "{line}");
    let missing = cas.missing(&blobs[..1]).await;
    assert_eq!(missing, blobs[..1], "the least recently used blob was kept");
    assert!(node.stop().is_some_and(|status| status.success()));
}
