//! The `tierstore` binary as a process: its flags, its start line, its refusals, and its drain on
//! SIGTERM and SIGINT.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{ByteRange, Capabilities, ObjectKey, ObjectStore};
use kbf_testkit::reaped::{Reaped, first_line, spawn};
use reqwest::Client;
use rustix::process::{Pid, Signal, kill_process};

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
    fn store(&self) -> S3Store {
        let config = S3Config {
            endpoint: format!("http://{}", self.addr),
            region: "us-east-1".to_owned(),
            bucket: "tier-bucket".to_owned(),
            credentials: Credentials::new(BIN_KEY_ID, BIN_SECRET),
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
/// a key file that others can read, a tier that is not `memory`, water marks out of order,
/// an id that is not an id, and an address that is in use.
#[test]
fn a_start_that_cannot_go_on_says_why() {
    let files = Files::new("bin-refused");
    let good = files.flags(&[]);
    let (code, out, err) = run(&with(&good, "--hot", "s3://hot"));
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.starts_with("tierstore: "), "{err}");
    assert!(err.contains("--hot"), "{err}");
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
