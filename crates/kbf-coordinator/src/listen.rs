//! The listeners: TLS in front of [`crate::http::handle`].
//!
//! Each listener holds a [`TlsSource`], so a certificate the farm CA renewed is picked up
//! without a restart (the files are looked at every interval, and a pair that does not make
//! a working configuration leaves the one in use). A heartbeat listener's source requires a
//! client certificate that chains to the farm CA; a control listener's asks for none. A
//! connection speaks HTTP/1.1 only, and has a limit on its handshake and on its request head.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use kbf_tls::TlsSource;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::http::{Peer, Surface, handle};
use crate::service::Coordinator;

/// How long a client has to finish its TLS handshake, and then to send a request head.
pub const CLIENT_PATIENCE: Duration = Duration::from_secs(10);

/// One listener.
pub struct Listener {
    /// The bound socket.
    pub tcp: TcpListener,
    /// Its TLS configuration.
    pub tls: TlsSource,
    /// What it serves.
    pub surface: Surface,
}

impl Listener {
    /// The address it is bound to.
    ///
    /// # Errors
    /// The socket no longer knows its address.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

/// Serves `listener` for `coordinator` for as long as the future is polled. A client gets
/// `patience` for its handshake and again for its request head. It never returns.
pub async fn serve(
    listener: Listener,
    coordinator: Arc<Coordinator>,
    patience: Duration,
) -> Infallible {
    loop {
        match listener.tcp.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(connection(
                    stream,
                    listener.tls.clone(),
                    listener.surface,
                    Arc::clone(&coordinator),
                    patience,
                ));
            }
            Err(why) => {
                tracing::warn!(%why, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn connection(
    stream: TcpStream,
    tls: TlsSource,
    surface: Surface,
    coordinator: Arc<Coordinator>,
    patience: Duration,
) {
    let mut config = (*tls.config()).clone();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let shaken = tokio::time::timeout(patience, TlsAcceptor::from(Arc::new(config)).accept(stream));
    let tls = match shaken.await {
        Ok(Ok(tls)) => tls,
        Ok(Err(why)) => {
            tracing::debug!(%why, "a TLS handshake failed");
            return;
        }
        Err(_) => {
            tracing::debug!("a TLS handshake did not finish in time");
            return;
        }
    };
    let peer: Peer = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(<[_]>::first)
        .map(|leaf| Arc::from(leaf.as_ref()));
    let service = service_fn(move |request| {
        let coordinator = Arc::clone(&coordinator);
        let peer = peer.clone();
        async move { Ok::<_, Infallible>(handle(&coordinator, surface, &peer, request).await) }
    });
    let served = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(patience)
        .serve_connection(TokioIo::new(tls), service)
        .await;
    if let Err(why) = served {
        tracing::debug!(%why, "a connection ended badly");
    }
}

/// Looks at the TLS files of `sources` every `interval`, for as long as the future is polled.
/// It never returns.
pub async fn reload_every(sources: Vec<TlsSource>, interval: Duration) -> Infallible {
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticks.tick().await;
    loop {
        ticks.tick().await;
        for source in &sources {
            let source = source.clone();
            // File reads and a key parse: off the runtime's threads.
            let _ = tokio::task::spawn_blocking(move || source.reload()).await;
        }
    }
}
