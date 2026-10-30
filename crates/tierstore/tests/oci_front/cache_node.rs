//! The registry front of a build node, in the `tierstore` process: a manifest and a blob that sit
//! in the upstream are pulled by digest over plain HTTP, and the second read of the blob is
//! answered from the cache's disk.

use std::fs::Permissions;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::{Capabilities, ObjectKey, ObjectStore};
use kbf_testkit::dir::TestDir;
use kbf_testkit::reaped::{first_line, spawn};
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
fn secret(dir: &TestDir, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).expect("a file");
    std::fs::set_permissions(&path, Permissions::from_mode(0o600)).expect("a mode");
    path
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
    let upstream = fake.store(Capabilities {
        conditional_put: true,
        object_lock: false,
    });
    let blob = pattern(300_000);
    let blob_hex = hex_of(&blob);
    let manifest_hex = hex_of(MANIFEST.as_bytes());
    for (hex, bytes) in [(&blob_hex, blob.clone()), (&manifest_hex, MANIFEST.into())] {
        let key = ObjectKey::new(format!("oci/blobs/sha256/{hex}")).expect("a key");
        upstream
            .put_new(&key, Bytes::from(bytes), None)
            .await
            .expect("put");
    }
    fake.require_credentials("UPKEY", "up-secret");

    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "oci-cache-node");
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
    assert_eq!(words.len(), 6, "{line}");
    assert_eq!(words[3], "mode=read-cache", "{line}");
    assert_eq!(words[5], "(http)", "{line}");
    let s3 = words[2].strip_prefix("listen=").expect("listen=");
    let oci: SocketAddr = words[4]
        .strip_prefix("oci-listen=")
        .expect("oci-listen=")
        .parse()
        .expect("an address");

    let metrics = || async {
        let reply = reqwest::get(format!("http://{s3}/metricz"))
            .await
            .expect("metricz");
        assert_eq!(reply.status(), 200);
        reply.text().await.expect("text")
    };
    let blob_gets = || {
        let wanted = format!("GET /kbf-fake/oci/blobs/sha256/{blob_hex}");
        let requests = fake.requests();
        requests.iter().filter(|r| r.starts_with(&wanted)).count()
    };
    let gets = || {
        let requests = fake.requests();
        requests.iter().filter(|r| r.starts_with("GET ")).count()
    };

    let mut conn = Conn::open_plain(oci).await.expect("a connection");
    let path = format!("/v2/library/app/manifests/sha256:{manifest_hex}");
    let got = conn.request("GET", &path, &[]).await.expect("an answer");
    assert_eq!(got.status, 200);
    assert_eq!(got.body, MANIFEST.as_bytes());
    assert_eq!(
        got.must("docker-content-digest"),
        format!("sha256:{manifest_hex}")
    );

    let before = metrics().await;
    let path = format!("/v2/library/app/blobs/sha256:{blob_hex}");
    let first = conn.request("GET", &path, &[]).await.expect("an answer");
    assert_eq!(first.status, 200);
    assert!(
        first.body == blob,
        "the blob read first differs from the upstream's"
    );
    assert_eq!(hex_of(&first.body), blob_hex);
    assert!(
        blob_gets() >= 1,
        "the first read did not go to the upstream"
    );
    let filled = metrics().await;
    for name in [
        "tierstore_cache_fills_total",
        "tierstore_cache_misses_total",
    ] {
        assert!(counter(&filled, name) > counter(&before, name), "{name}");
    }

    let (blob_gets_before, gets_before) = (blob_gets(), gets());
    let second = conn.request("GET", &path, &[]).await.expect("an answer");
    assert_eq!(second.status, 200);
    assert!(
        second.body == blob,
        "the blob read second differs from the upstream's"
    );
    assert_eq!(hex_of(&second.body), blob_hex);
    let after = metrics().await;
    let hits = "tierstore_cache_hits_total";
    assert!(counter(&after, hits) > counter(&filled, hits));
    assert_eq!(
        counter(&after, "tierstore_cache_fills_total"),
        counter(&filled, "tierstore_cache_fills_total")
    );
    assert_eq!(blob_gets(), blob_gets_before);
    assert_eq!(gets(), gets_before);

    let ok = r#"tierstore_oci_requests_total{route="blob",outcome="ok"} 2"#;
    assert!(after.contains(ok), "{after}");
    assert!(
        after.contains("tierstore_oci_front_info{mode=\"http\"} 1\n"),
        "{after}"
    );
    assert!(cache_dir.join("obj").is_dir());

    drop(conn);
    let pid = Pid::from_raw(i32::try_from(child.id()).expect("a pid")).expect("a pid");
    kill_process(pid, Signal::TERM).expect("signalled");
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("joined");
    assert_eq!(status.expect("waited").code(), Some(0));
}
