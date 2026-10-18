//! The front-door proxy in front of real `kbf` frontend processes, through a swap
//! (docs/design/kbf-proxy-api.md section 14, "Soak"): 200 long-lived gRPC streams are open
//! through the proxy to the frontend in slot A; the swap holds the gate, makes a second real
//! frontend the active slot (slot B) and releases; every old stream then runs to its end on A,
//! and every stream opened during the hold or after it is served by B.
//!
//! Everything but the proxy library, which runs in this process the way the installer's verb
//! runs it, is a child process: two `kbf storage` nodes (file stores under the scratch
//! directory, one behind each frontend) and two `kbf frontend` processes. The children are
//! this test binary run through a link named `kbf`, which answers as the real binary's own
//! `main` before the harness starts (the same trick as `m1.rs`), so the test needs no other
//! binary built and no service: not Postgres, not MinIO.
//!
//! The streams are `Execution.Execute` calls, the stream a Buck2 build holds open per action:
//! no worker runs them, so each is queued on its frontend and the stream stays open and idle.
//! The frontend counts them itself (`open_executions` of `GET /v1/frontend/drain`), which
//! makes every figure below the child's own and exact. Which child served a stream is read
//! from that count, and from where the action is: each action is in the store behind one
//! frontend only, so a stream the proxy sent to the wrong child would be refused.
//!
//! The proxy has no admin listener to poll (U2 builds it); its `/readyz` poll asks each
//! child's operator listener, which is the `--api-listen` port, so the soak also proves
//! `/readyz` against the real thing: 200 with A active, and 200 again once B is.
//!
//! Why not `ByteStream.Write` or a `Read` nobody reads (the pull request has the detail):
//! a `Write` whose last message is followed by an empty DATA frame with END_STREAM (how the
//! proxy, and every Go gRPC client, ends a request) makes the frontend pass an empty DATA
//! frame on to its storage node, and `h2` 0.4.20 ends a connection that receives more than
//! 100 of them in its life; and `Read`s that stall on flow control share the frontend's one
//! connection to its node, so a few of them starve the rest.

#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
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
use kbf_proxy::Proxy;
use kbf_proxy::config::ProxyConfig;
use kbf_proxy::control::NoUpdates;
use kbf_proxy::slot::{Retire, Slot};
use prost::Message as _;
use sha2::{Digest as _, Sha256};

/// Streams opened before the swap and finished after it.
const OLD: usize = 200;
/// Streams opened while the gate holds: served by the new child.
const HELD: usize = 20;
/// Streams opened and finished after the swap: served by the new child.
const NEW: usize = 30;

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

/// A `kbf` child that has printed its start line; stopped when dropped.
struct Child {
    process: std::process::Child,
    line: String,
    // Held open: a child that wrote to a closed pipe would die.
    _stdout: BufReader<ChildStdout>,
}

impl Child {
    fn start(dir: &Path, name: &str, args: &[String]) -> Self {
        let log = std::fs::File::create(dir.join(format!("{name}.log"))).expect("log file");
        let mut process = Command::new(dir.join("bin/kbf"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .expect("spawn kbf");
        let mut stdout = BufReader::new(process.stdout.take().expect("stdout"));
        let mut line = String::new();
        stdout.read_line(&mut line).expect("a start line");
        Self {
            process,
            line,
            _stdout: stdout,
        }
    }

    /// The `key=` field of the start line.
    fn field(&self, key: &str) -> String {
        self.line
            .split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} in the start line {:?}", self.line))
            .to_owned()
    }

    /// SIGTERM, then the exit status within 20 s.
    fn stop(&mut self) -> Option<std::process::ExitStatus> {
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

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// One `GET` on a child's operator API: the status code and the body.
fn api_get(api: &str, path: &str) -> (u16, String) {
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
async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .expect(what)
}

/// Waits (at most 30 s) until `done` holds.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
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

/// A base port such that the REAPI, worker, operator and admin ports of slot A (the base plus
/// 0 to 3) and of slot B (plus 10 to 13), as the proxy's static table has them, are free now.
fn free_base() -> u16 {
    let mut base = 20_000 + (std::process::id() % 20_000) as u16;
    loop {
        let bound: Vec<_> = [0, 1, 2, 3, 10, 11, 12, 13]
            .iter()
            .filter_map(|o| TcpListener::bind((Ipv4Addr::LOCALHOST, base + o)).ok())
            .collect();
        if bound.len() == 8 {
            return base;
        }
        base += 20;
    }
}

fn storage_node(dir: &Path, name: &str) -> Child {
    Child::start(
        dir,
        name,
        &[
            "storage".to_owned(),
            "--storage-listen=127.0.0.1:0".to_owned(),
            format!(
                "--cold-store=file://{}",
                dir.join(name).join("cold").display()
            ),
            format!("--meta-dir={}", dir.join(name).join("meta").display()),
        ],
    )
}

fn frontend(dir: &Path, name: &str, node: &Child, base: u16) -> Child {
    Child::start(
        dir,
        name,
        &[
            "frontend".to_owned(),
            format!("--listen=127.0.0.1:{base}"),
            format!("--worker-listen=127.0.0.1:{}", base + 1),
            format!("--api-listen=127.0.0.1:{}", base + 2),
            format!("--storage=http://{}", node.field("storage=")),
        ],
    )
}

/// One gRPC message: the 5-byte prefix and the encoded message.
fn framed(message: &impl prost::Message) -> Bytes {
    let body = message.encode_to_vec();
    let mut out = vec![0_u8];
    out.extend((body.len() as u32).to_be_bytes());
    out.extend(body);
    Bytes::from(out)
}

fn grpc_request(path: &str) -> http::Request<()> {
    http::Request::builder()
        .method("POST")
        .uri(format!("http://frontend.test{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .expect("a request")
}

async fn connect(addr: &str) -> (SendRequest<Bytes>, tokio::task::JoinHandle<()>) {
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (client, connection) = h2::client::Builder::new()
        .initial_connection_window_size(64 << 20)
        .handshake::<_, Bytes>(tcp)
        .await
        .expect("h2 handshake");
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    (client, driver)
}

fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as i64,
    }
}

/// The actions `(tag, 0..count)` and the blobs that make them: each action is a command of
/// its own, over an empty input root. Returns the `Execute` requests and the blobs to store.
fn actions(tag: &str, count: usize) -> (Vec<ExecuteRequest>, Vec<BlobRequest>) {
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
async fn store(reapi: &str, blobs: Vec<BlobRequest>) {
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
fn open_executions(api: &str) -> u64 {
    let (code, body) = api_get(api, "/v1/frontend/drain");
    assert_eq!(code, 200, "{body}");
    let status: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    status["open_executions"].as_u64().expect("open_executions")
}

/// An `Execute` that has its response head and its first message, and is then left alone:
/// the stream stays open until it is dropped, which resets it.
struct Open {
    body: RecvStream,
}

impl Open {
    /// Starts `request` through `client`. The `Err` is the gRPC status of a refusal that
    /// came in the headers (a trailers-only answer) or the transport error.
    async fn start(client: &SendRequest<Bytes>, request: &ExecuteRequest) -> Result<Self, String> {
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
    async fn quiet(&mut self) -> bool {
        tokio::time::timeout(Duration::from_millis(5), self.body.data())
            .await
            .is_err()
    }
}

/// A plain `GET` of the proxy's own paths on the shared connection: the status and the body.
async fn proxy_get(client: &SendRequest<Bytes>, path: &str) -> (u16, String) {
    let request = http::Request::builder()
        .uri(format!("http://frontend.test{path}"))
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

/// Waits (at most 30 s) until the proxy's `/readyz` is 200.
async fn until_ready(client: &SendRequest<Bytes>) {
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
        "/readyz never went 200 with a ready real child active"
    );
}

/// Catches, with real children, a proxy that: closes or GOAWAYs the client's one connection
/// at the swap, resets or fails a stream bound before it, moves an old stream to the new
/// child or a new one to the old, loses a held stream, retires the old slot under a stream,
/// or reports `/readyz` 503 beside a ready real child.
#[test]
fn the_flip_with_two_hundred_streams_drops_none_and_each_child_serves_its_own() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("proxy-soak-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).expect("scratch");
    std::os::unix::fs::symlink(
        std::env::current_exe().expect("this binary"),
        dir.join("bin/kbf"),
    )
    .expect("link kbf");

    let (node_a, node_b) = (storage_node(&dir, "node-a"), storage_node(&dir, "node-b"));
    let base = free_base();
    let mut a = frontend(&dir, "frontend-a", &node_a, base);
    // The candidate runs from the start, out of the proxy's table until the swap, so that
    // its store can hold its actions before any stream asks for them.
    let mut b = frontend(&dir, "frontend-b", &node_b, base + 10);
    let (api_a, api_b) = (
        format!("127.0.0.1:{}", base + 2),
        format!("127.0.0.1:{}", base + 12),
    );
    assert_eq!(
        (api_get(&api_a, "/readyz").0, api_get(&api_b, "/readyz").0),
        (200, 200)
    );

    let public = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("public socket");
    let public_addr: SocketAddr = public.local_addr().expect("addr");
    let config = ProxyConfig::from_toml_str(&format!(
        "operator_token_file = \"/etc/kbf/operator.token\"\n\
         server_cert_file = \"/etc/kbf/tls/server.pem\"\n\
         server_key_file = \"/etc/kbf/tls/server.key\"\n\
         client_ca_file = \"/etc/kbf/tls/ca.pem\"\n\
         [upstream]\nbase_port = {base}\n"
    ))
    .expect("a valid table");
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(public)).expect("bind");
    let serving = std::thread::spawn(move || runner.run(NoUpdates));
    // The supervisor's first act: adopt A, end the startup hold.
    let startup = handle.startup_hold().expect("the startup hold");
    handle.slots().adopt(Slot::A, None);
    handle.slots().release_into(Slot::A).expect("A is open");
    drop(startup);

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    client_runtime.block_on(async {
        // The old actions are in the store behind A only, the new ones behind B only.
        let (old_requests, old_blobs) = actions("old", OLD);
        let (new_requests, new_blobs) = actions("new", HELD + NEW);
        within("the old actions", store(&a.field("reapi="), old_blobs)).await;
        within("the new actions", store(&b.field("reapi="), new_blobs)).await;
        let (client, driver) = connect(&public_addr.to_string()).await;
        // `/readyz` is the proxy's poll of the real child: 200 once a poll has passed.
        until_ready(&client).await;

        // 200 streams through one connection to A, each open and idle.
        let starting: Vec<_> = old_requests
            .iter()
            .map(|r| Open::start(&client, r))
            .collect();
        let mut old: Vec<Open> = within("200 old streams", futures::future::join_all(starting))
            .await
            .into_iter()
            .map(|open| open.expect("an old stream starts on A"))
            .collect();
        assert_eq!(open_executions(&api_a), OLD as u64, "A's own count");
        assert_eq!(open_executions(&api_b), 0);
        assert_eq!(handle.snapshot().stats(Slot::A).open_streams, OLD as u64);
        assert_eq!(handle.snapshot().active, Some(Slot::A));

        // The swap: hold, make B the active slot, release.
        let hold = handle
            .slots()
            .hold(Duration::from_secs(20))
            .expect("not holding");
        let mut held = Vec::new();
        for request in &new_requests[..HELD] {
            held.push(tokio::spawn({
                let (client, request) = (client.clone(), request.clone());
                async move { Open::start(&client, &request).await }
            }));
        }
        until("the proxy holds the new streams", || {
            handle.snapshot().held == HELD
        })
        .await;
        assert_eq!(open_executions(&api_b), 0, "nothing reaches B while held");
        handle.slots().open(Slot::B);
        handle.slots().release_into(Slot::B).expect("B is open");
        drop(hold);
        assert_eq!(handle.snapshot().active, Some(Slot::B));
        assert!(
            !driver.is_finished(),
            "the flip left the client's connection alone"
        );

        // The held streams and the streams after the swap are all B's.
        let mut new = Vec::new();
        for stream in held {
            let open = within("a held stream", stream).await.expect("task");
            new.push(open.expect("a held stream is served by B"));
        }
        for request in &new_requests[HELD..] {
            let open = Open::start(&client, request).await;
            new.push(open.expect("a new stream is served by B"));
        }
        // And an old action after the swap goes to B, which has not got it: it is refused.
        let refused = Open::start(&client, &old_requests[0]).await;
        assert!(
            refused.is_err(),
            "a new stream for an old action is not served by A"
        );
        assert_eq!(
            open_executions(&api_b),
            (HELD + NEW) as u64,
            "B's own count"
        );
        assert_eq!(
            open_executions(&api_a),
            OLD as u64,
            "A kept its own, and took no more"
        );
        let snapshot = handle.snapshot();
        assert_eq!(
            (
                snapshot.stats(Slot::A).open_streams,
                snapshot.stats(Slot::B).open_streams
            ),
            (OLD as u64, (HELD + NEW) as u64)
        );

        // Every stream is still open and quiet: no GOAWAY, no reset, on either side. A is
        // not retired under its streams (`can_retire` is the supervisor's read-only probe;
        // `retire` is its commit, and it would close the slot's connections).
        for open in old.iter_mut().chain(new.iter_mut()) {
            assert!(open.quiet().await, "a stream ended or spoke");
        }
        assert!(!handle.slots().can_retire(Slot::A));

        // The old streams end (the client cancels them, as Buck2 does) and A sees each one.
        drop(old);
        until("A's executions end", || open_executions(&api_a) == 0).await;
        assert_eq!(
            open_executions(&api_b),
            (HELD + NEW) as u64,
            "B's are untouched"
        );
        until("A's last permit drops", || {
            handle.slots().can_retire(Slot::A)
        })
        .await;
        assert_eq!(handle.slots().retire(Slot::A), Retire::Closed);
        assert!(a.stop().is_some_and(|status| status.success()), "A exits 0");

        // `/readyz` is B's now, and the proxy dropped, refused and retried nothing.
        until_ready(&client).await;
        let (code, metrics) = proxy_get(&client, "/metrics").await;
        assert_eq!(code, 200);
        for zero in [
            "kbf_proxy_panics_total",
            "kbf_proxy_hold_rejected_total",
            "kbf_proxy_streams_refused_total",
            "kbf_proxy_upstream_dial_failed_total",
            "kbf_proxy_retries_total",
            "kbf_proxy_hold_expired_total",
            "kbf_proxy_hold_abandoned_total",
            "kbf_proxy_held_streams",
        ] {
            assert!(
                metrics.contains(&format!("\n{zero} 0\n")),
                "{zero} is not 0:\n{metrics}"
            );
        }
        assert!(
            !driver.is_finished(),
            "one connection carried every stream, to the end"
        );
        drop(new);
        until("B's executions end", || open_executions(&api_b) == 0).await;
        assert_eq!(b.stop().and_then(|s| s.code()), Some(0), "B exits 0");
    });
    assert_eq!(handle.snapshot().holds_abandoned, 0);
    handle.shutdown();
    serving.join().expect("the runner thread").expect("run");
    drop((node_a, node_b));
}
