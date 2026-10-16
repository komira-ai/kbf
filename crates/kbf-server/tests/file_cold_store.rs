//! The `kbf-server` binary with a `file://` `--cold-store` and a `--meta-dir`: blob bytes
//! in a local directory, an index in a Raft log, both kept across a restart. Every check
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

    /// `kbf-server --cold-store file://<blobs> --meta-dir <raft>` on free ports, with a
    /// floor of one byte: the default floor depends on the disk the tests run on, and
    /// under twice it the evictor would take what a test reads back.
    fn server(&self) -> Command {
        let mut c = self.unfloored();
        c.args(["--cold-store-floor-bytes", "1"]);
        c
    }

    /// [`Dirs::server`] without a floor.
    fn unfloored(&self) -> Command {
        let mut c = Command::new(BIN);
        c.arg("--cold-store")
            .arg(format!("file://{}", self.blobs.display()))
            .arg("--meta-dir")
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

/// Waits, at most `BOUND`, until FindMissing reports every one of `blobs`: the start-up
/// pass runs in the background once the server serves.
async fn forgotten(client: &Client, blobs: &[&Blob]) {
    let deadline = Instant::now() + BOUND;
    for blob in blobs {
        while client.holds(blob).await {
            assert!(
                Instant::now() < deadline,
                "a blob whose object file is gone is still reported held"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Bytes that differ from their neighbours, so a read at the wrong offset cannot match.
fn pattern(len: u32) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

/// Catches a farm on one host that loses its blobs at a restart: the store in memory,
/// as a store with no directory would be, or a
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

/// Catches a blob whose object file is gone (deleted, or cut to zero bytes) kept in the
/// index after the start-up pass, so FindMissing reports it present and a client never
/// uploads it again, and a read of it answered as anything but not found once it is
/// forgotten; a store that answers a read with damaged bytes as anything but
/// unavailable, or a read that does not take that blob out of the index (the pass
/// reads headers, not bytes, so only a read finds the damage); an upload that does not
/// heal each; and a torn temporary file kept at start. Each blob is uploaded alone, so
/// it is the only blob of its object file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_object_is_forgotten_at_start_and_a_corrupt_one_on_read() {
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
    forgotten(&after, &[&lost, &emptied]).await;
    for blob in [&lost, &emptied] {
        assert_eq!(read(&after, blob).await, Err(Code::NotFound));
    }
    assert!(
        after.holds(&corrupt).await,
        "the pass read an object's bytes"
    );
    assert_eq!(read(&after, &corrupt).await, Err(Code::Unavailable));
    assert!(
        !after.holds(&corrupt).await,
        "a failed read left the blob indexed"
    );
    for blob in [&corrupt, &lost, &emptied] {
        after.upload(&[blob]).await;
        assert_eq!(read(&after, blob).await, Ok(blob.data.clone()));
    }
    drop(after);
    stop(server);
    dirs.remove();
}

/// Catches a second server sharing a running server's cold store directory (its own
/// metadata directory does not make it a different store), and a directory that cannot
/// be created taken as started. Each exits 2 and says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_cold_store_refuses_what_it_cannot_keep() {
    let dirs = Dirs::new("refused");
    let (server, client) = started(dirs.server()).await;
    let mut second = Command::new(BIN);
    second
        .arg("--cold-store")
        .arg(format!("file://{}", dirs.blobs.display()))
        .arg("--meta-dir")
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
        .arg("--cold-store")
        .arg(format!("file://{}", file.join("blobs").display()))
        .arg("--meta-dir")
        .arg(dirs.root.join("raft-3"))
        .args(ANY_PORT);
    let (code, stderr) = fails(under_a_file);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("create"), "{stderr}");
    dirs.remove();
}

/// Catches an action-cache hit served after the store lost one of its output files
/// (deleted under a `file://` cold store while the server was down): the index still
/// holds the output until the start-up pass forgets it with the entry, or a check of
/// the store at the hit marks it. The client must get a miss (the action runs again),
/// not a result whose output it cannot download, and FindMissing must report the
/// output, so it is uploaded again.
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
    assert_eq!(after.cached(&job.action).await, Err(Code::NotFound));
    forgotten(&after, &[&out]).await;
    drop(after);
    stop(server);
    dirs.remove();
}

/// Catches a store that writes past its free-space floor (`--cold-store-floor-bytes`):
/// with a floor no file system meets, an upload is answered UNAVAILABLE and nothing is
/// indexed. The other tests run at the default floor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_are_refused_under_the_free_space_floor() {
    let dirs = Dirs::new("floor");
    let mut c = dirs.unfloored();
    c.args(["--cold-store-floor-bytes", &u64::MAX.to_string()]);
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

/// Catches a full `file://` cold store that stops taking uploads: a write refused for want of
/// room that does not wake the evictor (mutant: no nudge; the next round is the 10 s
/// tick, after every retry of the PUT has failed), an evictor that frees nothing, or
/// one that evicts what was used last. On a file system of 16 MiB that holds only the
/// object files, with a 4 MiB floor and no grace window, twenty uploads of 1 MiB each
/// (twice the room) all succeed; the first blob is then reported
/// missing (evicted), and the last reads back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fills_past_floor_and_keeps_serving() {
    const MIB: u32 = 1 << 20;
    let dirs = Dirs::new("fills");
    let mut c = dirs.unfloored();
    c.args(["--cold-store-size-bytes", &(16 * MIB).to_string()])
        .args(["--cold-store-floor-bytes", &(4 * MIB).to_string()])
        .args(["--cold-store-evict-grace-ms", "0"]);
    let (server, client) = started(c).await;
    let blobs: Vec<Blob> = (0..20_u32)
        .map(|i| {
            let mut data = pattern(MIB);
            data[..4].copy_from_slice(&i.to_be_bytes());
            Blob::new(data)
        })
        .collect();
    for blob in &blobs {
        client.upload(&[blob]).await;
    }
    assert!(
        !client.holds(&blobs[0]).await,
        "the least recently used blob was kept"
    );
    let last = blobs.last().expect("a blob");
    assert_eq!(read(&client, last).await, Ok(last.data.clone()));
    drop(client);
    stop(server);
    dirs.remove();
}
