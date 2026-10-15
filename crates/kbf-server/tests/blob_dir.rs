//! The `kbf-server` binary with `--blob-dir` and `--meta-log=raft`: blob bytes in a
//! local directory, an index in a Raft log, both kept across a restart. Every check
//! goes through the REAPI surface of a real process.

#![cfg(unix)]

mod support;

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{BatchUpdateBlobsRequest, batch_update_blobs_request};
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
            // The heartbeat interval the fake daemon expects.
            .args(["--heartbeat-interval-ms", "100"])
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

/// Runs to exit and returns the exit code and stderr; fails the test if the server is
/// still running after `BOUND` (it started when it should have refused).
fn fails(mut c: Command) -> (Option<i32>, String) {
    let mut child = Running(
        c.stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kbf-server"),
    );
    let deadline = Instant::now() + BOUND;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "kbf-server started; it should have refused"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    let mut pipe = child.0.stderr.take().expect("stderr");
    pipe.read_to_string(&mut stderr).expect("UTF-8 stderr");
    (status.code(), stderr)
}

/// The one object file under `blobs` whose bytes contain `data`, and where.
fn object_holding(blobs: &Path, data: &[u8]) -> (PathBuf, usize) {
    let mut found = Vec::new();
    for shard in std::fs::read_dir(blobs.join("objects")).expect("objects") {
        for file in std::fs::read_dir(shard.expect("a shard").path()).expect("a shard") {
            let path = file.expect("an object").path();
            let bytes = std::fs::read(&path).expect("read an object");
            if let Some(at) = bytes.windows(data.len()).position(|w| w == data) {
                found.push((path, at));
            }
        }
    }
    assert_eq!(found.len(), 1, "objects holding the blob: {found:?}");
    found.remove(0)
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

/// Catches a store that answers a read with damaged bytes, or a blob whose object file
/// is gone, as anything but unavailable; a read that does not take the blob out of the
/// index (FindMissing would keep reporting it present, so a client never uploads it
/// again); an upload that does not heal it; and a torn temporary file kept at start.
/// Each blob is uploaded alone, so it is the only blob of its object file. The index
/// still reports a damaged blob present until a read finds it out: nothing checks the
/// directory at start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_or_corrupt_object_is_reported_on_read_and_an_upload_heals_it() {
    let dirs = Dirs::new("damage");
    let (server, before) = started(dirs.server()).await;
    let corrupt = Blob::new("this blob's object file gets one byte flipped");
    let lost = Blob::new("this blob's object file is deleted");
    let kept = Blob::new("this blob's object file is left alone");
    let emptied = Blob::new("this blob's object file is cut to zero bytes");
    for blob in [&corrupt, &lost, &kept, &emptied] {
        before.upload(&[blob]).await;
    }
    drop(before);
    stop(server);

    let (path, at) = object_holding(&dirs.blobs, &corrupt.data);
    let mut bytes = std::fs::read(&path).expect("read the object");
    bytes[at] ^= 0x20;
    std::fs::write(&path, bytes).expect("corrupt the object");
    let (path, _) = object_holding(&dirs.blobs, &lost.data);
    std::fs::remove_file(path).expect("delete the object");
    // A final name whose bytes never reached the disk, as a crash between the write
    // and its sync would leave it on a file system that published the name first.
    let (path, _) = object_holding(&dirs.blobs, &emptied.data);
    std::fs::write(path, b"").expect("empty the object");
    let torn = dirs.blobs.join("tmp").join("7");
    std::fs::write(&torn, b"a write cut short").expect("plant a torn file");

    let (server, after) = started(dirs.server()).await;
    assert!(!torn.exists(), "a torn temporary file outlived the start");
    assert_eq!(read(&after, &kept).await, Ok(kept.data.clone()));
    for blob in [&corrupt, &lost, &emptied] {
        assert!(after.holds(blob).await, "nothing reads objects at start");
        assert_eq!(read(&after, blob).await, Err(Code::Unavailable));
        assert!(
            !after.holds(blob).await,
            "a failed read left the blob indexed"
        );
        after.upload(&[blob]).await;
        assert_eq!(read(&after, blob).await, Ok(blob.data.clone()));
    }
    drop(after);
    stop(server);
    dirs.remove();
}

/// Catches `--blob-dir` accepted with a store it would silently replace (`--store`, the
/// S3 flags), or under `--meta-log=memory`, whose index a restart forgets while the
/// files stay with nothing to delete them; a second server sharing a running server's
/// blob directory; and a directory that cannot be created, taken as started. Each
/// exits 2 and says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob_dir_refuses_what_it_cannot_keep() {
    let dirs = Dirs::new("refused");
    let others: [&[&str]; 5] = [
        &["--store", "memory"],
        &["--store", "s3"],
        &["--s3-endpoint", "http://127.0.0.1:9"],
        &["--s3-bucket", "b"],
        &["--s3-prefix", "x/"],
    ];
    for other in others {
        let mut c = dirs.server();
        c.args(other);
        let (code, stderr) = fails(c);
        assert_eq!(code, Some(2), "{other:?}");
        assert!(stderr.contains("--blob-dir"), "{other:?}: {stderr}");
        assert!(
            stderr.contains("cannot be used with"),
            "{other:?}: {stderr}"
        );
    }

    let mut memory = Command::new(BIN);
    memory.arg("--blob-dir").arg(&dirs.blobs).args(ANY_PORT);
    let (code, stderr) = fails(memory);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("--blob-dir needs --meta-log=raft"),
        "{stderr}"
    );

    let (server, client) = started(dirs.server()).await;
    let mut second = Command::new(BIN);
    second
        .arg("--blob-dir")
        .arg(&dirs.blobs)
        .args(["--meta-log", "raft", "--raft-dir"])
        .arg(dirs.root.join("raft-2"))
        .args(ANY_PORT);
    let (code, stderr) = fails(second);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("lock"), "{stderr}");
    assert!(stderr.contains("LOCK"), "{stderr}");
    drop(client);
    stop(server);

    let file = dirs.root.join("a-file");
    std::fs::write(&file, b"not a directory").expect("plant a file");
    let mut under_a_file = Command::new(BIN);
    under_a_file
        .arg("--blob-dir")
        .arg(file.join("blobs"))
        .args(["--meta-log", "raft", "--raft-dir"])
        .arg(dirs.root.join("raft-3"))
        .args(ANY_PORT);
    let (code, stderr) = fails(under_a_file);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("create"), "{stderr}");
    dirs.remove();
}

/// Catches an action-cache hit served after the store lost one of its output files
/// (deleted under `--blob-dir` while the server was down): the index still holds the
/// output, so only a check of the store turns the hit into a miss. The client must
/// get a miss (the action runs again), not a result whose output it cannot download,
/// and FindMissing must then report the output, so it is uploaded again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hit_whose_output_file_is_lost_is_a_miss_after_a_restart() {
    let dirs = Dirs::new("lost-output");
    let (server, before) = started(dirs.server()).await;
    let mut daemon = before.daemon("node-a", 4, 8).await;
    let job = Job::new("its output file is lost", &[]);
    before.upload(&job.blobs()).await;
    let mut ops = before.execute(&job.action).await;
    let start = daemon.start().await;
    let result = output(&before, "an output whose file is deleted", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result.clone()));
    assert_eq!(before.cached(&job.action).await, Ok(result));
    drop((ops, daemon, before));
    stop(server);

    let out = Blob::new("an output whose file is deleted");
    let (path, _) = object_holding(&dirs.blobs, &out.data);
    std::fs::remove_file(path).expect("delete the output's object");

    let (server, after) = started(dirs.server()).await;
    assert!(after.holds(&out).await, "nothing reads objects at start");
    assert_eq!(after.cached(&job.action).await, Err(Code::NotFound));
    assert!(
        !after.holds(&out).await,
        "the lost output is still reported held"
    );
    drop(after);
    stop(server);
    dirs.remove();
}

/// Catches a store that writes past its free-space floor (`--blob-dir-min-free-bytes`):
/// with a floor no file system meets, an upload is answered UNAVAILABLE and nothing is
/// indexed. The other tests run at the default floor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_are_refused_under_the_free_space_floor() {
    let dirs = Dirs::new("floor");
    let mut c = dirs.server();
    c.args(["--blob-dir-min-free-bytes", &u64::MAX.to_string()]);
    let (server, client) = started(c).await;
    let blob = Blob::new("a blob with no room");
    let response = ContentAddressableStorageClient::new(client.channel())
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: vec![batch_update_blobs_request::Request {
                digest: Some(blob.proto.clone()),
                data: blob.data.clone(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .await
        .expect("BatchUpdateBlobs")
        .into_inner();
    let codes: Vec<i32> = response
        .responses
        .iter()
        .map(|r| r.status.as_ref().expect("a status").code)
        .collect();
    assert_eq!(codes, [Code::Unavailable as i32]);
    assert!(!client.holds(&blob).await);
    drop(client);
    stop(server);
    dirs.remove();
}
