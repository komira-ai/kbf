//! Raw HTTP/1.1 calls to the operator API with the operator token, for the tests that
//! read its JSON.

#![allow(dead_code, reason = "each test binary uses some of the helpers")]

use std::net::SocketAddr;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::support::API_TOKEN;

/// One request, carrying the operator token and a JSON content type, with `body`: the
/// status code, the head and the JSON body (`null` if it is none).
pub async fn http(api: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String, Value) {
    let mut stream = TcpStream::connect(api).await.expect("connect");
    let auth = format!("Authorization: Bearer {API_TOKEN}\r\nContent-Type: application/json\r\n");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: {}\r\n{auth}\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (
        code,
        head.to_owned(),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

/// `GET path`: the status code and the JSON body.
pub async fn get(api: SocketAddr, path: &str) -> (u16, Value) {
    let (code, _, body) = http(api, "GET", path, "").await;
    (code, body)
}

/// `POST path` with `body`: the status code and the JSON body.
pub async fn post(api: SocketAddr, path: &str, body: &str) -> (u16, Value) {
    let (code, _, body) = http(api, "POST", path, body).await;
    (code, body)
}
