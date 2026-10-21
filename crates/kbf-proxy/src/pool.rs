//! The upstream pool: prior-knowledge HTTP/2 over loopback to the children, one pool per
//! (slot, generation, target).
//!
//! Built on h2's own client, not hyper's, because the child's advertised
//! `MAX_CONCURRENT_STREAMS` is on h2's `SendRequest` and not on hyper's. A pick reserves a
//! stream on a connection by compare-and-swap on that connection's in-flight count before
//! anything is sent, so a stream is never queued behind a long one on a full connection:
//! when every connection is full the stream dials another. Dials in flight count against the
//! cap, and waiters wake on "a stream freed or a dial finished", so a release does not make
//! thousands of tasks dial together.
//!
//! A pool is keyed by the slot's generation ([`crate::slot::Permit::generation`]): a slot that
//! was retired and opened again is a new child, and the first stream after the restart never
//! meets a connection of the dead process.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::future::OptionFuture;
use h2::client::SendRequest;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Notify;

use crate::config::UpstreamTable;
use crate::keepalive;
use crate::limits::h2_client_builder;
use crate::metrics::Metrics;
use crate::replay::Failure;
use crate::route::Target;
use crate::slot::Slot;

/// Connections per (slot, generation, target).
pub(crate) const MAX_CONNECTIONS: usize = 256;
/// Connects in flight at once per pool.
pub(crate) const MAX_DIALS: usize = 16;
/// The child's listener must accept within this, or the stream fails `UNAVAILABLE`.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Where a child listens: always loopback, whatever the table says.
pub(crate) const fn upstream_addr(table: UpstreamTable, slot: Slot, target: Target) -> SocketAddr {
    SocketAddr::new(
        std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
        table.port(slot, target),
    )
}

#[derive(Debug)]
struct Conn {
    send: SendRequest<Bytes>,
    inflight: AtomicUsize,
    /// Shared with the connection's driver task, which sets it when the connection ends. The
    /// task holds nothing else of the pool, so dropping the pool drops every `SendRequest` and
    /// the connection closes.
    dead: Arc<AtomicBool>,
    /// Set by the keepalive while the child has left a PING unanswered: no new stream goes
    /// here, the streams already on it are untouched, and the answer clears it.
    suspect: Arc<AtomicBool>,
}

impl Conn {
    /// Reserves one stream if the child's advertised limit has room.
    fn reserve(&self) -> bool {
        let max = self.send.current_max_send_streams();
        self.inflight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max).then_some(n + 1)
            })
            .is_ok()
    }
}

/// One reserved stream on one connection: held until both halves of the stream are finished.
pub(crate) struct Reservation {
    conn: Arc<Conn>,
    pool: Arc<TargetPool>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.conn.inflight.fetch_sub(1, Ordering::AcqRel);
        self.pool.notify.notify_waiters();
    }
}

/// A byte stream the proxy speaks h2 over: a socket, or an in-memory pipe in the tests.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(crate) type BoxIo = Box<dyn Io>;

/// Opens a byte stream to a child's address. The proxy's is [`tcp_connector`]; the tests
/// connect the pool to fake children over memory, so that a paused clock never mistakes a
/// socket's round trip for idleness.
pub(crate) type Connector = Arc<
    dyn Fn(SocketAddr) -> Pin<Box<dyn Future<Output = io::Result<BoxIo>> + Send>> + Send + Sync,
>;

/// The connector the proxy runs with: a TCP connection to the child's loopback port.
pub(crate) fn tcp_connector() -> Connector {
    Arc::new(|addr| {
        Box::pin(async move {
            let tcp = TcpStream::connect(addr).await?;
            tcp.set_nodelay(true)?;
            Ok(Box::new(tcp) as BoxIo)
        })
    })
}

type Handshaken = (SendRequest<Bytes>, h2::client::Connection<BoxIo, Bytes>);

/// Opens one prior-knowledge h2 connection to `addr`, or nothing: refused, too slow
/// ([`CONNECT_TIMEOUT`]) or a failed handshake are all "the child is not there".
async fn connect(connector: &Connector, addr: SocketAddr) -> Option<Handshaken> {
    let io = tokio::time::timeout(CONNECT_TIMEOUT, connector(addr))
        .await
        .ok()?
        .ok()?;
    h2_client_builder().handshake::<_, Bytes>(io).await.ok()
}

/// One of the pool's dial slots, held for the length of one dial.
struct DialSlot(Arc<TargetPool>);

impl Drop for DialSlot {
    fn drop(&mut self) {
        self.0.dialing.fetch_sub(1, Ordering::AcqRel);
        self.0.notify.notify_waiters();
    }
}

pub(crate) struct TargetPool {
    addr: SocketAddr,
    conns: Mutex<Vec<Arc<Conn>>>,
    dialing: AtomicUsize,
    notify: Arc<Notify>,
    metrics: Arc<Metrics>,
    connector: Connector,
}

impl TargetPool {
    fn new(addr: SocketAddr, metrics: Arc<Metrics>, connector: Connector) -> Arc<Self> {
        Arc::new(Self {
            addr,
            connector,
            conns: Mutex::new(Vec::new()),
            dialing: AtomicUsize::new(0),
            notify: Arc::new(Notify::new()),
            metrics,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Conn>>> {
        self.conns.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A connection with a free stream, reserved.
    fn pick(&self) -> Option<Arc<Conn>> {
        self.lock()
            .iter()
            .find(|c| {
                !c.dead.load(Ordering::Acquire) && !c.suspect.load(Ordering::Acquire) && c.reserve()
            })
            .map(Arc::clone)
    }

    /// Takes a dial slot if the dial and connection caps allow one. The slot is given back,
    /// and the waiters woken, when the guard drops: at the end of the dial, or when a stream
    /// that was dialing is cancelled (a client reset) and its future dropped.
    fn begin_dial(self: &Arc<Self>) -> Option<DialSlot> {
        let mut conns = self.lock();
        conns.retain(|c| !c.dead.load(Ordering::Acquire));
        let dialing = self.dialing.load(Ordering::Acquire);
        let room = dialing < MAX_DIALS && conns.len() + dialing < MAX_CONNECTIONS;
        room.then(|| {
            self.dialing.fetch_add(1, Ordering::AcqRel);
            DialSlot(Arc::clone(self))
        })
    }

    async fn dial(self: &Arc<Self>) -> Result<Arc<Conn>, Failure> {
        Metrics::inc(&self.metrics.upstream_dials);
        let Some((send, mut connection)) = connect(&self.connector, self.addr).await else {
            Metrics::inc(&self.metrics.upstream_dial_failed);
            return Err(Failure::Connect);
        };
        let dead = Arc::new(AtomicBool::new(false));
        let conn = Arc::new(Conn {
            send,
            inflight: AtomicUsize::new(0),
            dead: Arc::clone(&dead),
            suspect: Arc::new(AtomicBool::new(false)),
        });
        let pool = Arc::downgrade(self);
        let ping = connection.ping_pong();
        let close = Arc::new(Notify::new());
        let closing = Arc::clone(&close);
        tokio::spawn(async move {
            // The connection ends when the child closes it, or every handle is dropped, or the
            // keepalive gives up on it (dropping it resets what is left on it).
            tokio::select! {
                _ = &mut connection => {}
                () = closing.notified() => {}
            }
            dead.store(true, Ordering::Release);
            if let Some(pool) = pool.upgrade() {
                pool.notify.notify_waiters();
            }
        });
        // Until the child's SETTINGS arrive h2 assumes 100 streams, so a burst of streams could
        // all be reserved on this connection against a limit of 2. The child sends its SETTINGS
        // before it answers a PING, so one round trip makes the advertised limit the real one.
        // A child that does not answer is not there: running on h2's assumed 100 streams is the
        // very thing the PING is for.
        let ponged = OptionFuture::from(ping.map(|mut p| async move {
            let pong = p.ping(h2::Ping::opaque()).await;
            (p, pong)
        }));
        let Ok(Some((ping, Ok(_)))) = tokio::time::timeout(CONNECT_TIMEOUT, ponged).await else {
            conn.dead.store(true, Ordering::Release);
            Metrics::inc(&self.metrics.upstream_dial_failed);
            return Err(Failure::Connect);
        };
        // From here the connection pings itself; the keepalive ends it, if it has to, through
        // the driver task above.
        // It holds the flag, the pool's bell and the PING handle (which does not keep the
        // connection open), so it does not keep the pool or the connection alive; after the
        // connection is gone it ends at its next PING.
        let suspect = Arc::clone(&conn.suspect);
        let bell = Arc::clone(&self.notify);
        tokio::spawn(async move {
            keepalive::upstream(ping, &suspect, &bell).await;
            close.notify_one();
        });
        Ok(conn)
    }

    /// Reserves a stream: on an open connection if one has room, else on a new one. Waits
    /// (keeping the caller's permit) while the caps are reached. Returns the reservation, to
    /// hold until the stream is done, and the connection's sender, ready for one request.
    ///
    /// # Errors
    /// [`Failure::Connect`]: the child did not accept a connection.
    pub(crate) async fn acquire(
        self: &Arc<Self>,
    ) -> Result<(Reservation, SendRequest<Bytes>), Failure> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(conn) = self.pick() {
                // The reservation exists before the first await, so a stream cancelled while
                // it waits for the connection gives its place back.
                let reservation = Reservation {
                    conn: Arc::clone(&conn),
                    pool: Arc::clone(self),
                };
                match conn.send.clone().ready().await {
                    Ok(send) => return Ok((reservation, send)),
                    Err(_) => {
                        // A GOAWAY or a closed socket: no new stream goes here again.
                        conn.dead.store(true, Ordering::Release);
                        continue;
                    }
                }
            }
            if let Some(slot) = self.begin_dial() {
                let dialed = self.dial().await;
                if let Ok(conn) = &dialed {
                    self.lock().push(Arc::clone(conn));
                }
                drop(slot);
                dialed?;
                continue;
            }
            notified.await;
        }
    }
}

/// A pool's key: the slot's name, its generation, the target's port offset.
type PoolKey = (&'static str, u64, u16);

/// Every pool, by (slot, generation, target).
pub(crate) struct Pool {
    table: UpstreamTable,
    metrics: Arc<Metrics>,
    connector: Connector,
    pools: Mutex<HashMap<PoolKey, Arc<TargetPool>>>,
}

impl Pool {
    pub(crate) fn new(table: UpstreamTable, metrics: Arc<Metrics>, connector: Connector) -> Self {
        Self {
            table,
            metrics,
            connector,
            pools: Mutex::new(HashMap::new()),
        }
    }

    /// The pool of `target` in `slot` at `generation`. Asking for a generation drops the pools
    /// of the slot's earlier generations, and with them their connections, once their streams
    /// finish: a restarted child never meets the old process's connections.
    pub(crate) fn target(&self, slot: Slot, generation: u64, target: Target) -> Arc<TargetPool> {
        let mut pools = self.pools.lock().unwrap_or_else(PoisonError::into_inner);
        pools.retain(|(s, g, _), _| *s != slot.name() || *g >= generation);
        Arc::clone(
            pools
                .entry((slot.name(), generation, target.offset()))
                .or_insert_with(|| {
                    TargetPool::new(
                        upstream_addr(self.table, slot, target),
                        Arc::clone(&self.metrics),
                        Arc::clone(&self.connector),
                    )
                }),
        )
    }
}
