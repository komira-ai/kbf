//! A TCP proxy in front of a test server: it counts the connections the engine makes,
//! keeps the first bytes the engine sends on each (the HTTP/2 preface and settings),
//! and can cut every connection at once, as a reset would.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

use super::fake::lock;

/// How many of the engine's first bytes on a connection are kept.
const KEPT: usize = 256;

#[derive(Debug, Default)]
pub struct Proxy {
    pub accepted: AtomicUsize,
    /// The first bytes the engine sent on each connection.
    pub first_bytes: Mutex<Vec<Vec<u8>>>,
    links: Mutex<Vec<AbortHandle>>,
}

impl Proxy {
    /// A proxy to `target`; returns it and its address.
    pub async fn start(target: SocketAddr) -> (Arc<Self>, SocketAddr) {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let proxy = Arc::new(Self::default());
        let accepting = Arc::clone(&proxy);
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let n = accepting.accepted.fetch_add(1, Ordering::SeqCst);
                lock(&accepting.first_bytes).push(Vec::new());
                let proxy = Arc::clone(&accepting);
                let link = tokio::spawn(async move { proxy.relay(n, client, target).await });
                lock(&accepting.links).push(link.abort_handle());
            }
        });
        (proxy, addr)
    }

    /// Cuts every connection made so far: both sockets close.
    pub fn cut(&self) {
        for link in lock(&self.links).drain(..) {
            link.abort();
        }
    }

    async fn relay(&self, n: usize, mut client: TcpStream, target: SocketAddr) {
        let Ok(mut server) = TcpStream::connect(target).await else {
            return;
        };
        let (mut client_read, mut client_write) = client.split();
        let (mut server_read, mut server_write) = server.split();
        let up = async {
            let mut buffer = vec![0; 64 << 10];
            loop {
                let read = client_read.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                {
                    let mut kept = lock(&self.first_bytes);
                    let first = &mut kept[n];
                    let room = KEPT.saturating_sub(first.len()).min(read);
                    first.extend_from_slice(&buffer[..room]);
                }
                if server_write.write_all(&buffer[..read]).await.is_err() {
                    break;
                }
            }
            let _ = server_write.shutdown().await;
        };
        let down = async {
            let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
            let _ = client_write.shutdown().await;
        };
        tokio::join!(up, down);
    }
}

/// The settings the engine's HTTP/2 connection asks for, from its first bytes: the
/// initial stream window (SETTINGS_INITIAL_WINDOW_SIZE) and the connection window
/// (65,535 plus the first WINDOW_UPDATE on stream 0).
pub fn windows(first: &[u8]) -> (Option<u32>, u32) {
    const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    assert!(first.starts_with(PREFACE), "no HTTP/2 preface: {first:?}");
    let mut at = PREFACE.len();
    let (mut stream_window, mut connection_window) = (None, 65_535);
    while at + 9 <= first.len() {
        let len = u32::from_be_bytes([0, first[at], first[at + 1], first[at + 2]]) as usize;
        let kind = first[at + 3];
        let stream =
            u32::from_be_bytes([first[at + 5], first[at + 6], first[at + 7], first[at + 8]]);
        let payload = &first[(at + 9).min(first.len())..(at + 9 + len).min(first.len())];
        match kind {
            // SETTINGS: six-byte entries.
            4 => {
                for entry in payload.chunks_exact(6) {
                    if u16::from_be_bytes([entry[0], entry[1]]) == 4 {
                        stream_window =
                            Some(u32::from_be_bytes([entry[2], entry[3], entry[4], entry[5]]));
                    }
                }
            }
            // WINDOW_UPDATE on the connection.
            8 if stream == 0 && payload.len() == 4 => {
                connection_window +=
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                        & 0x7fff_ffff;
                return (stream_window, connection_window);
            }
            _ => {}
        }
        at += 9 + len;
    }
    (stream_window, connection_window)
}
