//! Tests that the HTTP source pools its keep-alive connections across requests and threads.

use super::*;

/// What a keep-alive server saw: requests served and connections accepted. Both are counted
/// before the response bytes are written, so they are final once the client has all its answers.
/// `ended` gets one message per connection whose client closed it.
struct KeepAlive {
    base: String,
    requests: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
    ended: std::sync::mpsc::Receiver<()>,
}

/// Serves `blobs` (by `<hex>/<size>`) over HTTP/1.1 with Content-Length bodies and no
/// `Connection: close`, any number of requests per accepted connection, one thread per
/// connection. A connection's thread ends when the client closes it; `ended` hears of it.
fn serve_keep_alive(blobs: HashMap<String, Vec<u8>>) -> KeepAlive {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let requests = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(AtomicUsize::new(0));
    let (ended_tx, ended) = std::sync::mpsc::channel();
    let (served, accepted) = (Arc::clone(&requests), Arc::clone(&connections));
    let blobs = Arc::new(blobs);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("accept");
            accepted.fetch_add(1, Ordering::SeqCst);
            let (served, blobs, ended_tx) =
                (Arc::clone(&served), Arc::clone(&blobs), ended_tx.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                while reader.read_line(&mut line).expect("request line") > 0 {
                    let path = line.split_whitespace().nth(1).expect("path").to_owned();
                    let mut header = String::new();
                    loop {
                        header.clear();
                        reader.read_line(&mut header).expect("header");
                        if header.trim().is_empty() {
                            break;
                        }
                    }
                    served.fetch_add(1, Ordering::SeqCst);
                    let body = &blobs[&path[1..]];
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                    stream.write_all(head.as_bytes()).expect("head");
                    stream.write_all(body).expect("body");
                    line.clear();
                }
                ended_tx.send(()).expect("ended");
            });
        }
    });
    KeepAlive {
        base,
        requests,
        connections,
        ended,
    }
}

/// Catches a connection opened for every request (which exhausts a host's local ports at high
/// concurrency): 40 files in 4 directories plus the root are fetched through one source with 4
/// allowed in flight, and the server sees every blob requested once and no more than 4
/// connections, each serving several requests.
#[test]
fn connections_are_reused_across_requests_and_threads() {
    let mut mem = Memory::default();
    let mut groups = Vec::new();
    for group in 0..4 {
        let files = (0..10)
            .map(|i| {
                let blob = mem.put(format!("group {group} file {i}").into_bytes());
                file(&format!("f{i}"), &blob, false)
            })
            .collect();
        let directory = mem.put_directory(&Directory {
            files,
            ..Directory::default()
        });
        groups.push(sub(&format!("g{group}"), &directory));
    }
    let root = mem.put_directory(&Directory {
        directories: groups,
        ..Directory::default()
    });
    let blobs = mem
        .blobs
        .iter()
        .map(|(hash, bytes)| (format!("{hash}/{}", bytes.len()), bytes.clone()))
        .collect();
    let server = serve_keep_alive(blobs);
    let scratch_dir = scratch("keep-alive");
    let dir = scratch_dir.join("in");
    let source = plain(&server.base);
    materialize(&source, &root, &dir, 4).expect("materialize");
    for group in 0..4 {
        for i in 0..10 {
            assert_eq!(
                std::fs::read(dir.join(format!("g{group}/f{i}"))).expect("file"),
                format!("group {group} file {i}").into_bytes()
            );
        }
    }
    assert_eq!(server.requests.load(Ordering::SeqCst), mem.blobs.len());
    let connections = server.connections.load(Ordering::SeqCst);
    assert!(connections <= 4, "{connections} connections");
    // Dropping the source drops its agent's pool, which closes the idle sockets; every
    // connection's loop then ends on end of input and reports it. A leaked socket hangs the
    // wait; the nextest profile's `terminate-after` (.config/nextest.toml) ends the hang and
    // fails the test. Nothing here asserts elapsed time.
    drop(source);
    for _ in 0..connections {
        server
            .ended
            .recv()
            .expect("a connection was not closed when the source was dropped");
    }
}
