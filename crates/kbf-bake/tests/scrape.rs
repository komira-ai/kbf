//! The metrics read over HTTP: a body is text, and a failure of any kind is an error.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use kbf_bake::scrape::http_get;

/// A server that answers every request with `response`, bytes as they are.
fn serve(response: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/metrics", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(&response);
        }
    });
    url
}

fn reply(status: &str, body: &[u8]) -> Vec<u8> {
    let mut r = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    r.extend_from_slice(body);
    r
}

/// Catches an error status read as a scrape (a gateway's page would be parsed as metrics) and a
/// body that is not text read as one.
#[test]
fn only_a_successful_text_response_is_a_scrape() {
    let t = Duration::from_secs(5);
    assert_eq!(
        http_get(&serve(reply("200 OK", b"kbf_up 1\n")), t).unwrap(),
        "kbf_up 1\n"
    );
    let err = http_get(&serve(reply("503 Service Unavailable", b"down")), t).unwrap_err();
    assert!(err.contains("503"), "{err}");
    assert!(http_get(&serve(reply("200 OK", &[0xff, 0xfe, b'\n'])), t).is_err());
}

/// Catches a read with no timeout: a server that accepts and says nothing is an error after the
/// timeout, not a hang.
#[test]
fn a_silent_server_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/metrics", listener.local_addr().unwrap());
    let _hold = std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().take(1).collect();
        std::thread::sleep(Duration::from_secs(3));
        drop(held);
    });
    let started = std::time::Instant::now();
    assert!(http_get(&url, Duration::from_millis(300)).is_err());
    assert!(started.elapsed() < Duration::from_secs(3));
}
