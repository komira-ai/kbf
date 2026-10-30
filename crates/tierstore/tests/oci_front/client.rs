//! An HTTPS client by hand: it trusts one CA, dials an IP address, and reads what the server
//! sends, down to a response that ends short.

use std::net::SocketAddr;
use std::sync::Arc;

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// A connector that trusts the CA in `ca_pem` and nothing else.
pub fn connector(ca_pem: &str) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        roots.add(cert.expect("a certificate")).expect("a root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// What the server answered.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The body is shorter than the `Content-Length` said (it is never short for a `HEAD`).
    pub short: bool,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        let found = self.headers.iter().find(|(n, _)| n == name);
        found.map(|(_, value)| value.as_str())
    }

    #[track_caller]
    pub fn must(&self, name: &str) -> &str {
        self.header(name)
            .unwrap_or_else(|| panic!("no {name} in {self:?}"))
    }

    /// The `code` of the one error in the body.
    #[track_caller]
    pub fn error_code(&self) -> String {
        let document: serde_json::Value =
            serde_json::from_slice(&self.body).expect("an error document");
        document["errors"][0]["code"]
            .as_str()
            .expect("a code")
            .to_owned()
    }
}

/// A connection, which carries requests one after the other.
pub struct Conn {
    stream: TlsStream<TcpStream>,
    buf: Vec<u8>,
}

impl Conn {
    /// Connects to `addr` by IP address and completes the handshake.
    pub async fn open(addr: SocketAddr, connector: &TlsConnector) -> std::io::Result<Self> {
        let tcp = TcpStream::connect(addr).await?;
        let name = rustls::pki_types::ServerName::from(addr.ip());
        let stream = connector.connect(name, tcp).await?;
        Ok(Self {
            stream,
            buf: Vec::new(),
        })
    }

    /// Sends a request and reads the answer. `None` if the connection ended before the head of an
    /// answer was complete.
    pub async fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Option<Reply> {
        self.send(method, path, headers).await;
        self.reply(method == "HEAD").await
    }

    pub async fn send(&mut self, method: &str, path: &str, headers: &[(&str, &str)]) {
        let mut text = format!("{method} {path} HTTP/1.1\r\nhost: oci.test\r\n");
        for (name, value) in headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        text.push_str("\r\n");
        self.stream.write_all(text.as_bytes()).await.expect("send");
    }

    /// Reads an answer: the head, and as much of the body as the `Content-Length` says is there.
    pub async fn reply(&mut self, head_only: bool) -> Option<Reply> {
        let end = loop {
            if let Some(at) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at;
            }
            if !self.fill().await {
                return None;
            }
        };
        let head = String::from_utf8_lossy(&self.buf[..end]).into_owned();
        self.buf.drain(..end + 4);
        let mut lines = head.lines();
        let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let length: usize = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);
        let want = if head_only { 0 } else { length };
        while self.buf.len() < want && self.fill().await {}
        let taken = want.min(self.buf.len());
        let body: Vec<u8> = self.buf.drain(..taken).collect();
        Some(Reply {
            status,
            headers,
            short: body.len() < want,
            body,
        })
    }

    /// Reads the head of an answer and leaves its body unread; the status.
    pub async fn head_only(&mut self) -> Option<u16> {
        loop {
            if let Some(at) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buf[..at]).into_owned();
                return head.split(' ').nth(1)?.parse().ok();
            }
            if !self.fill().await {
                return None;
            }
        }
    }

    /// Reads until the server closes the connection or it fails; whether there was nothing more
    /// to read.
    pub async fn closed(&mut self) -> bool {
        while self.fill().await {}
        self.buf.is_empty()
    }

    async fn fill(&mut self) -> bool {
        let mut chunk = vec![0; 64 * 1024];
        match self.stream.read(&mut chunk).await {
            Ok(0) | Err(_) => false,
            Ok(count) => {
                self.buf.extend_from_slice(&chunk[..count]);
                true
            }
        }
    }
}
