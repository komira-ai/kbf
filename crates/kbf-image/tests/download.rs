//! The plain downloader against a server in this process (a loopback address the system picks):
//! what it writes, what it refuses and the URLs it will not speak to.

mod support;

use kbf_image::{Downloader, Error, Timeouts};
use support::scratch;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// Serves `status` and `body` to every connection until the test ends; gives the address.
async fn serve(status: &'static str, body: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a listener");
    let addr = listener.local_addr().expect("an address");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = [0_u8; 1024];
                let _ = stream.read(&mut head).await;
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{addr}/x")
}

fn downloader() -> Downloader {
    Downloader::new(Timeouts::default())
}

/// Asserts that a body is returned whole to memory under its limit and refused over it, and
/// that it is written to a file with its length returned. Catches: a limit that is not applied
/// and a file that is not the body.
#[tokio::test]
async fn a_body_is_read_to_memory_within_its_limit_and_to_a_file_whole() {
    let url = serve("200 OK", b"hello release").await;
    assert_eq!(
        downloader().bytes(&url, 100).await.expect("body"),
        b"hello release"
    );
    let over = downloader()
        .bytes(&url, 5)
        .await
        .expect_err("over the limit");
    assert!(matches!(over, Error::TooLarge { .. }), "{over}");

    let dir = scratch("download-file");
    let to = dir.join("out");
    assert_eq!(downloader().file(&url, &to).await.expect("written"), 13);
    assert_eq!(std::fs::read(&to).expect("the file"), b"hello release");
}

/// Asserts that a status that is not success is an error naming the status, for both calls, and
/// that a file that cannot be created is an error naming its path. Catches: an error page taken
/// for the file.
#[tokio::test]
async fn a_failed_status_and_an_uncreatable_file_are_errors() {
    let url = serve("404 Not Found", b"").await;
    let bytes = downloader().bytes(&url, 10).await.expect_err("a 404");
    assert!(
        matches!(bytes, Error::Status { status: 404, .. }),
        "{bytes}"
    );
    let dir = scratch("download-status");
    let file = downloader()
        .file(&url, &dir.join("out"))
        .await
        .expect_err("a 404");
    assert!(matches!(file, Error::Status { status: 404, .. }), "{file}");

    let ok = serve("200 OK", b"x").await;
    let missing = dir.join("no-such-dir").join("out");
    let created = downloader()
        .file(&ok, &missing)
        .await
        .expect_err("no directory");
    assert!(matches!(created, Error::Io { .. }), "{created}");
    assert!(created.to_string().contains("no-such-dir"), "{created}");
}

/// Asserts that plain http to a host that is not loopback, and a text that is not a URL, are
/// refused without any connection. Catches: a downloader that speaks plain http to the network.
#[tokio::test]
async fn plain_http_off_loopback_and_a_bad_url_are_refused() {
    let plain = downloader()
        .bytes("http://release.example/x", 10)
        .await
        .expect_err("plain");
    assert!(matches!(plain, Error::Insecure(_)), "{plain}");
    let bad = downloader().bytes("not a url", 10).await.expect_err("bad");
    assert!(
        matches!(bad, Error::Transport { .. } | Error::Insecure(_)),
        "{bad}"
    );
}
