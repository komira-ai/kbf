//! The registry front of a build node, in the `tierstore` process: a manifest and a blob that sit
//! in the upstream are pulled by digest over plain HTTP, the second read of a blob is answered
//! from the cache's disk, a blob larger than `whole_fill_limit` is passed through and still
//! checked against its digest, and a start whose OCI address is taken fails.

use std::fs::Permissions;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::{Capabilities, ObjectKey, ObjectStore};
use kbf_testkit::dir::TestDir;
use kbf_testkit::reaped::{Reaped, first_line, spawn};
use rustix::process::{Pid, Signal, kill_process};

use crate::client::Conn;
use crate::rig::hex_of;

const MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#;

/// A blob of `len` bytes that does not repeat within a chunk.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + i / 251) % 256) as u8).collect()
}

/// The value of the counter `name` in the text of `/metricz`.
#[track_caller]
fn counter(text: &str, name: &str) -> u64 {
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '));
    let value = line.unwrap_or_else(|| panic!("no {name} in {text}"));
    value.parse().expect("a number")
}

/// A key file only its owner can read, in `dir`.
fn secret(dir: &TestDir, name: &str, text: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).expect("a file");
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).expect("a mode");
    path
}

/// Puts `bytes` in the upstream where the front reads `hex`, whatever `bytes` hash to.
async fn seed(fake: &FakeS3, hex: &str, bytes: &[u8]) {
    let upstream = fake.store(Capabilities {
        conditional_put: true,
        object_lock: false,
    });
    let key = ObjectKey::new(format!("oci/blobs/sha256/{hex}")).expect("a key");
    let body = Bytes::copy_from_slice(bytes);
    upstream.put_new(&key, body, None).await.expect("put");
}

/// A build node in a process, over the fake upstream, with the OCI front on plain HTTP.
struct Node<'a> {
    fake: &'a FakeS3,
    child: Reaped,
    s3: String,
    oci: SocketAddr,
    cache_dir: PathBuf,
    _dir: TestDir,
}

impl<'a> Node<'a> {
    /// Starts the node. `local` is more keys of `[local_cache]`. The upstream wants its own key,
    /// so what the test seeds is put before this is called.
    fn start(fake: &'a FakeS3, name: &str, local: &str) -> Self {
        fake.require_credentials("UPKEY", "up-secret");
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let key = secret(&dir, "key", "AKIDCACHE\ncache-secret\n");
        let upstream_key = secret(&dir, "upstream.key", "UPKEY\nup-secret\n");
        let cache_dir = dir.path().join("cache");
        let file = format!(
            r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "oci-cache"
bucket          = "tier-bucket"
access_key_file = "{key}"

[local_cache]
path   = "{cache}"
budget = "8MiB"
{local}

[cold_store]
kind             = "s3"
bucket           = "kbf-fake"
endpoint         = "{endpoint}"
credentials_file = "{upstream_key}"

[oci]
listen = "127.0.0.1:0"

[[policy]]
prefix = "oci/"
cache  = true
"#,
            key = key.display(),
            cache = cache_dir.display(),
            endpoint = fake.endpoint(),
            upstream_key = upstream_key.display(),
        );
        let config = secret(&dir, "node.toml", &file);
        let mut command = Command::new(env!("CARGO_BIN_EXE_tierstore"));
        command
            .args(["--config", &config.display().to_string()])
            .args(["--drain-lb-wait", "50ms"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = spawn(&mut command, 3).expect("spawned");
        let stdout = child.stdout.take().expect("stdout");
        let (line, _reader) = first_line(stdout, Duration::from_secs(30)).expect("a start line");
        let words: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(words.len(), 7, "{line}");
        assert_eq!(words[3], "mode=read-cache", "{line}");
        assert_eq!(words[5], "(http)", "{line}");
        let commit = tierstore::server::build_info::COMMIT;
        assert_eq!(words[6], format!("commit={commit}"), "{line}");
        let s3 = words[2].strip_prefix("listen=").expect("listen=");
        let oci = words[4].strip_prefix("oci-listen=").expect("oci-listen=");
        Self {
            fake,
            child,
            s3: s3.to_owned(),
            oci: oci.parse().expect("an address"),
            cache_dir,
            _dir: dir,
        }
    }

    /// The text of `/metricz`.
    async fn metrics(&self) -> String {
        let url = format!("http://{}/metricz", self.s3);
        let reply = reqwest::get(url).await.expect("metricz");
        assert_eq!(reply.status(), 200);
        reply.text().await.expect("text")
    }

    /// The GETs the upstream has had of the blob `hex`.
    fn blob_gets(&self, hex: &str) -> usize {
        let wanted = format!("GET /kbf-fake/oci/blobs/sha256/{hex}");
        let requests = self.fake.requests();
        requests.iter().filter(|r| r.starts_with(&wanted)).count()
    }

    /// The GETs the upstream has had of anything.
    fn gets(&self) -> usize {
        let requests = self.fake.requests();
        requests.iter().filter(|r| r.starts_with("GET ")).count()
    }

    /// Pulls the blob `hex` on a connection of its own: the status and the bytes, or `None` if
    /// the connection was cut before the head of an answer.
    async fn pull(&self, hex: &str) -> Option<(u16, Vec<u8>, bool)> {
        let mut conn = Conn::open_plain(self.oci).await.expect("a connection");
        let path = format!("/v2/library/app/blobs/sha256:{hex}");
        let reply = conn.request("GET", &path, &[]).await?;
        Some((reply.status, reply.body, reply.short))
    }

    /// Pulls the blob `hex` and asserts it is whole, 200, and has the bytes `want`.
    async fn pull_whole(&self, hex: &str, want: &[u8]) {
        let (status, body, short) = self.pull(hex).await.expect("an answer");
        assert_eq!((status, short), (200, false));
        assert!(body == want, "the bytes differ from the upstream's");
        assert_eq!(hex_of(&body), hex);
    }

    /// Stops the node with SIGTERM and gives its exit status.
    async fn stop(mut self) -> Option<i32> {
        let pid = Pid::from_raw(i32::try_from(self.child.id()).expect("a pid")).expect("a pid");
        kill_process(pid, Signal::TERM).expect("signalled");
        let waiting = tokio::task::spawn_blocking(move || self.child.wait());
        waiting.await.expect("joined").expect("waited").code()
    }
}

/// Asserts that a build node with an `[oci]` table, over an upstream that holds a manifest and a
/// blob, serves the manifest and the blob by digest over plain HTTP with the bytes and the digest
/// the upstream has; that the first blob read is a miss that fills the cache from the upstream;
/// that the second read of the blob is a hit on the disk, with the hit counter up, no new fill
/// and no new GET at the upstream; that `/metricz` has the OCI block and the cache block; and
/// that the process exits with status 0 on SIGTERM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_node_serves_the_registry_from_its_cache() {
    let fake = FakeS3::start().await;
    let blob = pattern(300_000);
    let blob_hex = hex_of(&blob);
    let manifest_hex = hex_of(MANIFEST.as_bytes());
    seed(&fake, &blob_hex, &blob).await;
    seed(&fake, &manifest_hex, MANIFEST.as_bytes()).await;
    let node = Node::start(&fake, "oci-cache-node", "");

    let mut conn = Conn::open_plain(node.oci).await.expect("a connection");
    let path = format!("/v2/library/app/manifests/sha256:{manifest_hex}");
    let got = conn.request("GET", &path, &[]).await.expect("an answer");
    assert_eq!(got.status, 200);
    assert_eq!(got.body, MANIFEST.as_bytes());
    assert_eq!(
        got.must("docker-content-digest"),
        format!("sha256:{manifest_hex}")
    );

    let before = node.metrics().await;
    node.pull_whole(&blob_hex, &blob).await;
    assert!(node.blob_gets(&blob_hex) >= 1, "the upstream was not read");
    let filled = node.metrics().await;
    for name in [
        "tierstore_cache_fills_total",
        "tierstore_cache_misses_total",
    ] {
        assert!(counter(&filled, name) > counter(&before, name), "{name}");
    }

    let (blob_gets_before, gets_before) = (node.blob_gets(&blob_hex), node.gets());
    node.pull_whole(&blob_hex, &blob).await;
    let after = node.metrics().await;
    let hits = "tierstore_cache_hits_total";
    assert!(counter(&after, hits) > counter(&filled, hits));
    let fills = "tierstore_cache_fills_total";
    assert_eq!(counter(&after, fills), counter(&filled, fills));
    assert_eq!(node.blob_gets(&blob_hex), blob_gets_before);
    assert_eq!(node.gets(), gets_before);

    let ok = r#"tierstore_oci_requests_total{route="blob",outcome="ok"} 2"#;
    assert!(after.contains(ok), "{after}");
    assert!(
        after.contains("tierstore_oci_front_info{mode=\"http\"} 1\n"),
        "{after}"
    );
    assert!(node.cache_dir.join("obj").is_dir());

    drop(conn);
    assert_eq!(node.stop().await, Some(0));
}

/// Asserts what `whole_fill_limit` does to the registry front, with the limit lowered to 200000
/// bytes: a blob within it is filled whole by the first ranged read of the front and the second
/// read is a hit with no GET at the upstream; a blob over it is served whole, with the bytes and
/// the digest of the upstream, by a pass-through that keeps nothing (no fill, no hit, and a GET
/// at the upstream on every read); and a blob over it whose bytes are not its digest is still
/// cut short and counted as a digest mismatch, so the pass-through verifies as the cached path
/// does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blob_over_the_whole_fill_limit_is_passed_through_and_verified() {
    let fake = FakeS3::start().await;
    let small = pattern(100_000);
    let big = pattern(300_000);
    let (small_hex, big_hex) = (hex_of(&small), hex_of(&big));
    let rotted = hex_of(b"not the bytes below");
    seed(&fake, &small_hex, &small).await;
    seed(&fake, &big_hex, &big).await;
    seed(&fake, &rotted, &big).await;
    let node = Node::start(&fake, "oci-cache-limit", "whole_fill_limit = 200000");
    let fills = "tierstore_cache_fills_total";
    let hits = "tierstore_cache_hits_total";

    let start = node.metrics().await;
    node.pull_whole(&small_hex, &small).await;
    let first = node.metrics().await;
    assert_eq!(counter(&first, fills), counter(&start, fills) + 1);
    let gets_after_first = node.blob_gets(&small_hex);
    assert!(gets_after_first >= 1);
    node.pull_whole(&small_hex, &small).await;
    let second = node.metrics().await;
    assert!(counter(&second, hits) > counter(&first, hits));
    assert_eq!(counter(&second, fills), counter(&first, fills));
    assert_eq!(node.blob_gets(&small_hex), gets_after_first);

    node.pull_whole(&big_hex, &big).await;
    let once = node.blob_gets(&big_hex);
    assert!(once >= 1, "the upstream was not read");
    node.pull_whole(&big_hex, &big).await;
    assert!(node.blob_gets(&big_hex) > once, "a second read was kept");
    let passed = node.metrics().await;
    assert_eq!(counter(&passed, fills), counter(&second, fills));
    assert_eq!(counter(&passed, hits), counter(&second, hits));

    let mismatches = "tierstore_oci_digest_mismatches_total";
    assert_eq!(counter(&passed, mismatches), 0);
    match node.pull(&rotted).await {
        None => {}
        Some((_, _, short)) => assert!(short, "a blob that is not its digest was delivered whole"),
    }
    let cut = node.metrics().await;
    assert_eq!(counter(&cut, mismatches), 1);
    assert_eq!(counter(&cut, fills), counter(&second, fills));
    assert_eq!(node.stop().await, Some(0));
}

/// Asserts the refill recipe of the README: after the data file of a cached blob is removed from
/// the cache directory, the next pull of it is whole and has the bytes and the digest of the
/// upstream, is filled again from the upstream (one more fill, one more GET there), and the pull
/// after that is a hit again with no GET at the upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cached_blob_whose_file_is_removed_is_filled_again() {
    let fake = FakeS3::start().await;
    let blob = pattern(150_000);
    let hex = hex_of(&blob);
    seed(&fake, &hex, &blob).await;
    let node = Node::start(&fake, "oci-cache-refill", "");
    let fills = "tierstore_cache_fills_total";
    let hits = "tierstore_cache_hits_total";

    node.pull_whole(&hex, &blob).await;
    let kept = node.metrics().await;
    let gets_kept = node.blob_gets(&hex);
    // The file is named by the SHA-256 of the key, which is not the digest of the blob.
    let named = hex_of(format!("oci/blobs/sha256/{hex}").as_bytes());
    let file = node.cache_dir.join("obj").join(&named[..2]);
    let file = file.join(&named[2..4]).join(&named);
    assert!(file.is_file(), "the blob is not in the cache directory");

    std::fs::remove_file(&file).expect("removed");
    node.pull_whole(&hex, &blob).await;
    let refilled = node.metrics().await;
    assert_eq!(counter(&refilled, fills), counter(&kept, fills) + 1);
    assert!(
        node.blob_gets(&hex) > gets_kept,
        "the upstream was not read"
    );
    assert!(file.is_file(), "the blob was not written again");

    let gets_refilled = node.blob_gets(&hex);
    node.pull_whole(&hex, &blob).await;
    let hit = node.metrics().await;
    assert!(counter(&hit, hits) > counter(&refilled, hits));
    assert_eq!(counter(&hit, fills), counter(&refilled, fills));
    assert_eq!(node.blob_gets(&hex), gets_refilled);
    assert_eq!(node.stop().await, Some(0));
}

/// Asserts that a start whose OCI address is taken exits with status 1 and prints no start line,
/// so no client is told of an S3 listener that is not served.
#[test]
fn a_taken_oci_address_fails_the_start_of_the_process() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "oci-cache-taken");
    let key = secret(&dir, "key", "AKIDCACHE\ncache-secret\n");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bound");
    let busy = taken.local_addr().expect("an address");
    let file = format!(
        r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "oci-taken"
bucket          = "tier-bucket"
access_key_file = "{key}"

[local_cache]
path   = "{cache}"
budget = "1MiB"

[cold_store]
kind = "memory"

[oci]
listen = "{busy}"

[[policy]]
prefix = "oci/"
cache  = true
"#,
        key = key.display(),
        cache = dir.path().join("cache").display(),
    );
    let config = secret(&dir, "node.toml", &file);
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore"))
        .args(["--config", &config.display().to_string()])
        .output()
        .expect("ran");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "a start line was printed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&format!("binding {busy}")), "{stderr}");
}
