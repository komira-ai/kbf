//! What the proxy tests share (`proxy_soak.rs`, `proxy_pool.rs`, `ui_through_proxy.rs`): real
//! `kbf` children (this test binary run through a link named `kbf`), the TLS h2 client of the
//! proxy, the `Execute` streams a Buck2 build holds open, and reads of a child's own count
//! of them. Each test says in its own header what it proves with them.

use std::io::{BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{ChildStdout, Command, ExitCode, Stdio};
use std::time::Duration;

use bytes::Bytes;
use h2::RecvStream;
use h2::client::SendRequest;
use kbf_proto::reapi::batch_update_blobs_request::Request as BlobRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    Action, BatchUpdateBlobsRequest, Command as ActionCommand, Digest, ExecuteRequest,
};
use kbf_testkit::reaped::{self, Reaped};
use prost::Message as _;
use sha2::{Digest as _, Sha256};

/// The operator token the proxy reads at start; `/metrics` wants it as a bearer.
pub const OPERATOR_TOKEN: &str = "operator-token-0123456789-abcdefghijkl";

// This binary answers as `kbf` when it is run through a link of that name.
const _: () = {
    extern "C" fn kbf_hook() {
        let argv0 = std::env::args_os().next().unwrap_or_default();
        if Path::new(&argv0).file_name().is_some_and(|n| n == "kbf") {
            let code = kbf_cli::main();
            std::process::exit(i32::from(code != ExitCode::SUCCESS));
        }
    }
    #[used]
    #[unsafe(link_section = ".init_array")]
    static KBF_HOOK: extern "C" fn() = kbf_hook;
};

/// A `kbf` child that has printed its start line; stopped and reaped when dropped (SIGTERM,
/// then SIGKILL after 3 s), and holding only descriptors 0 to 2.
pub struct Child {
    process: Reaped,
    line: String,
    // Held open: a child that wrote to a closed pipe would die.
    _stdout: BufReader<ChildStdout>,
}

/// `args` (a mode word, then its flags) with `--config=` pointing at an empty file of the
/// test's own directory. A `kbf` child with no `--config` reads `/etc/kbf/kbf.toml`, so a
/// host that has one (or one that denies the read) would decide what a soak serves.
pub fn own_config(dir: &Path, args: &[String]) -> Vec<String> {
    if args.iter().any(|a| a.starts_with("--config")) {
        return args.to_vec();
    }
    let mut own = args.to_vec();
    own.insert(1, kbf_testkit::config::flag(dir));
    own
}

/// The path of the `kbf` link [`link_kbf`] makes in `dir`.
pub fn kbf_path(dir: &Path) -> std::path::PathBuf {
    dir.join("bin/kbf")
}

/// Links this test binary as `bin/kbf` under `dir`: run through that name it answers as `kbf`.
pub fn link_kbf(dir: &Path) {
    std::fs::create_dir_all(dir.join("bin")).expect("scratch");
    std::os::unix::fs::symlink(std::env::current_exe().expect("this binary"), kbf_path(dir))
        .expect("link kbf");
}

/// The command that starts `kbf` from the link in `dir` with `args` and a config of the
/// test's own ([`own_config`]).
pub fn kbf_command(dir: &Path, args: &[String]) -> Command {
    let mut command = Command::new(kbf_path(dir));
    command.args(own_config(dir, args));
    command
}

impl Child {
    pub fn start(dir: &Path, name: &str, args: &[String]) -> Self {
        let log = std::fs::File::create(dir.join(format!("{name}.log"))).expect("log file");
        let mut command = kbf_command(dir, args);
        command.stdout(Stdio::piped()).stderr(log);
        let mut process = reaped::spawn(&mut command, 3).expect("spawn kbf");
        let (line, stdout) = reaped::first_line(
            process.stdout.take().expect("stdout"),
            Duration::from_secs(60),
        )
        .unwrap_or_else(|e| panic!("no start line from {name}: {e}"));
        Self {
            process,
            line,
            _stdout: stdout,
        }
    }

    /// The `key=` field of the start line.
    pub fn field(&self, key: &str) -> String {
        self.line
            .split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} in the start line {:?}", self.line))
            .to_owned()
    }

    /// SIGTERM, then the exit status within 20 s.
    pub fn stop(&mut self) -> Option<std::process::ExitStatus> {
        let pid = rustix::process::Pid::from_raw(self.process.id().cast_signed()).expect("pid");
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        for _ in 0..400 {
            if let Ok(Some(status)) = self.process.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

/// One `GET` on a child's operator API: the status code and the body.
pub fn api_get(api: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(api).expect("connect to the API");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n"
    )
    .expect("send");
    let mut text = String::new();
    stream.read_to_string(&mut text).expect("read");
    let (head, body) = text.split_once("\r\n\r\n").expect("a head and a body");
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok());
    (code.expect("a status code"), body.to_owned())
}

/// `future`, or a failure naming `what` if it takes more than a minute: a hang is a red test.
pub async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .expect(what)
}

/// Waits (at most 30 s) until `done` holds.
pub async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let mut met = done();
    for _ in 0..3_000 {
        if met {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        met = done();
    }
    assert!(met, "{what}");
}

/// The ports of two members whose bases are ten apart, as the proxy has them: REAPI, worker,
/// operator and admin calls of each.
const OFFSETS: [u16; 8] = [0, 1, 2, 3, 10, 11, 12, 13];

/// The claims of this process, kept until it exits (a lock lives as long as its file).
static CLAIMED: std::sync::Mutex<Vec<std::fs::File>> = std::sync::Mutex::new(Vec::new());

/// A base port such that the REAPI, worker, operator and admin ports of the first member (the
/// base plus 0 to 3) and of a second (plus 10 to 13), as the proxy has them, are free now and
/// stay claimed against every other test process using this function.
///
/// Probing alone is not enough: the probe's sockets are closed before the children bind, and
/// nextest runs each test in its own process, so two tests whose pid-derived bases are a few
/// apart (their port sets overlap) probe the same free ports and then one child finds its port
/// taken and exits. Each port is therefore also claimed
/// by an exclusive lock on a file named for it, held until this process exits. The bases stay
/// below the ephemeral range, so no outgoing connection takes one of them either.
pub fn free_base() -> u16 {
    const FIRST: u16 = 20_000;
    const SPAN: u16 = 12_000;
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("proxy-ports");
    std::fs::create_dir_all(&dir).expect("the port claims' directory");
    let mut base = FIRST + (std::process::id() % u32::from(SPAN)) as u16;
    loop {
        let mut claims = Vec::new();
        for o in OFFSETS {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(format!("{}.lock", base + o)))
                .expect("a port's claim file");
            if file.try_lock().is_ok() {
                claims.push(file);
            } else {
                break;
            }
        }
        let bound: Vec<_> = OFFSETS
            .iter()
            .filter_map(|o| TcpListener::bind((Ipv4Addr::LOCALHOST, base + o)).ok())
            .collect();
        if claims.len() == OFFSETS.len() && bound.len() == OFFSETS.len() {
            CLAIMED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(claims);
            return base;
        }
        base += 20;
        if base + 13 >= FIRST + SPAN {
            base = FIRST;
        }
    }
}

pub fn storage_node(dir: &Path, name: &str) -> Child {
    Child::start(
        dir,
        name,
        &[
            "storage".to_owned(),
            "--storage-listen=127.0.0.1:0".to_owned(),
            "--cold-store-floor-bytes=1".to_owned(),
            format!(
                "--cold-store=file://{}",
                dir.join(name).join("cold").display()
            ),
            format!("--meta-dir={}", dir.join(name).join("meta").display()),
        ],
    )
}

/// The arguments of a `kbf frontend` on the ports from `base`, in front of the storage node at
/// `storage` (its `host:port`), keeping its state in the SQLite file `state` (a frontend starts
/// only over a state store, and a file is held by one process, so each frontend has its own).
pub fn frontend_args(storage: &str, base: u16, state: &Path) -> Vec<String> {
    vec![
        "frontend".to_owned(),
        format!("--listen=127.0.0.1:{base}"),
        format!("--worker-listen=127.0.0.1:{}", base + 1),
        format!("--api-listen=127.0.0.1:{}", base + 2),
        format!("--storage=http://{storage}"),
        format!("--state-store=sqlite://{}", state.display()),
    ]
}

/// The proxy's configuration for the one member `a` whose first port is `base` (a test of
/// more members adds them, ten ports apart): TLS from a CA and a server certificate written
/// under `dir/pki`, and the operator token file the proxy reads at start (0600, at least 32
/// characters).
pub fn proxy_config(
    dir: &Path,
    base: u16,
) -> (
    kbf_proxy::config::ProxyConfig,
    kbf_integration_test::pki::PkiFiles,
) {
    let pki =
        kbf_integration_test::pki::write(&dir.join("pki"), "soak-client").expect("certificates");
    let token = dir.join("operator.token");
    std::fs::write(&token, OPERATOR_TOKEN).expect("token");
    std::fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("chmod");
    let config = kbf_proxy::config::ProxyConfig::from_toml_str(&format!(
        "operator_token_file = {token:?}\n\
         server_cert_file = {:?}\n\
         server_key_file = {:?}\n\
         client_ca_file = {:?}\n\
         proxy_auth_file = {:?}\n\
         [[frontend]]\nname = \"a\"\nbase_port = {base}\n",
        pki.server_cert,
        pki.server_key,
        pki.ca_cert,
        dir.join("proxy-auth"),
    ))
    .expect("a valid table");
    (config, pki)
}

/// One gRPC message: the 5-byte prefix and the encoded message.
pub fn framed(message: &impl prost::Message) -> Bytes {
    let body = message.encode_to_vec();
    let mut out = vec![0_u8];
    out.extend((body.len() as u32).to_be_bytes());
    out.extend(body);
    Bytes::from(out)
}

pub fn grpc_request(path: &str) -> http::Request<()> {
    http::Request::builder()
        .method("POST")
        .uri(format!("http://frontend.test{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .expect("a request")
}

/// A TLS h2 client of the proxy, trusting the CA in `ca_pem` (no client certificate: the
/// proxy's default `buck2_auth = "none"` serves an anonymous caller).
pub async fn connect(
    addr: &str,
    ca_pem: &Path,
) -> (SendRequest<Bytes>, tokio::task::JoinHandle<()>) {
    use rustls::pki_types::pem::PemObject as _;
    let mut roots = rustls::RootCertStore::empty();
    for der in rustls::pki_types::CertificateDer::pem_file_iter(ca_pem).expect("CA file") {
        roots.add(der.expect("CA PEM")).expect("root");
    }
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").expect("name"),
            tcp,
        )
        .await
        .expect("TLS handshake");
    let (client, connection) = h2::client::Builder::new()
        .initial_connection_window_size(64 << 20)
        .handshake::<_, Bytes>(tls)
        .await
        .expect("h2 handshake");
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    (client, driver)
}

pub fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as i64,
    }
}

/// The actions `(tag, 0..count)` and the blobs that make them: each action is a command of
/// its own, over an empty input root. Returns the `Execute` requests and the blobs to store.
pub fn actions(tag: &str, count: usize) -> (Vec<ExecuteRequest>, Vec<BlobRequest>) {
    let empty = digest_of(&[]);
    let mut blobs = vec![BlobRequest {
        digest: Some(empty.clone()),
        data: Vec::new(),
        ..BlobRequest::default()
    }];
    let mut executes = Vec::new();
    for n in 0..count {
        let command = ActionCommand {
            arguments: vec!["true".to_owned(), format!("{tag}-{n}")],
            ..ActionCommand::default()
        }
        .encode_to_vec();
        let action = Action {
            command_digest: Some(digest_of(&command)),
            input_root_digest: Some(empty.clone()),
            do_not_cache: true,
            ..Action::default()
        }
        .encode_to_vec();
        executes.push(ExecuteRequest {
            action_digest: Some(digest_of(&action)),
            skip_cache_lookup: true,
            ..ExecuteRequest::default()
        });
        for data in [command, action] {
            blobs.push(BlobRequest {
                digest: Some(digest_of(&data)),
                data,
                ..BlobRequest::default()
            });
        }
    }
    (executes, blobs)
}

/// Stores `blobs` in the store behind the frontend at `reapi`, straight to it (not through
/// the proxy), and checks that every one was accepted.
pub async fn store(reapi: &str, blobs: Vec<BlobRequest>) {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{reapi}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect to a frontend");
    let sent = blobs.len();
    let response = ContentAddressableStorageClient::new(channel)
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: blobs,
            ..BatchUpdateBlobsRequest::default()
        })
        .await
        .expect("BatchUpdateBlobs")
        .into_inner();
    assert_eq!(response.responses.len(), sent);
    let refused: Vec<_> = response
        .responses
        .iter()
        .filter(|r| r.status.as_ref().is_some_and(|s| s.code != 0))
        .collect();
    assert!(refused.is_empty(), "blobs refused: {refused:?}");
}

/// The Execute streams a frontend has open, from its own `GET /v1/frontend/drain`.
pub fn open_executions(api: &str) -> u64 {
    let (code, body) = api_get(api, "/v1/frontend/drain");
    assert_eq!(code, 200, "{body}");
    let status: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    status["open_executions"].as_u64().expect("open_executions")
}

/// An `Execute` that has its response head and its first message, and is then left alone:
/// the stream stays open until it is dropped, which resets it.
pub struct Open {
    body: RecvStream,
}

impl Open {
    /// Starts `request` through `client`. The `Err` is the gRPC status of a refusal that
    /// came in the headers (a trailers-only answer) or the transport error.
    pub async fn start(
        client: &SendRequest<Bytes>,
        request: &ExecuteRequest,
    ) -> Result<Self, String> {
        let (response, mut send) = client
            .clone()
            .ready()
            .await
            .expect("capacity")
            .send_request(
                grpc_request("/build.bazel.remote.execution.v2.Execution/Execute"),
                false,
            )
            .expect("send_request");
        send.send_data(framed(request), true).expect("request");
        let response = response.await.map_err(|e| e.to_string())?;
        if let Some(status) = response.headers().get("grpc-status") {
            return Err(format!("grpc-status {}", status.to_str().unwrap_or("?")));
        }
        let mut body = response.into_body();
        let first = body.data().await.and_then(Result::ok).unwrap_or_default();
        assert!(!first.is_empty(), "the first Operation");
        let _ = body.flow_control().release_capacity(first.len());
        Ok(Self { body })
    }

    /// Whether the stream is still open and quiet: nothing arrived in 5 ms, and it did not end.
    pub async fn quiet(&mut self) -> bool {
        tokio::time::timeout(Duration::from_millis(5), self.body.data())
            .await
            .is_err()
    }
}

/// A plain `GET` of the proxy's own paths on the shared connection: the status and the body.
pub async fn proxy_get(client: &SendRequest<Bytes>, path: &str) -> (u16, String) {
    let request = http::Request::builder()
        .uri(format!("http://frontend.test{path}"))
        .header("authorization", format!("Bearer {OPERATOR_TOKEN}"))
        .body(())
        .expect("a request");
    let (response, _) = client
        .clone()
        .ready()
        .await
        .expect("capacity")
        .send_request(request, true)
        .expect("send_request");
    let response = response.await.expect("an answer");
    let status = response.status().as_u16();
    let mut body = response.into_body();
    let mut text = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.expect("data");
        let _ = body.flow_control().release_capacity(chunk.len());
        text.extend_from_slice(&chunk);
    }
    (status, String::from_utf8_lossy(&text).into_owned())
}

/// Waits (at most 30 s) until the proxy's `/readyz` is 200: some member is up and ready.
pub async fn until_ready(client: &SendRequest<Bytes>) {
    let mut code = proxy_get(client, "/readyz").await.0;
    for _ in 0..1_500 {
        if code == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        code = proxy_get(client, "/readyz").await.0;
    }
    assert_eq!(
        code, 200,
        "/readyz never went 200 with a ready real child in the pool"
    );
}
