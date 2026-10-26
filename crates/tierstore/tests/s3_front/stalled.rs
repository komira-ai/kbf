//! A request sent by hand on a connection of its own, whose answer is not read.

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpSocket, TcpStream};

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

/// The receive buffer of the connection of [`send_get`]. A small fixed one, so that what the
/// server can send ahead of the reader does not depend on how large the host lets a socket's
/// buffer grow (a host with 32 MiB receive buffers would not hold back the answer that the tests
/// of an unread answer need held back).
const GET_RECEIVE_BUFFER: u32 = 256 << 10;

/// Sends a signed get of `key` on a connection of its own, which closes after the answer, and
/// does not read the answer.
pub async fn send_get(h: &Harness, key: &str) -> TcpStream {
    let path = format!("/{BUCKET}/{key}");
    let mut headers = h.signed("GET", &path, &[], &[], b"");
    headers.push(("connection".to_owned(), "close".to_owned()));
    let text = head("GET", &path, &headers, 0);
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(GET_RECEIVE_BUFFER).unwrap();
    let mut stream = socket.connect(h.addr).await.unwrap();
    stream.write_all(text.as_bytes()).await.unwrap();
    stream
}
