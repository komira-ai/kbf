//! The address an endpoint URL names, and the probe of an endpoint's `/readyz` over a socket.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use super::probe::{HttpProbe, Probe, authority};

/// Asserts what `authority` makes of the URLs the file may give: the host and the port, the
/// port of the scheme where there is none, an address in brackets; and what it refuses: another
/// scheme, a path or a trailing slash, a query, user information, an empty host, a port that is
/// empty, zero, too large or not a number, an address with colons and no brackets, and a
/// character a host name does not have.
#[test]
fn an_endpoint_names_a_host_and_a_port() {
    let accepted = [
        ("http://192.0.2.1:9300", "192.0.2.1:9300"),
        ("http://proxy-a.example", "proxy-a.example:80"),
        ("https://proxy_a.example", "proxy_a.example:443"),
        ("http://proxy-a.example:65535", "proxy-a.example:65535"),
        ("http://[2001:db8::1]:9300", "[2001:db8::1]:9300"),
        ("http://[2001:db8::1]", "[2001:db8::1]:80"),
    ];
    for (url, want) in accepted {
        assert_eq!(authority(url).as_deref(), Some(want), "{url}");
    }
    let refused = [
        "ftp://192.0.2.1:9300",
        "192.0.2.1:9300",
        "http://",
        "http://:9300",
        "http://192.0.2.1:",
        "http://192.0.2.1:0",
        "http://192.0.2.1:65536",
        "http://192.0.2.1:port",
        "http://192.0.2.1:9300/",
        "http://192.0.2.1:9300/path",
        "http://192.0.2.1:9300?x=1",
        "http://user@192.0.2.1:9300",
        "http://2001:db8::1:9300",
        "http://a b:1",
        "http://a\"b:1",
    ];
    for url in refused {
        assert_eq!(authority(url), None, "{url}");
    }
}

/// A server on loopback that answers the first connection with `reply` (and keeps the request
/// it read to give to the test), then closes. The address and the requests it saw.
async fn answering(reply: &'static [u8]) -> (SocketAddr, mpsc::UnboundedReceiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (seen, requests) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            // The probe sends its request in one write, so one read has all of it.
            let mut chunk = [0; 256];
            let read = stream.read(&mut chunk).await.unwrap();
            let _ = seen.send(chunk[..read].to_vec());
            let _ = stream.write_all(reply).await;
        }
    });
    (address, requests)
}

/// Asserts that the probe is ready for a 200 only: HTTP/1.1 and HTTP/1.0, whatever follows the
/// status; and not ready for another status, for text that is not HTTP, for a status that is not
/// a number or not text, for a connection closed before twelve bytes of answer, for a refused connection
/// and for a URL that names no address. It would catch a probe that takes a 2xx or 3xx as
/// ready, or an answer it cannot read.
#[tokio::test]
async fn the_probe_is_ready_for_a_200_only() {
    let probe = HttpProbe::new(Duration::from_secs(30));
    let cases: [(&'static [u8], bool); 10] = [
        (b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nready", true),
        (b"HTTP/1.0 200 OK\r\n\r\n", true),
        (b"HTTP/1.1 200", true),
        (b"HTTP/1.1 503 Service Unavailable\r\n\r\ndraining", false),
        (b"HTTP/1.1 204 No Content\r\n\r\n", false),
        (b"HTTP/1.1 301 Moved\r\n\r\n", false),
        (b"HTTP/2.0 200 OK\r\n\r\n", false),
        (b"HTTP/1.1 2x0 OK\r\n\r\n", false),
        (b"HTTP/1.1 \xff\xfe\xfd OK\r\n\r\n", false),
        (b"short", false),
    ];
    for (reply, want) in cases {
        let (address, _) = answering(reply).await;
        let url = format!("http://{address}");
        let got = probe.ready(&url).await;
        assert_eq!(got, want, "{}", String::from_utf8_lossy(reply));
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert!(!probe.ready(&url).await, "a refused connection");
    assert!(
        !probe.ready("ftp://192.0.2.1:9300").await,
        "a URL with no address"
    );
}

/// Asserts the request the probe sends: a GET of `/readyz` in HTTP/1.1 with the `Host` of the
/// endpoint and `Connection: close`, and nothing else (no body, no credential).
#[tokio::test]
async fn the_probe_sends_a_get_of_readyz() {
    let (address, mut requests) = answering(b"HTTP/1.1 200 OK\r\n\r\n").await;
    let probe = HttpProbe::new(Duration::from_secs(30));
    assert!(probe.ready(&format!("http://{address}")).await);
    let request = requests.recv().await.unwrap();
    let want = format!("GET /readyz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    assert_eq!(String::from_utf8(request).unwrap(), want);
}

/// Asserts that an endpoint that accepts the connection and never answers is not ready once the
/// probe's time is up. Time is the paused clock of the runtime, which moves on when nothing
/// else can run, so the test does not wait.
#[tokio::test(start_paused = true)]
async fn the_probe_gives_up_on_an_endpoint_that_does_not_answer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
        drop(stream);
    });
    let probe = HttpProbe::new(Duration::from_secs(2));
    assert!(!probe.ready(&url).await);
}
