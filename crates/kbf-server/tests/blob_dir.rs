//! The `kbf-server` binary with `--blob-dir` and `--meta-log=raft`: blob bytes in a
//! local directory, an index in a Raft log, both kept across a restart. Every check
//! goes through the REAPI surface of a real process.

#![cfg(unix)]

mod support;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use support::{Blob, Client, Job, done, output, ran, response};
use tonic::Code;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-server");
const ANY_PORT: [&str; 4] = ["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"];
/// How long a test waits for the server to print its start line or to exit.
const BOUND: Duration = Duration::from_secs(10);

/// A running server, killed and reaped when dropped, so a failed assertion never
/// leaves it running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A blob directory and a Raft directory for one test, fresh, under the target
/// directory.
struct Dirs {
    root: PathBuf,
    blobs: PathBuf,
    raft: PathBuf,
}

impl Dirs {
    fn new(name: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-server-blob-dir")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Self {
            blobs: root.join("blobs"),
            raft: root.join("raft"),
            root,
        }
    }

    /// `kbf-server --blob-dir <blobs> --meta-log=raft --raft-dir <raft>` on free ports.
    fn server(&self) -> Command {
        let mut c = Command::new(BIN);
        c.arg("--blob-dir")
            .arg(&self.blobs)
            .args(["--meta-log", "raft", "--raft-dir"])
            .arg(&self.raft)
            .args(ANY_PORT)
            .args(["--shutdown-timeout-secs", "1"]);
        c
    }

    fn remove(self) {
        std::fs::remove_dir_all(&self.root).expect("remove the test directories");
    }
}

/// Starts the server, reads its start line, and connects a client to both listeners.
async fn started(mut c: Command) -> (Running, Client) {
    let mut child = Running(
        c.stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn kbf-server"),
    );
    let out = child.0.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let line = BufReader::new(out)
            .lines()
            .map_while(Result::ok)
            .find(|l| !l.is_empty());
        let _ = tx.send(line);
    });
    let line = match rx.recv_timeout(BOUND) {
        Ok(Some(line)) => line,
        Ok(None) => panic!("kbf-server closed stdout without a start line"),
        Err(e) => panic!("no start line within {BOUND:?} ({e})"),
    };
    let addr = |name: &str| -> SocketAddr {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name} in {line:?}"))
            .parse()
            .expect("an address")
    };
    let client = Client::connect(addr("reapi="), addr("worker=")).await;
    (child, client)
}

/// Stops the server with SIGINT and checks it exits 0 within `BOUND`.
fn stop(mut child: Running) {
    let sent = Command::new("kill")
        .args(["-INT", &child.0.id().to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success());
    let deadline = Instant::now() + BOUND;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "still running {BOUND:?} after SIGINT"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "kbf-server exited with {status}");
}

/// `blob` read over ByteStream on the REAPI listener, or the call's code.
async fn read(client: &Client, blob: &Blob) -> Result<Vec<u8>, Code> {
    let request = ReadRequest {
        resource_name: format!("blobs/{}/{}", blob.digest.hash_hex(), blob.data.len()),
        ..ReadRequest::default()
    };
    let mut stream = ByteStreamClient::new(client.channel())
        .read(request)
        .await
        .map_err(|s| s.code())?
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(message) = stream.message().await.map_err(|s| s.code())? {
        bytes.extend_from_slice(&message.data);
    }
    Ok(bytes)
}

/// Bytes that differ from their neighbours, so a read at the wrong offset cannot match.
fn pattern(len: u32) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

/// Catches a farm on one host that loses its blobs at a restart: the store in memory,
/// as without `--blob-dir` (and `--meta-log=raft` refuses that pairing outright), or a
/// blob directory that is not where the objects went, or that the next start does not
/// read. The action-cache entry must still be a hit after the restart (its result blob
/// is read back to answer it), and every blob, a 3 MiB one included, must read back
/// byte for byte over ByteStream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_keeps_the_blobs_and_the_action_cache() {
    let dirs = Dirs::new("restart");
    let (server, before) = started(dirs.server()).await;
    let mut daemon = before.daemon("node-a", 4, 8).await;
    let job = Job::new("before the restart", &[]);
    before.upload(&job.blobs()).await;
    let big = Blob::new(pattern(3 << 20));
    before.upload(&[&big]).await;
    let mut ops = before.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&before, "ran before the restart", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    assert_eq!(before.cached(&job.action).await, Ok(result.clone()));
    drop((ops, daemon, before));
    stop(server);

    let (server, after) = started(dirs.server()).await;
    assert_eq!(
        after.cached(&job.action).await,
        Ok(result),
        "the action cache entry did not survive the restart"
    );
    let out = Blob::new("ran before the restart");
    for blob in job.blobs().into_iter().chain([&out, &big]) {
        assert_eq!(
            read(&after, blob).await,
            Ok(blob.data.clone()),
            "blob {} after the restart",
            blob.digest
        );
    }
    drop(after);
    stop(server);
    dirs.remove();
}
