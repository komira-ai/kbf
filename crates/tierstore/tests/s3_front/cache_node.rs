//! The `tierstore` binary started from the file of a build node: a read cache over an S3
//! upstream that wants its own key, in a process.

use std::path::Path;

use bytes::Bytes;
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::{ByteRange, Capabilities, ObjectKey, ObjectStore};
use reqwest::Client;
use rustix::process::Signal;

use crate::binary::{run, start};
use crate::files::Files;

/// The file of a build node whose upstream is `endpoint`, read with the key file `upstream_key`,
/// with its cache in `cache_dir`.
fn node_file(files: &Files, endpoint: &str, upstream_key: &Path, cache_dir: &Path) -> String {
    let text = format!(
        r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "bin-cache"
bucket          = "tier-bucket"
access_key_file = "{key}"

[local_cache]
path   = "{cache}"
budget = "1MiB"

[cold_store]
kind             = "s3"
bucket           = "kbf-fake"
endpoint         = "{endpoint}"
credentials_file = "{upstream_key}"

[[policy]]
prefix = "cas/"
cache  = true
"#,
        key = files.key.display(),
        cache = cache_dir.display(),
        upstream_key = upstream_key.display(),
    );
    files.secret("node.toml", &text).display().to_string()
}

fn args(config: &str) -> Vec<String> {
    ["--config", config, "--drain-lb-wait", "50ms"]
        .map(str::to_owned)
        .to_vec()
}

/// Asserts that a build node serves an object it reads from the upstream with the upstream's
/// key (the upstream refuses any other), reads it once from the upstream however often it is
/// asked, answers a range of it from the disk, refuses a write with the upstream untouched,
/// shows the counters on `/metricz` without a signature, keeps the object in its directory, and
/// exits with status 0 on SIGTERM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_node_reads_through_an_upstream_and_refuses_writes() {
    let fake = FakeS3::start().await;
    let upstream = fake.store(Capabilities {
        conditional_put: true,
        object_lock: false,
    });
    let key = ObjectKey::new("cas/obj").unwrap();
    let body = Bytes::from((0..3000u32).map(|n| (n % 251) as u8).collect::<Vec<u8>>());
    upstream.put_new(&key, body.clone(), None).await.unwrap();
    fake.require_credentials("UPKEY", "up-secret");

    let files = Files::new("bin-cache-node");
    let upstream_key = files.secret("upstream.key", "UPKEY\nup-secret\n");
    let cache_dir = files.key.with_file_name("cache");
    let config = node_file(&files, fake.endpoint(), &upstream_key, &cache_dir);
    let mut running = start(&args(&config));
    let store = running.store();
    let gets = || {
        let requests = fake.requests();
        requests
            .iter()
            .filter(|r| r.starts_with("GET /kbf-fake/cas/obj"))
            .count()
    };

    let all = ByteRange::new(0, 3000).unwrap();
    assert_eq!(store.get_range(&key, all).await.unwrap(), body);
    assert_eq!(gets(), 1);
    assert_eq!(store.get_range(&key, all).await.unwrap(), body);
    let part = ByteRange::new(5, 10).unwrap();
    assert_eq!(
        store.get_range(&key, part).await.unwrap(),
        body.slice(5..15)
    );
    assert_eq!(gets(), 1);

    let other = ObjectKey::new("cas/new").unwrap();
    let put = store.put_new(&other, Bytes::from_static(b"x"), None).await;
    assert!(put.is_err());
    assert!(fake.object_len(&other).is_none());

    let metrics = Client::new()
        .get(format!("http://{}/metricz", running.addr()))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), 200);
    let text = metrics.text().await.unwrap();
    for line in [
        "tierstore_cache_hits_total 2\n",
        "tierstore_cache_misses_total 1\n",
        "tierstore_cache_fills_total 1\n",
        "tierstore_cache_bytes 3000\n",
    ] {
        assert!(text.contains(line), "{line} in {text}");
    }
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
    assert!(cache_dir.join("obj").is_dir());
}

/// Asserts the exit status of a build node that cannot start: 1 for a cache directory that is a
/// file and for a bucket that is the name of a probe path, with the keys of the file in the
/// message and not flags, and 2 for a file with both a hot store and a cache.
#[test]
fn a_build_node_that_cannot_start_says_why_and_exits_with_the_status_of_the_reason() {
    let files = Files::new("bin-cache-refused");
    let upstream_key = files.secret("upstream.key", "UPKEY\nup-secret\n");
    let in_the_way = files.secret("in-the-way", "a file");
    let good = node_file(&files, "http://127.0.0.1:1", &upstream_key, &in_the_way);
    let (code, out, err) = run(&args(&good));
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.contains("opening the local cache"), "{err}");

    let cache_dir = files.key.with_file_name("cache");
    let text = node_file(&files, "http://127.0.0.1:1", &upstream_key, &cache_dir);
    let named = std::fs::read_to_string(&text)
        .unwrap()
        .replace("tier-bucket", "metricz");
    let renamed = files.secret("metricz.toml", &named).display().to_string();
    let (code, _, err) = run(&args(&renamed));
    assert_eq!(code, 1);
    assert!(
        err.contains("server.bucket") && err.contains("probe path"),
        "{err}"
    );
    assert!(!err.contains("--bucket"), "{err}");

    let mixed = std::fs::read_to_string(&text).unwrap();
    let mixed = mixed.replacen(
        "[local_cache]",
        "[hot_store]\nkind = \"memory\"\nbudget = 1\n\n[local_cache]",
        1,
    );
    let mixed = files.secret("mixed.toml", &mixed).display().to_string();
    let (code, _, err) = run(&args(&mixed));
    assert_eq!(code, 2);
    assert!(err.contains("is refused with [hot_store]"), "{err}");
}
