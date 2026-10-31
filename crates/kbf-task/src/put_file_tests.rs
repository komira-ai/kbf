use super::*;
use kbf_testkit::dir::TestDir;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-put-file-{name}"))
}

/// What the test server saw of one request.
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Serve one connection per entry of `answers` on a local port, in order: read the request
/// and its whole body, answer the status with the extra header lines. Returns the base URL and
/// what each request carried.
fn serve_each(answers: Vec<(u16, &'static str)>) -> (String, JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let handle = std::thread::spawn(move || {
        let mut all = Vec::new();
        for (status, extra) in answers {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').expect("header colon");
                headers.push((name.to_string(), value.trim().to_string()));
            }
            let length: usize = headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, v)| v.parse().expect("length"));
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("body");
            let reply = format!(
                "HTTP/1.1 {status} X\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            reader.get_mut().write_all(reply.as_bytes()).expect("reply");
            all.push(Seen {
                request_line: request_line.trim_end().to_string(),
                headers,
                body,
            });
        }
        all
    });
    (base, handle)
}

/// `len` bytes that differ from place to place, so a body sent short, doubled or out of order
/// does not equal them.
fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 7 + i / 251) % 256).expect("a byte"))
        .collect()
}

/// A sleeper that does not wait.
struct NoWait;

impl Sleeper for NoWait {
    fn sleep(&self, _wait: Duration) {}
}

/// Catches a body that is not the file's bytes (short, doubled, reordered), a missing
/// create-only header, a `Content-Length` that is not the file's size, and a chunked body
/// (which the store's size cap cannot check up front). The file is 3 MiB, far over any one read
/// buffer, so it is sent in many pieces.
#[test]
fn the_file_arrives_whole_with_its_length_create_only() {
    let dir = scratch("whole");
    let path = dir.join("blob");
    let bytes = pattern(3 << 20);
    std::fs::write(&path, &bytes).expect("write");
    let (base, server) = serve_each(vec![(201, "")]);
    let url = format!("{base}/large/key?X-Sig=secret");
    assert_eq!(put_file(&agent(None), &url, &path), Ok(()));
    let seen = server.join().expect("server");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].request_line, "PUT /large/key?X-Sig=secret HTTP/1.1");
    assert_eq!(seen[0].header("if-none-match"), Some("*"));
    assert_eq!(seen[0].header("content-length"), Some("3145728"));
    assert_eq!(seen[0].header("transfer-encoding"), None);
    assert!(seen[0].body == bytes, "the body differs from the file");
    assert!(path.exists(), "the file is left in place");
}

/// Catches an empty file being refused or sent chunked: it is a PUT with length 0.
#[test]
fn an_empty_file_is_a_put_of_zero_bytes() {
    let dir = scratch("empty");
    let path = dir.join("blob");
    std::fs::write(&path, b"").expect("write");
    let (base, server) = serve_each(vec![(200, "")]);
    assert_eq!(put_file(&agent(None), &format!("{base}/k"), &path), Ok(()));
    let seen = server.join().expect("server");
    assert_eq!(seen[0].header("content-length"), Some("0"));
    assert!(seen[0].body.is_empty());
}

/// Catches a 503 being final for a file, or the retry sending less than the file: a 503 and
/// then a 201 stores the file, and the second request carries all of it again (the file was
/// reopened), after the one wait the store asked for.
#[test]
fn a_503_then_a_201_resends_the_whole_file() {
    let dir = scratch("retry");
    let path = dir.join("blob");
    let bytes = pattern(1 << 20);
    std::fs::write(&path, &bytes).expect("write");
    let (base, server) = serve_each(vec![(503, "Retry-After: 4\r\n"), (201, "")]);
    let url = format!("{base}/k?X-Sig=secret");
    let waits = std::sync::Mutex::new(Vec::new());
    struct Record<'a>(&'a std::sync::Mutex<Vec<Duration>>);
    impl Sleeper for Record<'_> {
        fn sleep(&self, wait: Duration) {
            self.0.lock().expect("lock").push(wait);
        }
    }
    let agent = agent(None);
    let outcome = put_retrying(
        || put_file(&agent, &url, &path),
        &Record(&waits),
        &FixedJitter(0.5),
    );
    assert_eq!(outcome, Ok(()));
    let seen = server.join().expect("server");
    assert_eq!(seen.len(), 2);
    assert!(seen[0].body == bytes, "the first attempt differs");
    assert!(seen[1].body == bytes, "the retry differs");
    assert_eq!(seen[1].header("content-length"), Some("1048576"));
    assert_eq!(*waits.lock().expect("lock"), [Duration::from_secs(4)]);
}

/// Catches a 412 being retried or hidden: the key exists, the PUT is not repeated, and the
/// error names the redacted URL and not the signature.
#[test]
fn a_412_is_final() {
    let dir = scratch("exists");
    let path = dir.join("blob");
    std::fs::write(&path, b"bytes").expect("write");
    let (base, server) = serve_each(vec![(412, "")]);
    let url = format!("{base}/k?X-Sig=secret");
    let agent = agent(None);
    let err =
        put_retrying(|| put_file(&agent, &url, &path), &NoWait, &FixedJitter(0.5)).unwrap_err();
    assert_eq!(server.join().expect("server").len(), 1);
    assert_eq!(
        err,
        PutError::Exists {
            url: format!("{base}/k?<redacted>")
        }
    );
    assert!(!err.to_string().contains("secret"));
}

/// Catches another status being read as success, and its message holding the signature.
#[test]
fn another_status_is_reported_without_the_query() {
    let dir = scratch("status");
    let path = dir.join("blob");
    std::fs::write(&path, b"bytes").expect("write");
    let (base, server) = serve_each(vec![(413, "")]);
    let url = format!("{base}/k?X-Sig=secret");
    let err = put_file(&agent(None), &url, &path).unwrap_err();
    server.join().expect("server");
    assert_eq!(
        err,
        PutError::Status {
            status: 413,
            url: format!("{base}/k?<redacted>")
        }
    );
    assert!(!err.to_string().contains("secret"));
}

/// Catches a transport failure leaking the signature.
#[test]
fn an_unreachable_store_is_reported_without_the_query() {
    let dir = scratch("unreachable");
    let path = dir.join("blob");
    std::fs::write(&path, b"bytes").expect("write");
    // Bind a port, then free it: nothing listens there.
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let url = format!("http://127.0.0.1:{port}/k?X-Sig=secret");
    let err = put_file(&agent(None), &url, &path).unwrap_err();
    assert!(matches!(err, PutError::Transport { .. }), "{err:?}");
    assert!(!err.to_string().contains("secret"), "{err}");
}

/// Catches the cap being off by one, and a too-large file reaching the network: a file of
/// exactly the cap is sent, one byte more is refused with nothing contacted (the URL points at a
/// port where nothing listens, so a connection attempt would be a transport error instead).
#[test]
fn a_file_over_the_cap_is_refused_before_connecting() {
    let dir = scratch("cap");
    let path = dir.join("blob");
    std::fs::write(&path, b"0123456789").expect("write");
    let (base, server) = serve_each(vec![(201, "")]);
    assert_eq!(
        put_file_capped(&agent(None), &format!("{base}/k"), &path, 10),
        Ok(())
    );
    assert_eq!(server.join().expect("server")[0].body, b"0123456789");

    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let url = format!("http://127.0.0.1:{port}/k?X-Sig=secret");
    let err = put_file_capped(&agent(None), &url, &path, 9).unwrap_err();
    assert_eq!(err, PutError::TooLarge { size: 10, limit: 9 });
    assert!(err.to_string().contains("10 bytes"), "{err}");
}

/// Catches the cap constant drifting from the 512 MiB the store and the frontend enforce.
#[test]
fn the_large_cap_is_512_mib() {
    assert_eq!(MAX_LARGE_BYTES, 536_870_912);
}

/// Catches a missing file being sent as an empty one or panicking: it is a local error that
/// names the path, and nothing is sent.
#[test]
fn a_missing_file_is_a_local_error_naming_the_path() {
    let dir = scratch("missing");
    let path = dir.join("absent");
    let err = put_file(&agent(None), "http://127.0.0.1:9/k?X-Sig=secret", &path).unwrap_err();
    assert!(matches!(err, PutError::File { .. }), "{err:?}");
    assert!(err.to_string().contains("absent"), "{err}");
    assert!(!err.to_string().contains("secret"), "{err}");
}

/// Catches a held-open body not being streamed with its length, and the cap not applying to it:
/// the first `len` bytes of the reader arrive create-only with `Content-Length: len` even when
/// the reader holds more, and a length over the cap is refused before connecting.
#[test]
fn put_reader_sends_the_announced_length_and_refuses_over_the_cap() {
    let bytes = pattern(70_000);
    let (base, server) = serve_each(vec![(201, "")]);
    let mut reader: &[u8] = &bytes;
    assert_eq!(
        put_reader(&agent(None), &format!("{base}/k"), 60_000, &mut reader),
        Ok(())
    );
    let seen = server.join().expect("server");
    assert_eq!(seen[0].header("content-length"), Some("60000"));
    assert_eq!(seen[0].header("if-none-match"), Some("*"));
    assert!(seen[0].body == bytes[..60_000], "the body differs");

    let mut empty: &[u8] = &[];
    let err = put_reader(
        &agent(None),
        "http://127.0.0.1:9/k?X-Sig=secret",
        MAX_LARGE_BYTES + 1,
        &mut empty,
    )
    .unwrap_err();
    assert_eq!(
        err,
        PutError::TooLarge {
            size: MAX_LARGE_BYTES + 1,
            limit: MAX_LARGE_BYTES
        }
    );
}
