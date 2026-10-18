//! The fake children, the client and the rig the tests share.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use h2::client::{ResponseFuture, SendRequest};
use h2::server::SendResponse;
use h2::{RecvStream, SendStream};
use http::{HeaderMap, HeaderValue, Request};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use crate::config::{Limits, UpstreamTable};
use crate::control::{NoUpdates, UpdateHandler};
use crate::core::Core;
use crate::pool::{BoxIo, Connector, upstream_addr};
use crate::route::Target;
use crate::serve::{Listen, serve};
use crate::slot::{ActiveSlot, Slot};

pub(super) const EXECUTE: &str = "/build.bazel.remote.execution.v2.Execution/Execute";
pub(super) const CAPS: &str = "/build.bazel.remote.execution.v2.Capabilities/GetCapabilities";
pub(super) const WRITE: &str = "/google.bytestream.ByteStream/Write";
pub(super) const OPERATOR: &str = "/v1/status";

/// What a fake child is asked to do with one stream.
pub(super) struct Call {
    pub body: RecvStream,
    pub respond: SendResponse<Bytes>,
    /// How many streams this child has accepted so far, this one included.
    pub nth: usize,
    /// Rung to make this stream's connection send a graceful GOAWAY.
    pub goaway: Arc<Notify>,
}

pub(super) type Script =
    Arc<dyn Fn(Call) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub(super) fn script<F, Fut>(f: F) -> Script
where
    F: Fn(Call) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    Arc::new(move |call| Box::pin(f(call)))
}

#[derive(Default)]
pub(super) struct ChildState {
    pub dispatches: AtomicUsize,
    pub conns_open: AtomicUsize,
    pub conns_total: AtomicUsize,
    pub seen: Mutex<Vec<(String, HeaderMap)>>,
}

impl ChildState {
    pub(super) fn dispatches(&self) -> usize {
        self.dispatches.load(Ordering::Acquire)
    }

    pub(super) fn conns_open(&self) -> usize {
        self.conns_open.load(Ordering::Acquire)
    }

    pub(super) fn headers_of(&self, path: &str) -> Vec<HeaderMap> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, h)| h.clone())
            .collect()
    }
}

/// The in-memory network between the proxy and its children. A connect to an address with no
/// listener is refused, as on a real host, and nothing here touches a socket, so a paused clock
/// moves only when every task is truly idle.
#[derive(Default)]
pub(super) struct MemNet {
    listeners: Mutex<HashMap<SocketAddr, mpsc::UnboundedSender<DuplexStream>>>,
}

const PIPE_BYTES: usize = 1 << 20;

impl MemNet {
    fn listen(&self, addr: SocketAddr) -> mpsc::UnboundedReceiver<DuplexStream> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(addr, tx);
        rx
    }

    fn unlisten(&self, addr: SocketAddr) {
        self.listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&addr);
    }

    /// The pool's connector: a pipe to whoever listens on the address.
    pub(super) fn connector(self: &Arc<Self>) -> Connector {
        let net = Arc::clone(self);
        Arc::new(move |addr| {
            let net = Arc::clone(&net);
            Box::pin(async move {
                let listeners = net.listeners.lock().unwrap_or_else(PoisonError::into_inner);
                let tx = listeners
                    .get(&addr)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionRefused))?;
                let (near, far) = tokio::io::duplex(PIPE_BYTES);
                tx.send(far)
                    .map_err(|_| io::Error::from(io::ErrorKind::ConnectionRefused))?;
                Ok(Box::new(near) as BoxIo)
            })
        })
    }
}

/// The proxy's public listener, fed by the tests.
pub(super) struct MemListen {
    rx: mpsc::UnboundedReceiver<(DuplexStream, IpAddr)>,
}

impl Listen for MemListen {
    async fn accept(&mut self) -> io::Result<(BoxIo, IpAddr)> {
        let (io, peer) = self
            .rx
            .recv()
            .await
            .ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionAborted))?;
        Ok((Box::new(io), peer))
    }
}

/// One slot's fake child: a listener on each of its addresses, one set of counters.
pub(super) struct Child {
    pub state: Arc<ChildState>,
    net: Option<Arc<MemNet>>,
    addrs: Vec<SocketAddr>,
    accepting: Vec<JoinHandle<()>>,
}

impl Child {
    fn start(net: &Arc<MemNet>, addrs: Vec<SocketAddr>, script: &Script) -> Self {
        let state = Arc::new(ChildState::default());
        let accepting = addrs
            .iter()
            .map(|addr| {
                let mut rx = net.listen(*addr);
                let (state, script) = (Arc::clone(&state), Arc::clone(script));
                tokio::spawn(async move {
                    while let Some(io) = rx.recv().await {
                        tokio::spawn(connection(io, Arc::clone(&state), Arc::clone(&script)));
                    }
                })
            })
            .collect();
        Self {
            state,
            net: Some(Arc::clone(net)),
            addrs,
            accepting,
        }
    }

    /// The child's listeners close; its open connections live on, as a child that was
    /// told to stop but has not exited does.
    pub(super) async fn stop_accepting(&mut self) {
        for task in self.accepting.drain(..) {
            task.abort();
            let _ = task.await;
        }
        if let Some(net) = &self.net {
            for addr in &self.addrs {
                net.unlisten(*addr);
            }
        }
    }
}

/// A fake child on real sockets, for the one test that goes through `Proxy::bind`.
pub(super) fn start_child_on(listeners: Vec<std::net::TcpListener>, script: Script) -> Child {
    let state = Arc::new(ChildState::default());
    let accepting = listeners
        .into_iter()
        .map(|listener| {
            listener.set_nonblocking(true).expect("non-blocking");
            let listener = TcpListener::from_std(listener).expect("tokio listener");
            let (state, script) = (Arc::clone(&state), Arc::clone(&script));
            tokio::spawn(async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    tokio::spawn(connection(tcp, Arc::clone(&state), Arc::clone(&script)));
                }
            })
        })
        .collect();
    Child {
        state,
        net: None,
        addrs: Vec::new(),
        accepting,
    }
}

async fn connection(
    io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    state: Arc<ChildState>,
    script: Script,
) {
    let Ok(mut conn) = h2::server::Builder::new()
        .initial_window_size(4 << 20)
        .initial_connection_window_size(64 << 20)
        .handshake::<_, Bytes>(io)
        .await
    else {
        return;
    };
    state.conns_open.fetch_add(1, Ordering::AcqRel);
    state.conns_total.fetch_add(1, Ordering::AcqRel);
    let goaway = Arc::new(Notify::new());
    let mut graceful = false;
    loop {
        tokio::select! {
            accepted = conn.accept() => {
                let Some(Ok((request, respond))) = accepted else { break };
                let (parts, body) = request.into_parts();
                let nth = state.dispatches.fetch_add(1, Ordering::AcqRel) + 1;
                state
                    .seen
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((parts.uri.path().to_owned(), parts.headers.clone()));
                tokio::spawn((script)(Call {
                    body,
                    respond,
                    nth,
                    goaway: Arc::clone(&goaway),
                }));
            }
            () = goaway.notified(), if !graceful => {
                graceful = true;
                conn.graceful_shutdown();
            }
        }
    }
    state.conns_open.fetch_sub(1, Ordering::AcqRel);
}

/// Reads a request body to its end, returning the credit as it goes.
pub(super) async fn read_all(body: &mut RecvStream) -> Vec<u8> {
    let mut all = Vec::new();
    while let Some(Ok(chunk)) = body.data().await {
        let _ = body.flow_control().release_capacity(chunk.len());
        all.extend_from_slice(&chunk);
    }
    all
}

/// A gRPC answer: headers, one message, `grpc-status: 0`.
pub(super) fn grpc_ok(respond: &mut SendResponse<Bytes>, message: &'static str) {
    let response = http::Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .body(())
        .expect("a response");
    let mut send = respond.send_response(response, false).expect("headers");
    send.send_data(Bytes::from_static(message.as_bytes()), false)
        .expect("data");
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from_static("0"));
    send.send_trailers(trailers).expect("trailers");
}

/// A child that reads the request and answers `tag`.
pub(super) fn tagged(tag: &'static str) -> Script {
    script(move |mut call| async move {
        read_all(&mut call.body).await;
        grpc_ok(&mut call.respond, tag);
    })
}

/// What the client saw of one stream.
#[derive(Debug)]
pub(super) struct Reply {
    pub status: Option<u16>,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub trailers: Option<HeaderMap>,
    pub error: Option<h2::Error>,
}

impl Reply {
    pub(super) fn grpc_status(&self) -> Option<String> {
        let from = self.trailers.as_ref().unwrap_or(&self.headers);
        from.get("grpc-status")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    pub(super) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Reads a response to its end.
pub(super) async fn finish(response: ResponseFuture) -> Reply {
    let mut reply = Reply {
        status: None,
        headers: HeaderMap::new(),
        body: Vec::new(),
        trailers: None,
        error: None,
    };
    let response = match response.await {
        Ok(response) => response,
        Err(error) => {
            reply.error = Some(error);
            return reply;
        }
    };
    reply.status = Some(response.status().as_u16());
    reply.headers = response.headers().clone();
    let mut body = response.into_body();
    while let Some(chunk) = body.data().await {
        match chunk {
            Ok(chunk) => {
                let _ = body.flow_control().release_capacity(chunk.len());
                reply.body.extend_from_slice(&chunk);
            }
            Err(error) => {
                reply.error = Some(error);
                return reply;
            }
        }
    }
    match body.trailers().await {
        Ok(trailers) => reply.trailers = trailers,
        Err(error) => reply.error = Some(error),
    }
    reply
}

/// Big windows, as a real gRPC client has: with h2's defaults, a few hundred small unread
/// responses exhaust the client's own data-frame budget and the client closes the connection.
fn client_builder() -> h2::client::Builder {
    let mut builder = h2::client::Builder::new();
    builder
        .initial_window_size(4 << 20)
        .initial_connection_window_size(512 << 20);
    builder
}

/// An h2c client connection to the proxy.
pub(super) struct Client {
    pub send: SendRequest<Bytes>,
    pub driver: JoinHandle<Result<(), h2::Error>>,
}

impl Client {
    /// A connection to a listening [`Rig`] or [`MemListen`] feeder.
    pub(super) async fn connect(
        feed: &mpsc::UnboundedSender<(DuplexStream, IpAddr)>,
        ip: IpAddr,
    ) -> Self {
        let (near, far) = tokio::io::duplex(PIPE_BYTES);
        feed.send((far, ip)).expect("the proxy is listening");
        let (send, connection) = client_builder()
            .handshake(near)
            .await
            .expect("h2 handshake");
        Self {
            send,
            driver: tokio::spawn(connection),
        }
    }

    /// A connection to a proxy on a real socket.
    pub(super) async fn connect_tcp(addr: SocketAddr) -> Self {
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the proxy");
        let (send, connection) = client_builder().handshake(tcp).await.expect("h2 handshake");
        Self {
            send,
            driver: tokio::spawn(connection),
        }
    }

    /// Starts a gRPC request; `end` sends it as headers only.
    pub(super) async fn start(
        &mut self,
        path: &str,
        end: bool,
    ) -> (ResponseFuture, SendStream<Bytes>) {
        self.start_with(path, end, &[]).await
    }

    pub(super) async fn start_with(
        &mut self,
        path: &str,
        end: bool,
        extra: &[(&str, &str)],
    ) -> (ResponseFuture, SendStream<Bytes>) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(format!("http://frontend.test{path}"))
            .header("content-type", "application/grpc");
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(()).expect("a request");
        let mut send = self.send.clone().ready().await.expect("capacity");
        send.send_request(request, end).expect("send_request")
    }

    /// A plain HTTP request (no gRPC content type).
    pub(super) async fn plain(&mut self, method: &str, path: &str, body: &'static [u8]) -> Reply {
        let request = Request::builder()
            .method(method)
            .uri(format!("http://frontend.test{path}"))
            .body(())
            .expect("a request");
        let mut send = self.send.clone().ready().await.expect("capacity");
        let (response, mut stream) = send
            .send_request(request, body.is_empty())
            .expect("send_request");
        if !body.is_empty() {
            stream
                .send_data(Bytes::from_static(body), true)
                .expect("body");
        }
        finish(response).await
    }

    /// A whole unary call: headers, one message, END_STREAM.
    pub(super) async fn unary(&mut self, path: &str) -> Reply {
        let (response, mut body) = self.start(path, false).await;
        body.send_data(Bytes::from_static(b"ping"), true)
            .expect("body");
        finish(response).await
    }

    pub(super) fn alive(&self) -> bool {
        !self.driver.is_finished()
    }
}

/// The address a slot's target listens on.
fn addr_of(table: UpstreamTable, slot: Slot, target: Target) -> SocketAddr {
    upstream_addr(table, slot, target)
}

const TARGETS: [Target; 3] = [Target::Reapi, Target::Worker, Target::Operator];

/// The address the tests give the proxy's clients.
pub(super) const CLIENT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));

/// Two fake children behind one proxy core, with A active and open.
pub(super) struct Rig {
    pub core: Arc<Core>,
    pub net: Arc<MemNet>,
    pub a: Child,
    pub b: Child,
    pub table: UpstreamTable,
    feed: mpsc::UnboundedSender<(DuplexStream, IpAddr)>,
    stop: watch::Sender<bool>,
    pub serving: JoinHandle<()>,
}

static NEXT_BASE: AtomicU16 = AtomicU16::new(21_000);

/// Binds the six loopback ports of the static table (REAPI, worker, operator of each slot).
pub(super) fn reserve_ports() -> (u16, Vec<std::net::TcpListener>) {
    const OFFSETS: [u16; 6] = [0, 1, 2, 10, 11, 12];
    loop {
        let base = NEXT_BASE.fetch_add(20, Ordering::AcqRel);
        assert!(base < 60_000, "out of ports");
        let bound: Option<Vec<_>> = OFFSETS
            .iter()
            .map(|o| std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, base + o)).ok())
            .collect();
        if let Some(listeners) = bound {
            return (base, listeners);
        }
    }
}

impl Rig {
    pub(super) async fn start(a: Script, b: Script) -> Self {
        Self::start_with(a, b, NoUpdates, Limits::default()).await
    }

    pub(super) async fn start_with<U: UpdateHandler + 'static>(
        a: Script,
        b: Script,
        updates: U,
        limits: Limits,
    ) -> Self {
        let slots = ActiveSlot::new();
        slots.open(Slot::A);
        slots.set_active(Slot::A).expect("A is open");
        Self::build(a, b, updates, limits, slots).await
    }

    /// A rig over the gate `slots` as the caller made it.
    pub(super) async fn build<U: UpdateHandler + 'static>(
        a: Script,
        b: Script,
        updates: U,
        limits: Limits,
        slots: ActiveSlot,
    ) -> Self {
        let table = UpstreamTable { base_port: 18_000 };
        let net = Arc::new(MemNet::default());
        let core = Core::new(table, limits, slots, net.connector());
        let (feed, incoming) = mpsc::unbounded_channel();
        let (stop, stopped) = watch::channel(false);
        let serving = tokio::spawn(serve(
            Arc::clone(&core),
            updates,
            MemListen { rx: incoming },
            stopped,
        ));
        let child = |slot, script: &Script| {
            Child::start(
                &net,
                TARGETS.map(|t| addr_of(table, slot, t)).to_vec(),
                script,
            )
        };
        Self {
            core,
            a: child(Slot::A, &a),
            b: child(Slot::B, &b),
            net,
            table,
            feed,
            stop,
            serving,
        }
    }

    pub(super) async fn client(&self) -> Client {
        Client::connect(&self.feed, CLIENT_IP).await
    }

    /// Hands the proxy a connection's far half, as if `ip` had connected.
    pub(super) fn feed_raw(&self, far: DuplexStream, ip: IpAddr) {
        self.feed.send((far, ip)).expect("the proxy is listening");
    }

    pub(super) async fn client_from(&self, ip: IpAddr) -> Client {
        Client::connect(&self.feed, ip).await
    }

    /// Starts a new child in `slot` on the table's addresses, as a supervisor does after the
    /// old one stopped. The old child's connections are untouched.
    pub(super) fn respawn(&mut self, slot: Slot, script: &Script) {
        let addrs = TARGETS.map(|t| addr_of(self.table, slot, t)).to_vec();
        let child = Child::start(&self.net, addrs, script);
        match slot {
            Slot::A => self.a = child,
            Slot::B => self.b = child,
        }
    }

    /// Makes B ready and the active slot, as a finished swap does.
    pub(super) fn flip_to_b(&self) {
        self.core.slots.open(Slot::B);
        self.core.slots.set_active(Slot::B).expect("B is open");
    }

    pub(super) fn shutdown(&self) {
        self.stop.send_replace(true);
    }
}

/// Lets the runtime run until `done` holds: every wait is a yield, so under paused time it
/// ends when the tasks have nothing more to do and never advances the clock on its own.
pub(super) async fn until(mut done: impl FnMut() -> bool) {
    for round in 0..20_000_u32 {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
        if round % 64 == 63 {
            // Let the I/O driver run; under paused time this moves the clock by a millisecond.
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
    assert!(done(), "the condition never held");
}
