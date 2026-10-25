//! Requests held in flight on a connection of their own: a body that stops part-way, a body that
//! trickles, and an answer that is not read.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::support::{BUCKET, Harness};

/// The text of a request that has a body of `length` bytes: the request line, the headers, and
/// the blank line.
pub fn head(method: &str, path: &str, headers: &[(String, String)], length: usize) -> String {
    let mut text = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("content-length: {length}\r\n\r\n"));
    text
}

/// Everything the server sends on the connection until it closes it.
pub async fn read_answer(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The status code of an answer.
pub fn status_of(answer: &str) -> u16 {
    answer[9..12].parse().unwrap()
}

/// Sends a signed get of `key` on a connection of its own, which closes after the answer, and
/// does not read the answer.
pub async fn send_get(h: &Harness, key: &str) -> TcpStream {
    let path = format!("/{BUCKET}/{key}");
    let mut headers = h.signed("GET", &path, &[], &[], b"");
    headers.push(("connection".to_owned(), "close".to_owned()));
    let text = head("GET", &path, &headers, 0);
    let mut stream = TcpStream::connect(h.addr).await.unwrap();
    stream.write_all(text.as_bytes()).await.unwrap();
    stream
}

/// A put whose body was sent in part, so that the server holds it in flight.
pub struct Stalled {
    stream: TcpStream,
    rest: Vec<u8>,
}

impl Stalled {
    /// Sends the head of a put with these signed headers, and the first `sent` bytes of `body`.
    /// The connection is closed after the answer.
    pub async fn open(
        addr: SocketAddr,
        headers: &[(String, String)],
        path: &str,
        body: &[u8],
        sent: usize,
    ) -> Self {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut headers = headers.to_vec();
        headers.push(("connection".to_owned(), "close".to_owned()));
        let text = head("PUT", path, &headers, body.len());
        stream.write_all(text.as_bytes()).await.unwrap();
        stream.write_all(&body[..sent]).await.unwrap();
        Self {
            stream,
            rest: body[sent..].to_vec(),
        }
    }

    /// Sends the rest of the body and gives the status code of the answer.
    pub async fn finish(mut self) -> u16 {
        self.stream.write_all(&self.rest).await.unwrap();
        status_of(&read_answer(&mut self.stream).await)
    }

    /// Sends the rest of the body a byte at a time, `gap` apart, and gives the status code of the
    /// answer.
    pub async fn trickle(mut self, gap: Duration) -> u16 {
        self.stream.set_nodelay(true).unwrap();
        for byte in std::mem::take(&mut self.rest) {
            tokio::time::sleep(gap).await;
            self.stream.write_all(&[byte]).await.unwrap();
        }
        status_of(&read_answer(&mut self.stream).await)
    }

    /// Reads the answer to the end of the connection without sending the rest of the body.
    pub async fn answer(mut self) -> String {
        read_answer(&mut self.stream).await
    }
}
