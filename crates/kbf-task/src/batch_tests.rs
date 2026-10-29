//! Tests of the batch route against a fake grant that speaks the batch framing over plain HTTP
//! with keep-alive and records every request.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use kbf_proto::reapi::{Directory, FileNode};
use kbf_testkit::dir::TestDir;

use super::*;

type Blobs = Arc<HashMap<String, Vec<u8>>>;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-task-batch-{name}"))
}

fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: i64::try_from(bytes.len()).expect("size"),
    }
}

/// A distinct blob of `size` bytes for number `i`.
fn content(i: usize, size: usize) -> Vec<u8> {
    let mut bytes = format!("blob number {i} ")
        .repeat(size / 8 + 2)
        .into_bytes();
    bytes.truncate(size);
    bytes
}

/// A one-directory tree of `files` (name, bytes, executable): the blobs by label, and the root.
fn tree(files: &[(String, Vec<u8>, bool)]) -> (Blobs, Digest) {
    let mut blobs = HashMap::new();
    let mut nodes = Vec::new();
    for (name, bytes, is_executable) in files {
        let digest = digest_of(bytes);
        blobs.insert(label(&digest), bytes.clone());
        nodes.push(FileNode {
            name: name.clone(),
            digest: Some(digest),
            is_executable: *is_executable,
            ..FileNode::default()
        });
    }
    let root = Directory {
        files: nodes,
        ..Directory::default()
    }
    .encode_to_vec();
    let digest = digest_of(&root);
    blobs.insert(label(&digest), root);
    (Arc::new(blobs), digest)
}

/// `count` files `f0`... of `size` bytes each; every seventh is executable.
fn many(count: usize, size: usize) -> (Blobs, Digest) {
    let files: Vec<_> = (0..count)
        .map(|i| (format!("f{i}"), content(i, size), i % 7 == 0))
        .collect();
    tree(&files)
}

fn frame(status: u8, length: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![status];
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

fn ok_frame(bytes: &[u8]) -> Vec<u8> {
    frame(0, bytes.len() as u64, bytes)
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// What the fake grant answers: a status, a body, and how many bytes of the body it promises
/// and never sends (it closes the connection instead).
struct Reply {
    status: u16,
    body: Vec<u8>,
    missing: usize,
}

impl Reply {
    fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            missing: 0,
        }
    }

    fn missing(mut self, missing: usize) -> Self {
        self.missing = missing;
        self
    }
}

/// The digests a batch request body names.
fn digests_of(body: &[u8]) -> Vec<String> {
    let text = std::str::from_utf8(body).expect("utf-8");
    let list = text
        .strip_prefix("{\"digests\":[")
        .and_then(|rest| rest.strip_suffix("]}"))
        .expect("the batch body shape");
    list.split(',')
        .map(|item| item.trim_matches('"').to_owned())
        .collect()
}

fn frames(blobs: &Blobs, wanted: &[String]) -> Reply {
    let body = wanted.iter().flat_map(|l| ok_frame(&blobs[l])).collect();
    Reply::new(200, body)
}

type BatchFn = Box<dyn Fn(&[String]) -> Reply + Send + Sync>;
type GetFn = Box<dyn Fn(&str, usize) -> Reply + Send + Sync>;

/// A fake grant: by default it answers every batch with an ok frame per digest and every GET with
/// the blob; `on_batch` and `on_get` replace those answers. A GET's function is given the label
/// and how many GETs of it came before.
struct Grant {
    blobs: Blobs,
    batch: Option<BatchFn>,
    get: Option<GetFn>,
}

impl Grant {
    fn new(blobs: &Blobs) -> Self {
        Self {
            blobs: Arc::clone(blobs),
            batch: None,
            get: None,
        }
    }

    fn on_batch(mut self, f: impl Fn(&[String]) -> Reply + Send + Sync + 'static) -> Self {
        self.batch = Some(Box::new(f));
        self
    }

    fn on_get(mut self, f: impl Fn(&str, usize) -> Reply + Send + Sync + 'static) -> Self {
        self.get = Some(Box::new(f));
        self
    }

    fn answer(&self, request: &Request, earlier: usize) -> Reply {
        if request.method == "POST" {
            let wanted = digests_of(&request.body);
            return match &self.batch {
                Some(f) => f(&wanted),
                None => frames(&self.blobs, &wanted),
            };
        }
        let wanted = request.path.trim_start_matches('/');
        match &self.get {
            Some(f) => f(wanted, earlier),
            None => Reply::new(200, self.blobs[wanted].clone()),
        }
    }
}

/// One request the fake saw.
struct Logged {
    method: String,
    path: String,
    authorization: String,
    content_type: String,
    body: Vec<u8>,
}

struct Server {
    base: String,
    log: Arc<Mutex<Vec<Logged>>>,
}

impl Server {
    fn start(grant: Grant) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let log: Arc<Mutex<Vec<Logged>>> = Arc::default();
        let grant = Arc::new(grant);
        let shared = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let (grant, log) = (Arc::clone(&grant), Arc::clone(&shared));
                let stream = stream.expect("accept");
                std::thread::spawn(move || connection(&stream, &grant, &log));
            }
        });
        Self { base, log }
    }

    fn source(&self) -> HttpBlobSource {
        HttpBlobSource::build(&self.base, "tok", &[], false)
    }

    /// The digests of each batch request, in the order they arrived.
    fn batches(&self) -> Vec<Vec<String>> {
        let log = self.log.lock().expect("log");
        log.iter()
            .filter(|r| r.method == "POST")
            .map(|r| digests_of(&r.body))
            .collect()
    }

    /// The labels of the GETs, in the order they arrived.
    fn gets(&self) -> Vec<String> {
        let log = self.log.lock().expect("log");
        log.iter()
            .filter(|r| r.method == "GET")
            .map(|r| r.path.trim_start_matches('/').to_owned())
            .collect()
    }
}

/// Serves requests on one connection until the client closes it or a reply is cut short.
fn connection(stream: &TcpStream, grant: &Grant, log: &Mutex<Vec<Logged>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut writer = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("request line") == 0 {
            return;
        }
        let mut words = line.split_whitespace();
        let method = words.next().expect("method").to_owned();
        let path = words.next().expect("path").to_owned();
        let (mut length, mut authorization, mut content_type) = (0, String::new(), String::new());
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("header");
            let header = header.trim().to_owned();
            if header.is_empty() {
                break;
            }
            let lower = header.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length: ") {
                length = value.parse().expect("length");
            }
            if lower.starts_with("authorization: ") {
                header["authorization: ".len()..].clone_into(&mut authorization);
            }
            if let Some(value) = lower.strip_prefix("content-type: ") {
                value.clone_into(&mut content_type);
            }
        }
        let mut body = vec![0; length];
        std::io::Read::read_exact(&mut reader, &mut body).expect("body");
        let request = Request { method, path, body };
        let earlier = {
            let mut log = log.lock().expect("log");
            let earlier = log
                .iter()
                .filter(|r| r.method == request.method && r.path == request.path)
                .count();
            log.push(Logged {
                method: request.method.clone(),
                path: request.path.clone(),
                authorization,
                content_type,
                body: request.body.clone(),
            });
            earlier
        };
        let reply = grant.answer(&request, earlier);
        let head = format!(
            "HTTP/1.1 {} Reply\r\nContent-Length: {}\r\n\r\n",
            reply.status,
            reply.body.len() + reply.missing
        );
        // The client may hang up on a body it has read enough of.
        let _ = writer.write_all(head.as_bytes());
        let _ = writer.write_all(&reply.body);
        if reply.missing > 0 {
            return;
        }
    }
}

fn batching(server: &Server) -> HttpBlobSource {
    server.source().with_batch(true)
}

fn check_files(dir: &Path, count: usize, size: usize) {
    for i in 0..count {
        let path = dir.join(format!("f{i}"));
        assert_eq!(
            std::fs::read(&path).expect("file"),
            content(i, size),
            "f{i}"
        );
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, if i % 7 == 0 { 0o755 } else { 0o644 }, "mode of f{i}");
    }
}

/// Catches many small files costing a request each, or a batch that is not the documented
/// request: 600 files of 1 KiB and their Directory arrive in four batch requests (the Directory,
/// then 256, 256 and 88 files), no GET is made, each request is a POST to `/batch` with the
/// bearer token and a JSON content type, and every file has its bytes and mode.
#[test]
fn many_small_files_are_fetched_in_few_batch_requests() {
    let (blobs, root) = many(600, 1024);
    let server = Server::start(Grant::new(&blobs));
    let scratch_dir = scratch("many");
    let dir = scratch_dir.join("in");
    materialize(&batching(&server), &root, &dir, 4).expect("materialize");
    check_files(&dir, 600, 1024);
    let mut sizes: Vec<usize> = server.batches().iter().map(Vec::len).collect();
    sizes.sort_unstable();
    assert_eq!(sizes, [1, 88, 256, 256]);
    assert!(server.gets().is_empty());
    let log = server.log.lock().expect("log");
    assert_eq!(log.len(), 4);
    for request in log.iter() {
        assert_eq!(request.path, "/batch");
        assert_eq!(request.authorization, "Bearer tok");
        assert_eq!(request.content_type, "application/json");
    }
}

/// Catches a batch that goes past 4 MiB of summed size: 100 files of 60,000 bytes (69 fit in a
/// batch) arrive in requests that each sum to at most 4 MiB, and every file is right.
#[test]
fn a_batch_never_asks_for_more_than_4_mib() {
    let (blobs, root) = many(100, 60_000);
    let server = Server::start(Grant::new(&blobs));
    let scratch_dir = scratch("bytes");
    let dir = scratch_dir.join("in");
    materialize(&batching(&server), &root, &dir, 1).expect("materialize");
    check_files(&dir, 100, 60_000);
    let batches = server.batches();
    assert_eq!(batches.len(), 3, "the Directory, then 69 and 31 files");
    for batch in &batches {
        let total: i64 = batch
            .iter()
            .map(|l| {
                l.rsplit('/')
                    .next()
                    .expect("size")
                    .parse::<i64>()
                    .expect("int")
            })
            .sum();
        assert!(total <= MAX_BATCH_BYTES, "{total}");
        assert!(batch.len() <= MAX_BATCH_DIGESTS);
    }
}

fn sized(sizes: &[i64]) -> Vec<Digest> {
    sizes
        .iter()
        .enumerate()
        .map(|(i, &size)| Digest {
            hash: format!("{i:064x}"),
            size_bytes: size,
        })
        .collect()
}

fn batch_lengths(units: &[Unit]) -> Vec<usize> {
    units
        .iter()
        .map(|unit| match unit {
            Unit::Batch(indexes) => indexes.len(),
            Unit::Get(_) => 0,
        })
        .collect()
}

/// Catches an off-by-one in the grouping: 256 entries make one batch and 257 two; sizes summing
/// to exactly 4 MiB make one batch and one byte more start another; blobs of 64 KiB and over, and
/// malformed digests, are single GETs placed after the batches, and no batch is empty.
#[test]
fn the_grouping_respects_both_limits_exactly() {
    assert_eq!(batch_lengths(&plan(&sized(&[1; 256]))), [256]);
    assert_eq!(batch_lengths(&plan(&sized(&[1; 257]))), [256, 1]);
    let mut exact = vec![65_535; 64];
    exact.push(64);
    assert_eq!(batch_lengths(&plan(&sized(&exact))), [65]);
    exact.push(1);
    assert_eq!(batch_lengths(&plan(&sized(&exact))), [65, 1]);
    assert_eq!(batch_lengths(&plan(&sized(&[]))), Vec::<usize>::new());
    let mut digests = sized(&[10, 65_536, 65_535, 70_000, -1, 0]);
    digests[0].hash = "short".to_owned();
    assert_eq!(
        plan(&digests),
        [
            Unit::Batch(vec![2, 5]),
            Unit::Get(0),
            Unit::Get(1),
            Unit::Get(3),
            Unit::Get(4)
        ]
    );
}

/// Catches the size threshold being wrong or ignored: of a 65,535-byte, a 65,536-byte and a
/// 100 KiB file, only the first is asked for in a batch; the others are GETs.
#[test]
fn blobs_of_64_kib_and_over_go_by_get() {
    let files = [
        ("a".to_owned(), content(1, 65_535), false),
        ("b".to_owned(), content(2, 65_536), false),
        ("c".to_owned(), content(3, 100 << 10), false),
    ];
    let (blobs, root) = tree(&files);
    let server = Server::start(Grant::new(&blobs));
    let scratch_dir = scratch("threshold");
    let dir = scratch_dir.join("in");
    materialize(&batching(&server), &root, &dir, 2).expect("materialize");
    for (name, bytes, _) in &files {
        assert!(
            std::fs::read(dir.join(name)).expect("file") == *bytes,
            "{name}"
        );
    }
    let small = label(&digest_of(&files[0].1));
    let batched: Vec<String> = server.batches().concat();
    assert_eq!(batched.len(), 2);
    assert!(batched.contains(&small));
    let mut gets = server.gets();
    gets.sort();
    let mut expected = vec![
        label(&digest_of(&files[1].1)),
        label(&digest_of(&files[2].1)),
    ];
    expected.sort();
    assert_eq!(gets, expected);
}

/// The file `f` of 500 bytes, alone in its tree.
fn one_file() -> (Blobs, Digest, Vec<u8>, String) {
    let bytes = content(9, 500);
    let (blobs, root) = tree(&[("f".to_owned(), bytes.clone(), false)]);
    let target = label(&digest_of(&bytes));
    (blobs, root, bytes, target)
}

/// A batch answer that gives `frame_for_target` for the file's blob and ok frames for the rest.
fn target_frame(
    blobs: &Blobs,
    target: &str,
    frame_for_target: Vec<u8>,
) -> impl Fn(&[String]) -> Reply + use<> {
    let (blobs, target) = (Arc::clone(blobs), target.to_owned());
    move |wanted| {
        let body = wanted
            .iter()
            .flat_map(|l| {
                if *l == target {
                    frame_for_target.clone()
                } else {
                    ok_frame(&blobs[l])
                }
            })
            .collect();
        Reply::new(200, body)
    }
}

/// Catches a "too large" frame being fatal or skipped: the blob is fetched by GET, once, and the
/// file is complete. A "store unavailable" frame is retried by GET the same way.
#[test]
fn a_too_large_or_unavailable_frame_is_refetched_by_get() {
    for status in [3, 4] {
        let (blobs, root, bytes, target) = one_file();
        let answer = target_frame(&blobs, &target, frame(status, 0, b""));
        let server = Server::start(Grant::new(&blobs).on_batch(answer));
        let scratch_dir = scratch(&format!("refetch{status}"));
        let dir = scratch_dir.join("in");
        materialize(&batching(&server), &root, &dir, 2).expect("materialize");
        assert_eq!(
            std::fs::read(dir.join("f")).expect("file"),
            bytes,
            "{status}"
        );
        assert_eq!(server.gets(), [target], "{status}");
    }
}

/// Catches a second failure after "store unavailable" being retried again or swallowed: the one
/// GET fails, the fetch fails as a source error naming the blob, and nothing is left at the path.
#[test]
fn a_failed_retry_after_store_unavailable_fails_the_fetch() {
    let (blobs, root, _, target) = one_file();
    let answer = target_frame(&blobs, &target, frame(4, 0, b""));
    let server = Server::start(
        Grant::new(&blobs)
            .on_batch(answer)
            .on_get(|_, _| Reply::new(503, Vec::new())),
    );
    let scratch_dir = scratch("retry-fails");
    let dir = scratch_dir.join("in");
    let error = materialize(&batching(&server), &root, &dir, 2).expect_err("fails");
    assert!(matches!(&error, FetchError::Source { .. }), "{error:?}");
    assert!(error.to_string().contains(&target), "{error}");
    assert!(!dir.join("f").exists());
    assert_eq!(server.gets(), [target]);
}

/// Catches "not found" and "out of scope" frames being ignored, retried or reported with the URL
/// or token: each fails the fetch as a source error naming the blob, leaves nothing at the path,
/// and makes no GET.
#[test]
fn a_blob_the_grant_lacks_or_withholds_fails_the_fetch() {
    for status in [1, 2] {
        let (blobs, root, _, target) = one_file();
        let answer = target_frame(&blobs, &target, frame(status, 0, b""));
        let server = Server::start(Grant::new(&blobs).on_batch(answer));
        let scratch_dir = scratch(&format!("refused{status}"));
        let dir = scratch_dir.join("in");
        let error = materialize(&batching(&server), &root, &dir, 2).expect_err("fails");
        let text = error.to_string();
        assert!(matches!(&error, FetchError::Source { .. }), "{error:?}");
        assert!(text.contains(&target), "{text}");
        assert!(!text.contains(&server.base), "{text}");
        assert!(!text.contains("tok"), "{text}");
        assert!(!dir.join("f").exists());
        assert!(server.gets().is_empty());
    }
}

/// Catches an ok frame being written without the hash check: bytes of the right length that do
/// not hash to the digest fail the fetch as a mismatch and leave no file.
#[test]
fn a_corrupt_ok_frame_fails_and_leaves_no_file() {
    let (blobs, root, bytes, target) = one_file();
    let mut wrong = bytes;
    wrong[0] ^= 1;
    let answer = target_frame(&blobs, &target, ok_frame(&wrong));
    let server = Server::start(Grant::new(&blobs).on_batch(answer));
    let scratch_dir = scratch("corrupt");
    let dir = scratch_dir.join("in");
    let error = materialize(&batching(&server), &root, &dir, 2).expect_err("fails");
    assert!(matches!(&error, FetchError::Mismatch { .. }), "{error:?}");
    assert!(!dir.join("f").exists());
}

/// Catches a response that breaks the framing being accepted. Each answer for the file's blob is
/// a source error saying the response is malformed, and a file that failed is not left behind (the
/// bytes after the last frame come after a complete, checked file, which stays).
#[test]
fn a_malformed_batch_response_fails_the_fetch() {
    let (blobs, root, bytes, target) = one_file();
    let size = bytes.len() as u64;
    let full = ok_frame(&bytes);
    let mut trailing = full.clone();
    trailing.push(7);
    let cut_body = full[..full.len() - 10].to_vec();
    let cases: Vec<(&str, Reply, bool)> = vec![
        ("no frame", Reply::new(200, Vec::new()), false),
        ("short header", Reply::new(200, vec![0, 0, 0]), false),
        (
            "short body",
            Reply::new(200, frame(0, size, &bytes[..400])),
            false,
        ),
        (
            "ok length below size",
            Reply::new(200, frame(0, size - 1, &bytes[..499])),
            false,
        ),
        (
            "ok length above size",
            Reply::new(200, frame(0, size + 1, &content(1, 501))),
            false,
        ),
        (
            "length on a refusal",
            Reply::new(200, frame(1, 1, b"x")),
            false,
        ),
        (
            "length on too large",
            Reply::new(200, frame(3, size, &bytes)),
            false,
        ),
        ("unknown status", Reply::new(200, frame(5, 0, b"")), false),
        ("trailing bytes", Reply::new(200, trailing), true),
        (
            "cut in a header",
            Reply::new(200, vec![0, 0, 0]).missing(20),
            false,
        ),
        (
            "cut in a body",
            Reply::new(200, cut_body).missing(10),
            false,
        ),
        (
            "cut after the last frame",
            Reply::new(200, full).missing(5),
            true,
        ),
    ];
    for (name, reply, complete) in cases {
        let answer = Arc::new(Mutex::new(Some(reply)));
        let (all, target) = (Arc::clone(&blobs), target.clone());
        let server = Server::start(Grant::new(&blobs).on_batch(move |wanted| {
            if wanted.contains(&target) {
                return answer.lock().expect("reply").take().expect("one answer");
            }
            frames(&all, wanted)
        }));
        let scratch_dir = scratch("malformed");
        let dir = scratch_dir.join("in");
        let error = materialize(&batching(&server), &root, &dir, 1).expect_err(name);
        assert!(
            matches!(&error, FetchError::Source { .. }),
            "{name}: {error:?}"
        );
        assert!(error.to_string().contains("malformed"), "{name}: {error}");
        assert_eq!(dir.join("f").exists(), complete, "{name}");
    }
}

/// Catches a server without the route breaking the run, or the client asking again: after a 404
/// or 405 for the first batch every blob (the Directory, then 300 files) comes by GET, only that
/// one batch request is made, and the tree is right.
#[test]
fn a_missing_batch_route_falls_back_to_get_for_the_rest_of_the_run() {
    for status in [404, 405] {
        let (blobs, root) = many(300, 100);
        let server = Server::start(
            Grant::new(&blobs)
                .on_batch(move |_| Reply::new(status, b"{\"reason\":\"x\"}".to_vec())),
        );
        let scratch_dir = scratch(&format!("fallback{status}"));
        let dir = scratch_dir.join("in");
        materialize(&batching(&server), &root, &dir, 4).expect("materialize");
        check_files(&dir, 300, 100);
        assert_eq!(server.batches().len(), 1, "{status}");
        assert_eq!(server.gets().len(), 301, "{status}");
    }
}

/// Catches a client or token fault being papered over with a fallback: a 400, 401, 403, 413 or
/// 500 for a whole batch fails the fetch as a source error that says the status and does not
/// contain the token or the address, and no GET follows.
#[test]
fn other_whole_request_refusals_fail_the_fetch() {
    for status in [400, 401, 403, 413, 500] {
        let (blobs, root) = many(3, 100);
        let server = Server::start(
            Grant::new(&blobs)
                .on_batch(move |_| Reply::new(status, b"{\"reason\":\"x\"}".to_vec())),
        );
        let scratch_dir = scratch(&format!("refused-all{status}"));
        let error =
            materialize(&batching(&server), &root, &scratch_dir.join("in"), 2).expect_err("fails");
        let text = error.to_string();
        assert!(matches!(&error, FetchError::Source { .. }), "{error:?}");
        assert!(text.contains(&format!("answered {status}")), "{text}");
        assert!(!text.contains("tok"), "{text}");
        assert!(!text.contains(&server.base), "{text}");
        assert!(server.gets().is_empty(), "{status}");
    }
}

/// Catches batching being on without the flag: with the batch route off, the default, a tree of
/// small files is fetched by one GET per blob and no POST is made.
#[test]
fn without_the_flag_no_batch_request_is_made() {
    let (blobs, root) = many(20, 100);
    let server = Server::start(Grant::new(&blobs));
    let source = server.source();
    assert!(!source.batching());
    let scratch_dir = scratch("off");
    let dir = scratch_dir.join("in");
    materialize(&source, &root, &dir, 4).expect("materialize");
    check_files(&dir, 20, 100);
    assert!(server.batches().is_empty());
    assert_eq!(server.gets().len(), 21);
}

/// Catches duplicates being dropped or a malformed digest reaching the server: a digest asked for
/// twice is answered twice, in order, in one request, and a malformed digest is refused as a bad
/// digest without any request.
#[test]
fn duplicates_are_answered_each_time_and_a_bad_digest_is_refused() {
    let (blobs, _, bytes, target) = one_file();
    let server = Server::start(Grant::new(&blobs));
    let digest = digest_of(&bytes);
    let seen: Mutex<Vec<(usize, Vec<u8>)>> = Mutex::new(Vec::new());
    let collect = |index: usize, fetch: Fetch<'_>| -> Result<(), FetchError> {
        let mut out = Vec::new();
        fetch(&mut out)?;
        seen.lock().expect("seen").push((index, out));
        Ok(())
    };
    batching(&server)
        .fetch_many(&[digest.clone(), digest], 4, &collect)
        .expect("fetch");
    let mut seen = seen.into_inner().expect("seen");
    seen.sort();
    assert_eq!(seen, [(0, bytes.clone()), (1, bytes)]);
    assert_eq!(server.batches(), [[target.clone(), target]]);
    let bad = Digest {
        hash: "nope".to_owned(),
        size_bytes: 3,
    };
    let error = batching(&server)
        .fetch_many(&[bad], 4, &|_, fetch| fetch(&mut Vec::new()))
        .expect_err("bad digest");
    assert!(matches!(&error, FetchError::BadDigest(_)), "{error:?}");
    assert_eq!(server.log.lock().expect("log").len(), 1);
}

/// Catches a batch route that is unreachable being reported as success: a refused connection is a
/// source error that names the batch.
#[test]
fn an_unreachable_grant_fails_a_batch_as_a_source_error() {
    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", closed.local_addr().expect("addr"));
    drop(closed);
    let source = HttpBlobSource::build(&base, "tok", &[], false).with_batch(true);
    let digest = digest_of(b"x");
    let error = source
        .fetch_many(&[digest], 1, &|_, fetch| fetch(&mut Vec::new()))
        .expect_err("refused");
    assert!(matches!(&error, FetchError::Source { .. }), "{error:?}");
    assert!(error.to_string().contains("batch of 1"), "{error}");
}
