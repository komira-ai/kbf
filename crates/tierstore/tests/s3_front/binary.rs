//! The `tierstore` binary as a process: its flags, its start line, its refusals, and its drain on
//! SIGTERM and SIGINT.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::s3::{Credentials, MULTIPART_THRESHOLD, S3Config, S3Store};
use kbf_objstore::{BodyStream, ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_testkit::reaped::{Reaped, first_line, spawn};
use reqwest::Client;
use rustix::process::{Pid, Signal, kill_process};

use crate::client::assert_refused;
use crate::files::{BIN_KEY_ID, BIN_SECRET, Files, with};

const BIN: &str = env!("CARGO_BIN_EXE_tierstore");

/// Runs the binary to its end: the exit status, stdout and stderr.
fn run(flags: &[String]) -> (i32, String, String) {
    let out = Command::new(BIN).args(flags).output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A running binary, and the address it said it listens on.
struct Running {
    child: Reaped,
    addr: String,
    /// Keeps the pipe of the binary's stdout open.
    _stdout: std::io::BufReader<std::process::ChildStdout>,
}

fn start(flags: &[String]) -> Running {
    let mut command = Command::new(BIN);
    command
        .args(flags)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = spawn(&mut command, 3).unwrap();
    let stdout = child.stdout.take().unwrap();
    let (line, reader) = first_line(stdout, Duration::from_secs(30)).unwrap();
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words[0], "tierstore", "{line}");
    assert_eq!(words[1], env!("CARGO_PKG_VERSION"), "{line}");
    let listen = words[2].strip_prefix("listen=").expect("listen=");
    Running {
        child,
        addr: listen.to_owned(),
        _stdout: reader,
    }
}

impl Running {
    /// The client of kbf with the key of the key file.
    fn store(&self) -> S3Store {
        self.store_as(BIN_KEY_ID, BIN_SECRET)
    }

    /// The client of kbf with this key.
    fn store_as(&self, key_id: &str, secret: &str) -> S3Store {
        let config = S3Config {
            endpoint: format!("http://{}", self.addr),
            region: "us-east-1".to_owned(),
            bucket: "tier-bucket".to_owned(),
            credentials: Credentials::new(key_id, secret),
            capabilities: Capabilities {
                conditional_put: true,
                object_lock: false,
            },
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            min_body_rate: None,
        };
        S3Store::new(config).unwrap()
    }

    fn signal(&self, signal: Signal) {
        let pid = Pid::from_raw(i32::try_from(self.child.id()).unwrap()).unwrap();
        kill_process(pid, signal).unwrap();
    }

    /// The exit code, once the process has exited.
    async fn exit_code(&mut self) -> Option<i32> {
        for _ in 0..1000 {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the process did not exit");
    }
}

/// Asserts that the binary prints one start line with its version and the address it is bound
/// to, serves S3 calls with the key of its key file, answers `/readyz` 200, and on SIGTERM
/// answers `/readyz` 503, goes on serving through the wait, and exits with status 0.
#[tokio::test]
async fn it_announces_serves_and_drains_on_sigterm() {
    let files = Files::new("bin-sigterm");
    let flags = files.flags(&[
        "--drain-lb-wait",
        "1500ms",
        "--reconcile-interval",
        "100ms",
        "--lease-ttl",
        "1s",
    ]);
    let mut running = start(&flags);
    let store = running.store();
    let k = ObjectKey::new("cas/bin").unwrap();
    store
        .put_new(&k, Bytes::from_static(b"served by the binary"), None)
        .await
        .unwrap();
    let first = ByteRange::new(0, 6).unwrap();
    assert_eq!(store.get_range(&k, first).await.unwrap(), "served");
    let ready = format!("http://{}/readyz", running.addr);
    let probe = Client::new().get(&ready).send().await.unwrap();
    assert_eq!(probe.status(), 200);

    running.signal(Signal::TERM);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let probe = Client::new().get(&ready).send().await.unwrap();
    assert_eq!(probe.status(), 503);
    let still = ByteRange::new(7, 2).unwrap();
    assert_eq!(store.get_range(&k, still).await.unwrap(), "by");
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that SIGINT starts the same drain and ends the process with status 0.
#[tokio::test]
async fn it_drains_on_sigint_too() {
    let files = Files::new("bin-sigint");
    let mut running = start(&files.flags(&["--drain-lb-wait", "50ms"]));
    running.signal(Signal::INT);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that `--help` and `--version` exit 0 and print the flags and the version, and that
/// no flags, a flag that is not known, and a value that is not valid exit 2 with a usage text.
#[test]
fn the_flags_are_parsed_and_refused_as_the_usage_says() {
    let (code, out, _) = run(&["--help".to_owned()]);
    assert_eq!(code, 0);
    let named = [
        "--listen",
        "--access-key-file",
        "--drain-deadline",
        "--hot-bytes",
    ];
    for flag in named {
        assert!(out.contains(flag), "{flag}");
    }
    let (code, out, _) = run(&["--version".to_owned()]);
    assert_eq!(code, 0);
    let version = format!("tierstore {}", env!("CARGO_PKG_VERSION"));
    assert_eq!(out.trim_end(), version);
    let (code, _, err) = run(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("--listen"), "{err}");
    let files = Files::new("bin-flags");
    let mut unknown = files.flags(&[]);
    unknown.push("--no-such-flag".to_owned());
    assert_eq!(run(&unknown).0, 2);
    let bad_value = files.flags(&["--drain-lb-wait", "soon"]);
    assert_eq!(run(&bad_value).0, 2);
}

/// Asserts that a start that cannot go on exits with status 1 and a reason that names the flag:
/// a key file that others can read, a tier that names a bucket and says nothing of the store it
/// is in, water marks out of order, an id that is not an id, and an address that is in use.
#[test]
fn a_start_that_cannot_go_on_says_why() {
    let files = Files::new("bin-refused");
    let good = files.flags(&[]);
    let (code, out, err) = run(&with(&good, "--hot", "s3://hot"));
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.starts_with("tierstore: "), "{err}");
    assert!(err.contains("--hot"), "{err}");
    let (code, _, err) = run(&with(&good, "--cold", "tape"));
    assert_eq!(code, 1);
    assert!(err.contains("--cold"), "{err}");
    let (code, _, err) = run(&files.flags(&["--low-water", "90"]));
    assert_eq!(code, 1);
    assert!(err.contains("--low-water"), "{err}");
    let (code, _, err) = run(&with(&good, "--proxy-id", "a/b"));
    assert_eq!(code, 1);
    assert!(err.contains("opening the proxy"), "{err}");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let (code, _, err) = run(&with(&good, "--listen", &addr));
    assert_eq!(code, 1);
    assert!(err.contains("binding"), "{err}");
    drop(taken);
    std::fs::set_permissions(&files.key, Permissions::from_mode(0o644)).unwrap();
    let (code, _, err) = run(&good);
    assert_eq!(code, 1);
    assert!(err.contains("others can read it"), "{err}");
}

/// Asserts that the binary serves a read-only key given as a JSON file beside the full key of a
/// file of two lines: the read-only key reads a ranged get of what the full key put, and is
/// refused a put and a delete with 403 `AccessDenied`.
#[tokio::test]
async fn it_serves_a_read_only_key_from_a_json_file_beside_the_full_key() {
    let files = Files::new("bin-read-only");
    let json = r#"{"access_key_id": "AKIDREADER", "secret_access_key": "reader-secret"}"#;
    let reader_file = files.secret("reader.json", json);
    let reader_flag = reader_file.display().to_string();
    let flags = files.flags(&[
        "--read-only-key-file",
        reader_flag.as_str(),
        "--drain-lb-wait",
        "50ms",
    ]);
    let mut running = start(&flags);
    let full = running.store();
    let reader = running.store_as("AKIDREADER", "reader-secret");
    let k = ObjectKey::new("cas/ro").unwrap();
    let body = Bytes::from_static(b"read by the daemons");
    full.put_new(&k, body, None).await.unwrap();
    let first = ByteRange::new(0, 4).unwrap();
    assert_eq!(reader.get_range(&k, first).await.unwrap(), "read");
    let other = ObjectKey::new("cas/other").unwrap();
    let put = reader.put_new(&other, Bytes::from_static(b"x"), None).await;
    assert_refused(put, 403, "AccessDenied");
    let delete = reader.delete(&k).await;
    assert_refused(delete, 403, "AccessDenied");
    assert_eq!(full.get_range(&k, first).await.unwrap(), "read");
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that two keys with one id, the full key's and a read-only key's, are refused at start
/// with status 2, as flags that cannot be mended by a restart are, with a message that names the
/// id and the flags and not the secret; that a read-only key file that is not a key (a JSON
/// object with a field besides the two) or that others can read exits 1 and names the flag, and
/// not the secret.
#[test]
fn a_read_only_key_file_that_cannot_be_used_is_a_start_that_says_why() {
    let files = Files::new("bin-read-only-refused");
    let same = files.secret("same", &format!("{BIN_KEY_ID}\nanother-secret\n"));
    let flag = |path: &std::path::Path| {
        let shown = path.display().to_string();
        files.flags(&["--read-only-key-file", shown.as_str()])
    };
    let (code, out, err) = run(&flag(same.as_path()));
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.starts_with("tierstore: "), "{err}");
    assert!(err.contains(BIN_KEY_ID), "{err}");
    assert!(err.contains("--read-only-key-file"), "{err}");
    assert!(!err.contains("another-secret"), "{err}");
    let extra = r#"{"access_key_id": "A", "secret_access_key": "topsecret", "note": 1}"#;
    let typo = files.secret("typo", extra);
    let (code, _, err) = run(&flag(typo.as_path()));
    assert_eq!(code, 1);
    assert!(err.contains("--read-only-key-file"), "{err}");
    assert!(!err.contains("topsecret"), "{err}");
    let open = files.secret("open", "READER\nreader-secret\n");
    std::fs::set_permissions(&open, Permissions::from_mode(0o644)).unwrap();
    let (code, _, err) = run(&flag(open.as_path()));
    assert_eq!(code, 1);
    assert!(err.contains("others can read it"), "{err}");
    assert!(!err.contains("reader-secret"), "{err}");
}

/// Asserts that the binary serves over two S3 tiers, each a bucket of a store of its own that it
/// reaches with the key file and the region of its tier: an object put is read back, is in the
/// hot store and then, copied by the reconciler, in the cold store; a multipart upload lands in
/// the cold store only, as one object made of the parts, with no upload left open; and the
/// requests of each tier were signed with the key and the region of that tier (each store
/// refuses another key); and the `ETag` a head answers is the tag in quotes once, for an
/// object put whole and for one made of parts, and an `If-Match` with it names the object's
/// tag (the answer is that objects cannot be replaced) where another tag is a failed
/// precondition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn it_serves_over_two_s3_tiers_and_lands_a_multipart_upload_in_the_cold_one() {
    let hot = FakeS3::start().await;
    let cold = FakeS3::start().await;
    hot.require_credentials("HOTKEY", "hot-secret");
    cold.require_credentials("COLDKEY", "cold-secret");
    let files = Files::new("bin-s3-tiers");
    let hot_key = files.secret("hot-key", "HOTKEY\nhot-secret\n");
    let cold_key = files.secret("cold-key", "COLDKEY\ncold-secret\n");
    let hot_key = hot_key.display().to_string();
    let cold_key = cold_key.display().to_string();
    let base = files.flags(&[
        "--drain-lb-wait",
        "50ms",
        "--reconcile-interval",
        "100ms",
        "--lease-ttl",
        "1s",
        "--hot-endpoint",
        hot.endpoint(),
        "--hot-credentials-file",
        hot_key.as_str(),
        "--cold-endpoint",
        cold.endpoint(),
        "--cold-region",
        "eu-west-1",
        "--cold-credentials-file",
        cold_key.as_str(),
    ]);
    let flags = with(&base, "--hot", "s3:hot-bucket");
    let flags = with(&flags, "--cold", "s3:cold-bucket");
    let mut running = start(&flags);
    let store = running.store();

    let small = ObjectKey::new("cas/small").unwrap();
    let text = Bytes::from_static(b"small object");
    store.put_new(&small, text, None).await.unwrap();
    let all = ByteRange::new(0, 12).unwrap();
    let got = store.get_range(&small, all).await.unwrap();
    assert_eq!(got, "small object");
    let name = "cas/small".to_owned();
    assert!(hot.keys().contains(&name));
    for _ in 0..500 {
        if cold.keys().contains(&name) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(cold.keys().contains(&name), "not copied");

    let big = ObjectKey::new("cas/big").unwrap();
    let len = MULTIPART_THRESHOLD + 1;
    let bytes = Bytes::from(vec![9_u8; usize::try_from(len).unwrap()]);
    let body: BodyStream = stream::iter(vec![Ok(bytes)]).boxed();
    store.put_stream(&big, len, body).await.unwrap();
    assert_eq!(cold.object_len(&big), Some(len));
    assert!(!hot.keys().contains(&"cas/big".to_owned()));
    let completed = (cold.stats().completed, hot.stats().completed);
    assert_eq!(completed, (1, 0));
    assert_eq!(cold.incomplete_uploads(), 0);
    assert_eq!(store.head(&big).await.unwrap().size, len);
    let head = store.head(&big).await.unwrap();
    let inner = head.etag.trim_matches('"');
    assert_eq!(head.etag, format!("\"{inner}\""));
    let head = store.head(&small).await.unwrap();
    let inner = head.etag.trim_matches('"');
    assert_eq!(head.etag, format!("\"{inner}\""));
    let swap = store.put_if_match(&small, Bytes::new(), &head.etag);
    let same = swap.await.unwrap_err();
    assert!(
        matches!(same, ObjectStoreError::Service { status: 501, .. }),
        "{same:?}"
    );
    let swap = store.put_if_match(&small, Bytes::new(), "\"stale\"");
    let stale = swap.await.unwrap_err();
    assert!(
        matches!(stale, ObjectStoreError::Service { status: 412, .. }),
        "{stale:?}"
    );
    assert_eq!(hot.regions(), ["us-east-1"]);
    assert_eq!(cold.regions(), ["eu-west-1"]);

    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that a start with a tier that its flags do not describe exits with status 1 and a
/// reason that names the flag: an S3 tier with no endpoint, one whose credentials file is not
/// there, one whose bucket name is not a bucket name, and the flags of an S3 tier given for a
/// tier that is not one.
#[test]
fn an_s3_tier_that_its_flags_do_not_describe_is_a_start_that_says_why() {
    let files = Files::new("bin-s3-refused");
    let secret = files.secret("hot-key", "ID\nsecret\n");
    let key = secret.display().to_string();
    let no_endpoint = files.flags(&["--hot-credentials-file", key.as_str()]);
    let s3 = with(&no_endpoint, "--hot", "s3:hot-bucket");
    let (code, out, err) = run(&s3);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.starts_with("tierstore: "), "{err}");
    assert!(err.contains("--hot-endpoint"), "{err}");

    let described = files.flags(&[
        "--hot-credentials-file",
        key.as_str(),
        "--hot-endpoint",
        "http://127.0.0.1:1",
    ]);
    let flags = with(&described, "--hot", "s3:hot-bucket");
    let (code, _, err) = run(&with(&flags, "--hot-credentials-file", "/no/such/file"));
    assert_eq!(code, 1);
    assert!(err.contains("--hot-credentials-file"), "{err}");
    let (code, _, err) = run(&with(&flags, "--hot", "s3:Not_A_Bucket"));
    assert_eq!(code, 1);
    assert!(err.contains("bucket name"), "{err}");
    let (code, _, err) = run(&with(&flags, "--hot", "memory"));
    assert_eq!(code, 1);
    assert!(err.contains("s3:<bucket>"), "{err}");
}
