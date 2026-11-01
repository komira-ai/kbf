//! The real sink's large-object PUT against a real HTTP server.

use std::io::{BufRead, BufReader};

use kbf_outputs::CHUNK_BYTES;

use super::*;
use crate::outputs::collect_outputs;
use crate::put::FixedJitter;

/// What one request carried.
struct Seen {
    length: Option<String>,
    body: Vec<u8>,
}

/// Serves one connection per status, in order: reads the request and its body, answers.
fn serve(statuses: Vec<u16>) -> (String, std::thread::JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let server = std::thread::spawn(move || {
        let mut all = Vec::new();
        for status in statuses {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut length = None;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("line");
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().to_owned());
                }
            }
            let mut body = vec![0u8; length.as_deref().map_or(0, |l| l.parse().expect("length"))];
            reader.read_exact(&mut body).expect("body");
            let reply = format!(
                "HTTP/1.1 {status} X\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            reader.get_mut().write_all(reply.as_bytes()).expect("reply");
            all.push(Seen { length, body });
        }
        all
    });
    (base, server)
}

/// A sink whose one large slot is `url`, and which records its waits.
fn sink(url: String, waits: &Arc<Waits>) -> HttpSink {
    HttpSink {
        agent: agent(None),
        urls: PutUrls {
            packs: vec!["http://127.0.0.1:9/unused".to_owned()],
            large: vec![url],
        },
        sleeper: Arc::new(Arc::clone(waits)),
        jitter: Arc::new(FixedJitter(0.5)),
    }
}

/// Records a file bigger than a hashing chunk, so it is held as a descriptor; returns the
/// blobs, its digest and its bytes.
async fn recorded(name: &str) -> (TestDir, Blobs, kbf_types::Digest, Vec<u8>) {
    let dir = scratch(name);
    let bytes: Vec<u8> = (0..CHUNK_BYTES + 5000).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("blob"), &bytes).expect("blob");
    std::fs::write(dir.join("out"), b"o").expect("out");
    std::fs::write(dir.join("err"), b"e").expect("err");
    let outputs = collect_outputs(
        &dir,
        "",
        &["blob".to_owned()],
        &dir.join("out"),
        &dir.join("err"),
        OutputLimits::default(),
        u64::MAX,
    )
    .await
    .expect("collected");
    let digest = digest_of(outputs.result.output_files[0].digest.as_ref()).expect("digest");
    (dir, outputs.blobs, digest, bytes)
}

/// Catches a large PUT that sends less than the blob, or without its length, or that does not
/// start the blob again after a 503: a 503 and then a 201 PUT the whole blob twice, each time
/// with `Content-Length` its size, after the one wait the store asked for.
#[tokio::test]
async fn a_large_put_retries_a_503_with_the_whole_blob() {
    let (_dir, blobs, digest, bytes) = recorded("http-large").await;
    let (base, server) = serve(vec![503, 201]);
    let waits = Arc::new(Waits::default());
    sink(format!("{base}/large?sig=s"), &waits)
        .store_large(0, &blobs, &digest)
        .expect("stored");
    let seen = server.join().expect("server");
    assert_eq!(seen.len(), 2);
    for request in &seen {
        assert_eq!(request.length, Some(bytes.len().to_string()));
        assert!(request.body == bytes, "the body differs from the blob");
    }
    assert_eq!(*waits.0.lock().unwrap(), [Duration::from_secs(1)]);
}

/// Catches a 412 on a large object being retried: the key exists, one PUT, a put failure.
#[tokio::test]
async fn a_large_put_does_not_retry_a_412() {
    let (_dir, blobs, digest, _) = recorded("http-large-412").await;
    let (base, server) = serve(vec![412]);
    let waits = Arc::new(Waits::default());
    let err = sink(format!("{base}/large?sig=s"), &waits)
        .store_large(0, &blobs, &digest)
        .expect_err("exists");
    assert_eq!(server.join().expect("server").len(), 1);
    assert!(matches!(err, PutError::Exists { .. }), "{err:?}");
    assert!(waits.0.lock().unwrap().is_empty());
}

/// Catches a blob that is not recorded being sent as if it were: the PUT fails locally, naming
/// the blob, and does not contact the store (nothing listens at the URL).
#[tokio::test]
async fn a_large_put_of_an_unrecorded_blob_fails_without_connecting() {
    let (_dir, blobs, _, _) = recorded("http-large-absent").await;
    let waits = Arc::new(Waits::default());
    let absent = sha256(b"never recorded");
    let err = sink("http://127.0.0.1:9/large?sig=s".to_owned(), &waits)
        .store_large(0, &blobs, &absent)
        .expect_err("not recorded");
    assert!(matches!(err, PutError::File { .. }), "{err:?}");
    assert!(err.to_string().contains("output blob"), "{err}");
    assert_eq!(Failure::from(err).exit_code(), EXIT_PUT);
}

/// Catches the sink's slot counts not being the key set's.
#[test]
fn the_http_sink_reports_the_slots_of_its_key_set() {
    let sink = sink("http://h/l".to_owned(), &Arc::new(Waits::default()));
    assert_eq!(sink.pack_slots(), 1);
    assert_eq!(sink.large_slots(), 1);
}

/// Catches a large object whose file changed after it was hashed being reported stored: the PUT
/// reaches the store (the object there is an orphan), and the sink fails with a put error
/// (exit 13) saying the bytes changed.
#[tokio::test]
async fn a_large_put_of_a_changed_file_fails_after_the_store_took_it() {
    let (dir, blobs, digest, _) = recorded("http-large-changed").await;
    crate::task::recording_sink::flip_first_byte(&dir.join("blob"));
    let (base, server) = serve(vec![201]);
    let waits = Arc::new(Waits::default());
    let err = sink(format!("{base}/large?sig=s"), &waits)
        .store_large(0, &blobs, &digest)
        .expect_err("changed");
    assert_eq!(server.join().expect("server").len(), 1);
    assert!(err.to_string().contains("orphan"), "{err}");
    assert_eq!(Failure::from(err).exit_code(), EXIT_PUT);
}
