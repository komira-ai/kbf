//! The download client against a loopback server.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use mdm_host::{Download, Net};

/// Serves `connections` requests on a free loopback port, then ends.
fn serve(connections: usize) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        for stream in listener.incoming().take(connections) {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap()
                .to_owned();
            let path = line.split(' ').nth(1).unwrap().to_owned();
            let (head, body) = match path.as_str() {
                "/ok" => ("200 OK", "hello".to_owned()),
                "/redirect" => ("302 Found\r\nLocation: /ok", String::new()),
                "/big" => ("200 OK", "x".repeat(1000)),
                _ => ("404 Not Found", "nope".to_owned()),
            };
            let reply = format!(
                "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (base, handle)
}

/// Catches a client that treats an error status as a body, ignores the size limit, does not
/// follow a redirect (release assets are served from one), or dials plain http when it must
/// not: only the last needs no server.
#[test]
fn downloads_follow_redirects_and_refuse_errors_big_bodies_and_plain_http() {
    let (base, server) = serve(6);
    let net = Net::new(false);
    assert_eq!(net.get(&format!("{base}/ok"), 100).unwrap(), b"hello");
    assert_eq!(net.get(&format!("{base}/redirect"), 100).unwrap(), b"hello");
    assert_eq!(
        net.get(&format!("{base}/missing"), 100).unwrap_err(),
        "answered 404"
    );
    assert!(net.get(&format!("{base}/big"), 10).is_err());
    assert!(net.get(&format!("{base}/big"), 2000).is_ok());
    server.join().unwrap();

    let strict = Net::new(true);
    assert!(strict.get(&format!("{base}/ok"), 100).is_err());
}
