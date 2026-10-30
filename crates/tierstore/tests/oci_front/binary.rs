//! The binary's flags start the front: `run_serving` in this process, and the `tierstore`
//! process itself.

use std::fs::Permissions;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use clap::Parser;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{Capabilities, ObjectKey, ObjectStore};
use kbf_testkit::dir::TestDir;
use kbf_testkit::reaped::{first_line, spawn};
use rustix::process::{Pid, Signal, kill_process};
use tierstore::server::Drained;
use tierstore::server::cli::{Addresses, Cli, run_serving};
use tokio::sync::oneshot;

use crate::client::{Conn, connector};
use crate::pki::{Leaf, ca, ip, leaf};
use crate::rig::hex_of;

const KEY_ID: &str = "AKIDOCI";
const SECRET: &str = "oci-secret";

struct Files {
    _dir: TestDir,
    key: String,
    leaf: Leaf,
    ca_pem: String,
}

fn files(name: &str) -> Files {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let key = dir.path().join("key");
    std::fs::write(&key, format!("{KEY_ID}\n{SECRET}\n")).expect("key file");
    std::fs::set_permissions(&key, Permissions::from_mode(0o600)).expect("mode");
    let ca = ca();
    let leaf = leaf(dir.path(), "front", &ca, vec![ip("127.0.0.1")]);
    Files {
        key: key.display().to_string(),
        leaf,
        ca_pem: ca.pem,
        _dir: dir,
    }
}

impl Files {
    fn flags(&self, oci: bool) -> Vec<String> {
        let mut flags: Vec<String> = [
            "tierstore",
            "--listen",
            "127.0.0.1:0",
            "--proxy-id",
            "oci",
            "--hot",
            "memory",
            "--cold",
            "memory",
            "--hot-bytes",
            "1048576",
            "--bucket",
            "tier-bucket",
            "--access-key-file",
            self.key.as_str(),
            "--drain-lb-wait",
            "50ms",
        ]
        .map(str::to_owned)
        .to_vec();
        if oci {
            let more = [
                "--oci-listen",
                "127.0.0.1:0",
                "--oci-tls-cert",
                self.leaf.cert.to_str().expect("a path"),
                "--oci-tls-key",
                self.leaf.key.to_str().expect("a path"),
            ];
            flags.extend(more.map(str::to_owned));
        }
        flags
    }
}

fn store(addr: SocketAddr) -> S3Store {
    let config = S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: "tier-bucket".to_owned(),
        credentials: Credentials::new(KEY_ID, SECRET),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    S3Store::new(config).expect("a client")
}

/// Runs the server over `flags`, calls `work` with the addresses it is bound to, then stops it.
async fn serving<F, Fut>(flags: Vec<String>, work: F) -> Drained
where
    F: FnOnce(Addresses) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let cli = Cli::try_parse_from(flags).expect("flags");
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let running = run_serving(
        &cli,
        async move {
            let _ = stopped.await;
        },
        move |addresses| {
            let _ = told.send(addresses);
        },
    );
    let driving = async {
        work(bound.await.expect("bound")).await;
        stop.send(()).expect("running");
    };
    let (drained, ()) = tokio::join!(running, driving);
    drained.expect("a drain")
}

/// Asserts that a blob put through the S3 front with its full key is pulled by digest over
/// HTTPS from the OCI front of the same process; that `/metricz` on the S3 listener shows the
/// request and the bytes; and that a stop drains with nothing cut and closes the OCI socket.
#[tokio::test]
async fn an_object_put_over_s3_is_pulled_by_digest_over_https() {
    let files = files("oci-binary-run");
    let content = b"a layer put through the S3 front".repeat(100);
    let hex = hex_of(&content);
    let connector = connector(&files.ca_pem);
    let drained = serving(files.flags(true), |bound| async move {
        let oci = bound.oci.expect("the OCI front is bound");
        let key = ObjectKey::new(format!("oci/blobs/sha256/{hex}")).expect("a key");
        store(bound.s3)
            .put_new(&key, Bytes::from(content.clone()), None)
            .await
            .expect("put");
        let mut conn = Conn::open(oci, &connector).await.expect("a connection");
        let path = format!("/v2/library/app/blobs/sha256:{hex}");
        let reply = conn.request("GET", &path, &[]).await.expect("an answer");
        assert_eq!(reply.status, 200);
        assert!(reply.body == content);
        let metrics = reqwest::get(format!("http://{}/metricz", bound.s3))
            .await
            .expect("metricz");
        assert_eq!(metrics.status(), 200);
        let text = metrics.text().await.expect("text");
        let ok = r#"tierstore_oci_requests_total{route="blob",outcome="ok"} 1"#;
        assert!(text.contains(ok), "{text}");
        let bytes = format!("tierstore_oci_bytes_served_total {}\n", content.len());
        assert!(text.contains(&bytes), "{text}");
        assert!(
            text.contains("tierstore_oci_digest_mismatches_total 0\n"),
            "{text}"
        );
    })
    .await;
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that without the OCI flags no such listener is bound and `/metricz` says the front is
/// not enabled.
#[tokio::test]
async fn without_the_flags_there_is_no_front() {
    let files = files("oci-binary-off");
    let drained = serving(files.flags(false), |bound| async move {
        assert_eq!(bound.oci, None);
        let metrics = reqwest::get(format!("http://{}/metricz", bound.s3))
            .await
            .expect("metricz");
        assert_eq!(metrics.status(), 200);
        let text = metrics.text().await.expect("text");
        assert!(text.contains("not enabled"), "{text}");
        assert!(!text.contains("tierstore_oci_requests_total"));
    })
    .await;
    assert_eq!(drained, Drained { abandoned: 0 });
}

/// Asserts that the process prints the OCI address on its start line, serves HTTPS there, and
/// exits 0 on SIGTERM, and that a start without a usable certificate exits 2 and says why.
#[tokio::test]
async fn the_process_announces_the_front_and_refuses_a_bad_certificate() {
    let files = files("oci-binary-process");
    let bin = env!("CARGO_BIN_EXE_tierstore");
    let mut command = Command::new(bin);
    command
        .args(&files.flags(true)[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = spawn(&mut command, 3).expect("spawned");
    let stdout = child.stdout.take().expect("stdout");
    let (line, _reader) = first_line(stdout, Duration::from_secs(30)).expect("a start line");
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words.len(), 4, "{line}");
    assert!(words[2].starts_with("listen="), "{line}");
    let oci: SocketAddr = words[3]
        .strip_prefix("oci-listen=")
        .expect("oci-listen=")
        .parse()
        .expect("an address");
    let mut conn = Conn::open(oci, &connector(&files.ca_pem))
        .await
        .expect("a connection");
    assert_eq!(
        conn.request("GET", "/v2/", &[])
            .await
            .expect("an answer")
            .status,
        200
    );
    drop(conn);
    let pid = Pid::from_raw(i32::try_from(child.id()).expect("a pid")).expect("a pid");
    kill_process(pid, Signal::TERM).expect("signalled");
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .expect("joined");
    assert_eq!(status.expect("waited").code(), Some(0));

    let mut flags = files.flags(true);
    let at = flags
        .iter()
        .position(|f| f == "--oci-tls-cert")
        .expect("a flag");
    flags[at + 1] = files.key.clone();
    let out = Command::new(bin).args(&flags[1..]).output().expect("ran");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--oci-tls-cert"), "{stderr}");
}

/// Asserts that a start whose OCI address is taken fails with the address in the message, and
/// that the process exits with the status of a start that a restart may mend.
#[tokio::test]
async fn an_oci_address_in_use_fails_the_start() {
    let files = files("oci-binary-taken");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bound");
    let mut flags = files.flags(true);
    let at = flags
        .iter()
        .position(|f| f == "--oci-listen")
        .expect("a flag");
    flags[at + 1] = taken.local_addr().expect("an address").to_string();
    let cli = Cli::try_parse_from(flags).expect("flags");
    let started = run_serving(&cli, std::future::pending(), |_| {}).await;
    let why = started.expect_err("a failed start").to_string();
    assert!(why.starts_with("binding 127.0.0.1:"), "{why}");
}
