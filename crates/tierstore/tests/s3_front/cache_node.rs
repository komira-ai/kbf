//! The `tierstore` binary started from the file of a build node: a read cache over an S3
//! upstream that wants its own key, in a process.

use std::path::Path;

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::{ByteRange, Capabilities, ObjectKey, ObjectStore};
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

/// The file of a build node with `extra` lines in `[local_cache]`, over an upstream at `endpoint`
/// that the check must not call, with the cache in `cache_dir`.
fn check_file(files: &Files, extra: &str, endpoint: &str, cache_dir: &Path) -> String {
    let upstream_key = files.secret("upstream.key", "UPKEY\nup-secret\n");
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
{extra}

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
    files.secret("check.toml", &text).display().to_string()
}

fn checking(config: &str) -> Vec<String> {
    ["--config", config, "--check-config"]
        .map(str::to_owned)
        .to_vec()
}

/// Asserts that `--check-config` of a good build-node file with `write_through` prints nothing
/// and exits 0, and that it does not make the cache directory: it reads the file and the key files
/// and does no more.
#[test]
fn check_config_accepts_a_good_write_through_file_and_prints_nothing() {
    let files = Files::new("bin-check-good");
    let cache_dir = files.key.with_file_name("cache");
    let config = check_file(
        &files,
        "write_through = true",
        "http://192.0.2.1:9300",
        &cache_dir,
    );
    assert_eq!(run(&checking(&config)), (0, String::new(), String::new()));
    assert!(!cache_dir.exists());
}

/// Asserts that `--check-config` makes no call to the network: the upstream of the file is an
/// address nothing listens on (a port that was bound and let go), and the check still exits 0,
/// where a start of the same file with `write_through` cannot reach it.
#[test]
fn check_config_does_not_call_the_upstream() {
    let files = Files::new("bin-check-offline");
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let cache_dir = files.key.with_file_name("cache");
    let config = check_file(&files, "write_through = true", &endpoint, &cache_dir);
    assert_eq!(run(&checking(&config)), (0, String::new(), String::new()));
    let (code, _, _) = run(&["--config".to_owned(), config]);
    assert_eq!(code, 1);
}

/// Asserts that a file a start refuses is refused by `--check-config` with the same message and
/// status 2: a key the file must not have, and a key file that is not there.
#[test]
fn check_config_refuses_what_a_start_refuses_with_the_same_message() {
    let files = Files::new("bin-check-bad");
    let cache_dir = files.key.with_file_name("cache");
    let endpoint = "http://192.0.2.1:9300";
    let unknown = check_file(&files, "no_such_key = 1", endpoint, &cache_dir);
    let (code, _, said) = run(&checking(&unknown));
    assert_eq!(code, 2);
    assert!(said.contains("no_such_key"), "{said}");
    let (code, _, started) = run(&["--config".to_owned(), unknown]);
    assert_eq!(code, 2);
    assert_eq!(said, started);

    let good = check_file(&files, "", endpoint, &cache_dir);
    let gone = std::fs::read_to_string(&good).unwrap();
    let gone = gone.replace(&files.key.display().to_string(), "/no/such/access.key");
    let gone = files.secret("gone.toml", &gone).display().to_string();
    let (code, out, said) = run(&checking(&gone));
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(said.contains("/no/such/access.key"), "{said}");
}

/// Asserts that `--check-config` without `--config` is a usage error: status 2, and a message
/// that names the flag it needs when every other flag is given.
#[test]
fn check_config_without_a_file_is_a_usage_error() {
    let (code, _, said) = run(&["--check-config".to_owned()]);
    assert_eq!(code, 2);
    assert!(!said.is_empty());
    let files = Files::new("bin-check-usage");
    let mut flags = files.flags(&[]);
    flags.push("--check-config".to_owned());
    let (code, _, said) = run(&flags);
    assert_eq!(code, 2);
    assert!(said.contains("--config"), "{said}");
}

/// The file of a storage node with an `[oci]` table holding `oci`.
fn oci_file(files: &Files, oci: &str) -> String {
    let text = format!(
        r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "bin-oci"
bucket          = "tier-bucket"
access_key_file = "{key}"

[hot_store]
kind   = "memory"
budget = 1048576

[cold_store]
kind = "memory"

[oci]
{oci}
"#,
        key = files.key.display(),
    );
    files.secret("oci.toml", &text).display().to_string()
}

/// Asserts that `--check-config` of a storage-node file with a valid `[oci]` table passes with
/// no output and status 0, on a loopback address and on an address that is not loopback (where a
/// start warns that it serves plain HTTP, and the check does not), and that it binds nothing: the
/// OCI address is held by a listener of the test, which a check that bound it would have found in
/// use.
#[test]
fn check_config_accepts_a_storage_node_with_oci_without_a_warning_or_a_bind() {
    let files = Files::new("bin-check-oci-good");
    let held = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = held.local_addr().unwrap().port();
    for listen in [format!("127.0.0.1:{port}"), format!("0.0.0.0:{port}")] {
        let config = oci_file(&files, &format!("listen = \"{listen}\""));
        assert_eq!(
            run(&checking(&config)),
            (0, String::new(), String::new()),
            "{listen}"
        );
    }
    drop(held);
}

/// Asserts that `--check-config` refuses an `[oci]` table a start refuses, with the start's
/// message and status 2: a certificate file that is not there, and a prefix that overlaps
/// `_journal/`; and a certificate without its key.
#[test]
fn check_config_refuses_a_bad_oci_table_with_the_message_of_a_start() {
    let files = Files::new("bin-check-oci-bad");
    let key = files.secret("oci.key", "not a key\n").display().to_string();
    let cases = [
        (
            format!(
                "listen = \"127.0.0.1:0\"\ntls_cert_file = \"/no/such/cert.pem\"\ntls_key_file = \"{key}\""
            ),
            "/no/such/cert.pem",
        ),
        (
            "listen = \"127.0.0.1:0\"\nprefix = \"_journal/x/\"".to_owned(),
            "overlaps",
        ),
        (
            format!("listen = \"127.0.0.1:0\"\ntls_cert_file = \"{key}\""),
            "tls_key_file",
        ),
    ];
    for (oci, want) in cases {
        let config = oci_file(&files, &oci);
        let (code, out, said) = run(&checking(&config));
        assert_eq!((code, out.as_str()), (2, ""), "{oci}");
        assert!(said.contains(want), "{want}: {said}");
        let (_, _, started) = run(&["--config".to_owned(), config]);
        assert_eq!(said, started, "{oci}");
    }
}
