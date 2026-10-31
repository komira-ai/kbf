use kbf_testkit::dir::TestDir;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::net::TcpListener;
use std::os::unix::fs::MetadataExt as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use kbf_proto::reapi::{Directory, DirectoryNode, FileNode, SymlinkNode};

use super::*;

fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as i64,
    }
}

/// Holds requests for gated blobs until `need` of them are in flight at once; later ones pass.
/// The release is decided by counting arrivals, never by time. Code that never runs `need`
/// requests together hangs the test here; the nextest profile's `terminate-after`
/// (.config/nextest.toml) ends the hang and fails the test.
#[derive(Default)]
struct Rendezvous {
    need: usize,
    arrived: Mutex<usize>,
    open: Condvar,
}

impl Rendezvous {
    fn new(need: usize) -> Self {
        Self {
            need,
            ..Self::default()
        }
    }

    fn arrive(&self) {
        let mut arrived = self.arrived.lock().expect("arrived");
        *arrived += 1;
        self.open.notify_all();
        let _held = self
            .open
            .wait_while(arrived, |arrived| *arrived < self.need)
            .expect("wait");
    }
}

/// Blobs by hash; `reads` counts the `get`s per hash, `in_flight` and `most` the requests
/// being served at once now and at the most, and `gate` holds requests for the blobs named in
/// `gated`.
#[derive(Default)]
struct Memory {
    blobs: HashMap<String, Vec<u8>>,
    reads: Mutex<HashMap<String, usize>>,
    in_flight: AtomicUsize,
    most: AtomicUsize,
    gated: HashSet<String>,
    gate: Rendezvous,
}

impl Memory {
    fn put(&mut self, bytes: Vec<u8>) -> Digest {
        let digest = digest_of(&bytes);
        self.blobs.insert(digest.hash.clone(), bytes);
        digest
    }

    fn put_directory(&mut self, directory: &Directory) -> Digest {
        self.put(directory.encode_to_vec())
    }

    fn reads(&self, digest: &Digest) -> usize {
        self.reads
            .lock()
            .expect("reads")
            .get(&digest.hash)
            .copied()
            .unwrap_or(0)
    }

    fn most_in_flight(&self) -> usize {
        self.most.load(Ordering::SeqCst)
    }
}

impl BlobSource for Memory {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        *self
            .reads
            .lock()
            .expect("reads")
            .entry(digest.hash.clone())
            .or_default() += 1;
        if self.gated.contains(&digest.hash) {
            self.gate.arrive();
        }
        let found = self
            .blobs
            .get(&digest.hash)
            .cloned()
            .ok_or_else(|| FetchError::Source {
                digest: label(digest),
                why: "absent".to_owned(),
            });
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        found
    }
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-task-fetch-{name}"))
}

fn file(name: &str, digest: &Digest, is_executable: bool) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(digest.clone()),
        is_executable,
        ..FileNode::default()
    }
}

fn sub(name: &str, digest: &Digest) -> DirectoryNode {
    DirectoryNode {
        name: name.to_owned(),
        digest: Some(digest.clone()),
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// root: bin/run (executable), a.txt, copy.txt (same blob as a.txt), link -> a.txt,
/// twice/one and twice/two (the same Directory), deep/x/y/z.txt.
fn tree() -> (Memory, Digest) {
    let mut mem = Memory::default();
    let text = mem.put(b"hello".to_vec());
    let script = mem.put(b"tool bytes\n".to_vec());
    let empty = mem.put(Vec::new());
    let bin = mem.put_directory(&Directory {
        files: vec![file("run", &script, true)],
        ..Directory::default()
    });
    let shared = mem.put_directory(&Directory {
        files: vec![file("e", &empty, false)],
        ..Directory::default()
    });
    let z = mem.put_directory(&Directory {
        files: vec![file("z.txt", &text, false)],
        ..Directory::default()
    });
    let y = mem.put_directory(&Directory {
        directories: vec![sub("y", &z)],
        ..Directory::default()
    });
    let x = mem.put_directory(&Directory {
        directories: vec![sub("x", &y)],
        ..Directory::default()
    });
    let root = mem.put_directory(&Directory {
        files: vec![file("a.txt", &text, false), file("copy.txt", &text, false)],
        directories: vec![
            sub("bin", &bin),
            sub("one", &shared),
            sub("two", &shared),
            sub("deep", &x),
        ],
        symlinks: vec![SymlinkNode {
            name: "link".to_owned(),
            target: "a.txt".to_owned(),
            ..SymlinkNode::default()
        }],
        ..Directory::default()
    });
    (mem, root)
}

/// Catches a tree laid out wrongly: a nested directory, the executable bit and the plain
/// mode, a symlink kept as one, two files of one blob each filled, and a blob or Directory
/// read more than once.
#[test]
fn a_nested_tree_is_laid_out_with_modes_and_symlinks() {
    let (mem, root) = tree();
    let scratch_dir = scratch("layout");
    let dir = scratch_dir.join("in");
    materialize(&mem, &root, &dir, 4).expect("materialize");
    assert_eq!(std::fs::read(dir.join("a.txt")).expect("a"), b"hello");
    assert_eq!(std::fs::read(dir.join("copy.txt")).expect("copy"), b"hello");
    assert_eq!(
        std::fs::read(dir.join("deep/x/y/z.txt")).expect("z"),
        b"hello"
    );
    assert_eq!(
        std::fs::read(dir.join("bin/run")).expect("run"),
        b"tool bytes\n"
    );
    assert_eq!(mode(&dir.join("bin/run")), 0o755);
    assert_eq!(mode(&dir.join("a.txt")), 0o644);
    assert_eq!(
        std::fs::read_link(dir.join("link")).expect("link"),
        Path::new("a.txt")
    );
    assert!(dir.join("one/e").is_file());
    assert!(dir.join("two/e").is_file());
    assert_eq!(mem.reads(&digest_of(b"hello")), 1);
    let shared = digest_of(
        &Directory {
            files: vec![file("e", &digest_of(b""), false)],
            ..Directory::default()
        }
        .encode_to_vec(),
    );
    assert_eq!(mem.reads(&shared), 1);
}

/// Catches a blob accepted though its bytes are not the ones its digest names: wrong
/// content of the right length, wrong length, and a missing blob.
#[test]
fn a_blob_that_does_not_match_its_digest_is_refused() {
    let mut mem = Memory::default();
    let good = digest_of(b"abc");
    mem.blobs.insert(good.hash.clone(), b"abd".to_vec());
    let error = mem.verified(&good).expect_err("same length, other bytes");
    assert!(matches!(&error, FetchError::Mismatch { .. }), "{error:?}");
    assert!(error.to_string().contains(&label(&good)), "{error}");
    assert!(error.to_string().contains("SHA-256"), "{error}");

    mem.blobs.insert(good.hash.clone(), b"abcd".to_vec());
    let error = mem.verified(&good).expect_err("longer");
    assert!(error.to_string().contains("longer than 3 bytes"), "{error}");

    let absent = digest_of(b"nothing");
    let error = mem.verified(&absent).expect_err("absent");
    assert!(matches!(error, FetchError::Source { .. }), "{error:?}");

    mem.blobs.insert(good.hash.clone(), b"abc".to_vec());
    assert_eq!(mem.verified(&good).expect("good"), b"abc");
}

/// Catches a corrupt blob inside a tree being written: the file blob is checked, and so is a
/// Directory message.
#[test]
fn a_corrupt_blob_in_a_tree_fails_materialize() {
    let (mut mem, root) = tree();
    let text = digest_of(b"hello");
    mem.blobs.insert(text.hash.clone(), b"HELLO".to_vec());
    let error = materialize(&mem, &root, &scratch("corrupt-file"), 4).expect_err("file");
    assert!(matches!(error, FetchError::Mismatch { .. }), "{error:?}");

    let (mut mem, root) = tree();
    mem.blobs.insert(root.hash.clone(), b"junk".to_vec());
    let error = materialize(&mem, &root, &scratch("corrupt-dir"), 4).expect_err("directory");
    assert!(matches!(error, FetchError::Mismatch { .. }), "{error:?}");
}

/// Catches a Directory entry name that could write outside the directory or collide:
/// slash, `..`, `.`, empty, NUL, a name twice (also across files and directories).
#[test]
fn a_bad_name_is_refused() {
    let names = ["a/b", "..", ".", "", "a\0b", "dup"];
    for (i, name) in names.iter().enumerate() {
        let mut mem = Memory::default();
        let blob = mem.put(b"x".to_vec());
        let empty = mem.put_directory(&Directory::default());
        let mut directory = Directory {
            files: vec![file(name, &blob, false)],
            ..Directory::default()
        };
        if *name == "dup" {
            directory.directories.push(sub("dup", &empty));
        }
        let root = mem.put_directory(&directory);
        let error = materialize(&mem, &root, &scratch(&format!("name{i}")), 4).expect_err(name);
        assert!(
            matches!(error, FetchError::Invalid(_)),
            "{name:?}: {error:?}"
        );
    }
}

/// Catches a file and a symlink of one name in one Directory being accepted.
#[test]
fn a_symlink_and_a_file_of_one_name_are_refused() {
    let mut mem = Memory::default();
    let blob = mem.put(b"x".to_vec());
    let root = mem.put_directory(&Directory {
        files: vec![file("clash", &blob, false)],
        symlinks: vec![SymlinkNode {
            name: "clash".to_owned(),
            target: "elsewhere".to_owned(),
            ..SymlinkNode::default()
        }],
        ..Directory::default()
    });
    let error = materialize(&mem, &root, &scratch("clash"), 4).expect_err("clash");
    assert!(matches!(&error, FetchError::Invalid(_)), "{error:?}");
    assert!(error.to_string().contains("twice"), "{error}");
}

/// Catches an entry without a digest, and a blob that is not a Directory where one is wanted.
#[test]
fn a_malformed_tree_is_refused() {
    let mut mem = Memory::default();
    let no_file_digest = mem.put_directory(&Directory {
        files: vec![FileNode {
            name: "f".to_owned(),
            ..FileNode::default()
        }],
        ..Directory::default()
    });
    let no_dir_digest = mem.put_directory(&Directory {
        directories: vec![DirectoryNode {
            name: "d".to_owned(),
            digest: None,
        }],
        ..Directory::default()
    });
    let not_a_directory = mem.put(vec![0xff, 0xff, 0xff]);
    for (i, root) in [no_file_digest, no_dir_digest, not_a_directory]
        .iter()
        .enumerate()
    {
        let error =
            materialize(&mem, root, &scratch(&format!("malformed{i}")), 4).expect_err("bad");
        assert!(matches!(error, FetchError::Invalid(_)), "{i}: {error:?}");
    }
}

/// Catches a tree written over something already there (`O_EXCL`), and an unwritable target.
#[test]
fn writing_does_not_overwrite_or_follow() {
    let (mem, root) = tree();
    let dir = scratch("exists");
    std::fs::write(dir.join("a.txt"), b"mine").expect("pre-existing");
    let error = materialize(&mem, &root, &dir, 4).expect_err("exists");
    assert!(matches!(error, FetchError::Io { .. }), "{error:?}");
    assert_eq!(std::fs::read(dir.join("a.txt")).expect("kept"), b"mine");

    // The directory itself cannot be made under a file.
    let under_file = dir.join("a.txt").join("in");
    let error = materialize(&mem, &root, &under_file, 4).expect_err("under a file");
    assert!(matches!(error, FetchError::Io { .. }), "{error:?}");

    // A symlink in the tree cannot be made where a directory entry already is.
    let dir = scratch("link-exists");
    std::fs::write(dir.join("link"), b"x").expect("pre-existing");
    let error = materialize(&mem, &root, &dir, 4).expect_err("link exists");
    assert!(matches!(error, FetchError::Io { .. }), "{error:?}");
}

/// Catches a digest that could reach a URL or path unchecked.
#[test]
fn a_malformed_digest_is_refused() {
    let mem = Memory::default();
    let good = digest_of(b"x");
    let cases = [
        Digest {
            hash: "../etc".to_owned(),
            size_bytes: 1,
        },
        Digest {
            hash: good.hash.to_uppercase(),
            size_bytes: 1,
        },
        Digest {
            hash: good.hash.clone(),
            size_bytes: -1,
        },
    ];
    for digest in cases {
        let error = mem.verified(&digest).expect_err("bad digest");
        assert!(matches!(error, FetchError::BadDigest(_)), "{error:?}");
    }
}

/// Catches working-directory and output paths that leave the tree.
#[test]
fn relative_paths_are_checked() {
    for ok in ["", "a", "a/b.c", "a/.hidden"] {
        check_relative("path", ok).expect(ok);
    }
    for bad in ["/a", "a//b", "a/", ".", "..", "a/../b", "a/./b", "a\0b"] {
        let error = check_relative("output path", bad).expect_err(bad);
        assert!(error.to_string().contains("output path"), "{error}");
    }
}

/// One request the test server saw: its request line and its Authorization header.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// Serves `answers` (status line and body), one connection each, in order, over plain HTTP;
/// returns the base URL and what it saw.
fn serve(answers: Vec<(&'static str, Vec<u8>)>) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for (status, body) in answers {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let (mut line, mut auth) = (String::new(), String::new());
            reader.read_line(&mut line).expect("request line");
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("header");
                if header.trim().is_empty() {
                    break;
                }
                if let Some(value) = header.strip_prefix("authorization: ") {
                    value.trim().clone_into(&mut auth);
                }
            }
            log.lock()
                .expect("log")
                .push((line.trim().to_owned(), auth));
            let mut stream = stream;
            // The client may hang up on a body it has read enough of.
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (base, seen)
}

fn plain(base: &str) -> HttpBlobSource {
    HttpBlobSource::build(&format!("{base}/"), "tok", &[], false)
}

/// Catches a wrong URL shape or a missing bearer: the grant sees `GET /<hex>/<size>` and the
/// token, the trailing slash of the base does not double, and the body comes back whole.
#[test]
fn the_http_source_asks_the_grant_with_the_token() {
    let blob = b"payload".to_vec();
    let digest = digest_of(&blob);
    let (base, seen) = serve(vec![("200 OK", blob.clone())]);
    let got = plain(&base).verified(&digest).expect("fetch");
    assert_eq!(got, blob);
    let seen = seen.lock().expect("seen");
    assert_eq!(
        seen[0],
        (
            format!("GET /{}/{} HTTP/1.1", digest.hash, digest.size_bytes),
            "Bearer tok".to_owned()
        )
    );
}

/// Catches a non-200 taken for a blob, a body longer than its digest taken whole, and an
/// unreachable grant reported as anything but a source error naming the digest.
#[test]
fn the_http_source_refuses_bad_answers() {
    let digest = digest_of(b"abc");
    let (base, _) = serve(vec![
        ("403 Forbidden", b"no".to_vec()),
        ("200 OK", vec![b'a'; 100_000]),
    ]);
    let source = plain(&base);
    let error = source.verified(&digest).expect_err("403");
    assert!(matches!(&error, FetchError::Source { .. }), "{error:?}");
    assert!(error.to_string().contains("403"), "{error}");
    assert!(error.to_string().contains(&label(&digest)), "{error}");
    let error = source.verified(&digest).expect_err("too long");
    assert!(matches!(&error, FetchError::Mismatch { .. }), "{error:?}");
    assert!(error.to_string().contains("longer than 3 bytes"), "{error}");

    let closed = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", closed.local_addr().expect("addr"));
    drop(closed);
    let error = plain(&base).verified(&digest).expect_err("unreachable");
    assert!(matches!(error, FetchError::Source { .. }), "{error:?}");
}

/// Catches a body that ends before its Content-Length being taken for a blob, or reported as
/// anything but a source error naming the digest.
#[test]
fn a_body_cut_short_is_a_source_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort");
    });
    let digest = digest_of(&[b'a'; 100]);
    let error = plain(&base).get(&digest).expect_err("cut short");
    assert!(matches!(&error, FetchError::Source { .. }), "{error:?}");
    assert!(error.to_string().contains(&label(&digest)), "{error}");
}

/// Catches a digest reaching the URL unchecked when `get` is called directly.
#[test]
fn the_http_source_checks_the_digest_it_is_asked_for() {
    let bad = Digest {
        hash: "x/../y".to_owned(),
        size_bytes: 1,
    };
    let error = plain("http://127.0.0.1:1").get(&bad).expect_err("bad");
    assert!(matches!(error, FetchError::BadDigest(_)), "{error:?}");
}

/// Catches a source built with no roots to trust, and the redacted Debug.
#[test]
fn the_https_source_needs_roots_and_hides_its_token() {
    let error = HttpBlobSource::new("https://grant.example", "secret", &[]).expect_err("none");
    assert!(matches!(error, FetchError::Invalid(_)), "{error:?}");
    let source = HttpBlobSource::new("https://grant.example/", "secret", &[vec![1, 2, 3]])
        .expect("one root");
    let shown = format!("{source:?}");
    assert!(shown.contains("https://grant.example"), "{shown}");
    assert!(!shown.contains("secret"), "{shown}");
    // Plain HTTP is refused by a source built with `new`.
    let digest = digest_of(b"x");
    let error = HttpBlobSource::new("http://127.0.0.1:1", "t", &[vec![1]])
        .expect("source")
        .get(&digest)
        .expect_err("http");
    assert!(matches!(error, FetchError::Source { .. }), "{error:?}");
    let _ = native_roots();
}

/// A root of `siblings` distinct directories, each holding one file of its own: returns the
/// source, the root, the digests of the sibling Directory messages and of the files.
fn wide(siblings: usize) -> (Memory, Digest, Vec<String>, Vec<String>) {
    let mut mem = Memory::default();
    let (mut directories, mut files, mut nodes) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..siblings {
        let blob = mem.put(format!("file number {i}").into_bytes());
        let directory = mem.put_directory(&Directory {
            files: vec![file("f", &blob, false)],
            ..Directory::default()
        });
        nodes.push(sub(&format!("d{i}"), &directory));
        files.push(blob.hash);
        directories.push(directory.hash);
    }
    let root = mem.put_directory(&Directory {
        directories: nodes,
        ..Directory::default()
    });
    (mem, root, directories, files)
}

fn check_wide(dir: &Path, siblings: usize) {
    for i in 0..siblings {
        assert_eq!(
            std::fs::read(dir.join(format!("d{i}/f"))).expect("file"),
            format!("file number {i}").into_bytes()
        );
    }
}

/// Catches a tree with shared subtrees and duplicate blobs laid out wrongly or fetched more
/// than once: every file has its bytes and its own mode (a blob named by an executable and a
/// plain path gets both modes), and every Directory and every distinct blob is requested
/// exactly once.
#[test]
fn a_tree_with_shared_subtrees_and_duplicate_blobs_is_fetched_once_each() {
    let mut mem = Memory::default();
    let tool = mem.put(b"tool".to_vec());
    let same = mem.put(b"same".to_vec());
    let other = mem.put(b"other".to_vec());
    let shared = mem.put_directory(&Directory {
        files: vec![file("s", &same, false), file("t", &tool, true)],
        ..Directory::default()
    });
    let deep = mem.put_directory(&Directory {
        files: vec![file("x", &same, false), file("y", &other, false)],
        ..Directory::default()
    });
    let c = mem.put_directory(&Directory {
        directories: vec![sub("deep", &deep), sub("shared", &shared)],
        ..Directory::default()
    });
    let root = mem.put_directory(&Directory {
        files: vec![
            file("tool", &tool, true),
            file("tool-plain", &tool, false),
            file("same1", &same, false),
            file("same2", &same, false),
        ],
        directories: vec![sub("a", &shared), sub("b", &shared), sub("c", &c)],
        ..Directory::default()
    });
    let scratch_dir = scratch("shared");
    let dir = scratch_dir.join("in");
    materialize(&mem, &root, &dir, 4).expect("materialize");
    for (path, bytes, executable) in [
        ("tool", "tool", true),
        ("tool-plain", "tool", false),
        ("same1", "same", false),
        ("same2", "same", false),
        ("a/s", "same", false),
        ("a/t", "tool", true),
        ("b/s", "same", false),
        ("b/t", "tool", true),
        ("c/deep/x", "same", false),
        ("c/deep/y", "other", false),
        ("c/shared/s", "same", false),
        ("c/shared/t", "tool", true),
    ] {
        assert_eq!(std::fs::read(dir.join(path)).expect(path), bytes.as_bytes());
        assert_eq!(
            mode(&dir.join(path)),
            if executable { 0o755 } else { 0o644 }
        );
    }
    // Each path is a file of its own: a write through one does not show through another.
    let (one, two) = (dir.join("same1"), dir.join("same2"));
    assert_ne!(
        std::fs::metadata(&one).expect("one").ino(),
        std::fs::metadata(&two).expect("two").ino()
    );
    std::fs::write(&one, b"changed").expect("write");
    assert_eq!(std::fs::read(&two).expect("two"), b"same");
    let reads = mem.reads.lock().expect("reads");
    assert_eq!(reads.len(), mem.blobs.len(), "every blob is requested");
    for (hash, count) in reads.iter() {
        assert_eq!(*count, 1, "{hash} requested more than once");
    }
}

/// Catches the walk ignoring the limit: with 4 allowed, the 8 sibling Directory messages are
/// held until 4 are being fetched at once, so at least 4 run together, and no more than 4 ever.
#[test]
fn directories_are_fetched_four_at_a_time_when_four_are_allowed() {
    let (mut mem, root, directories, _) = wide(8);
    mem.gated = directories.into_iter().collect();
    mem.gate = Rendezvous::new(4);
    let scratch_dir = scratch("dirs4");
    let dir = scratch_dir.join("in");
    materialize(&mem, &root, &dir, 4).expect("materialize");
    check_wide(&dir, 8);
    assert_eq!(mem.most_in_flight(), 4);
}

/// Catches the file phase ignoring the limit, the same way for the 8 file blobs.
#[test]
fn file_blobs_are_fetched_four_at_a_time_when_four_are_allowed() {
    let (mut mem, root, _, files) = wide(8);
    mem.gated = files.into_iter().collect();
    mem.gate = Rendezvous::new(4);
    let scratch_dir = scratch("files4");
    let dir = scratch_dir.join("in");
    materialize(&mem, &root, &dir, 4).expect("materialize");
    check_wide(&dir, 8);
    assert_eq!(mem.most_in_flight(), 4);
}

/// Catches a limit of 1 not being one (and a limit of 0 not being taken as 1): one request at
/// a time, and the tree still complete.
#[test]
fn a_limit_of_one_fetches_one_at_a_time() {
    for limit in [1, 0] {
        let (mem, root, _, _) = wide(6);
        let scratch_dir = scratch(&format!("one{limit}"));
        let dir = scratch_dir.join("in");
        materialize(&mem, &root, &dir, limit).expect("materialize");
        check_wide(&dir, 6);
        assert_eq!(mem.most_in_flight(), 1);
    }
}

/// Catches requests continuing after a failure: with one at a time, a missing first blob ends
/// the fetch before the next blob is asked for.
#[test]
fn the_first_failure_stops_new_requests() {
    let mut mem = Memory::default();
    let missing = digest_of(b"missing");
    let later = mem.put(b"later".to_vec());
    let root = mem.put_directory(&Directory {
        files: vec![file("a", &missing, false), file("b", &later, false)],
        ..Directory::default()
    });
    let scratch_dir = scratch("stop");
    let dir = scratch_dir.join("in");
    let error = materialize(&mem, &root, &dir, 1).expect_err("missing");
    assert!(!dir.join("a").exists());
    assert!(!dir.join("b").exists());
    assert!(matches!(&error, FetchError::Source { .. }), "{error:?}");
    assert!(error.to_string().contains(&label(&missing)), "{error}");
    assert_eq!(mem.reads(&missing), 1);
    assert_eq!(mem.reads(&later), 0);
}

/// Catches a file left behind claiming to be complete when its bytes fail the digest: a blob
/// of the right length with other bytes, and a longer body, leave nothing at any path that
/// names the blob.
#[test]
fn a_blob_that_fails_its_digest_leaves_no_file() {
    for (name, bytes, why) in [
        ("same-length", b"HELLO".to_vec(), "SHA-256"),
        (
            "too-long",
            b"hello, and more".to_vec(),
            "longer than 5 bytes",
        ),
        ("too-short", b"hell".to_vec(), "4 bytes"),
    ] {
        let (mut mem, root) = tree();
        mem.blobs.insert(digest_of(b"hello").hash, bytes);
        let scratch_dir = scratch(name);
        let dir = scratch_dir.join("in");
        let error = materialize(&mem, &root, &dir, 4).expect_err(name);
        assert!(matches!(&error, FetchError::Mismatch { .. }), "{error:?}");
        assert!(error.to_string().contains(why), "{name}: {error}");
        for path in ["a.txt", "copy.txt", "deep/x/y/z.txt"] {
            assert!(!dir.join(path).exists(), "{name}: {path} is there");
        }
    }
}

/// A source that sends ten chunks of 1000 bytes and counts those its writer accepted.
struct Chunks {
    accepted: AtomicUsize,
}

impl BlobSource for Chunks {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        let mut bytes = Vec::new();
        self.fetch_to(digest, &mut bytes)?;
        Ok(bytes)
    }

    fn fetch_to(&self, digest: &Digest, out: &mut dyn Write) -> Result<(), FetchError> {
        for _ in 0..10 {
            out.write_all(&[7; 1000]).map_err(|e| FetchError::Source {
                digest: label(digest),
                why: e.to_string(),
            })?;
            self.accepted.fetch_add(1000, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Catches a body read past the digest's size: of 10,000 bytes sent for a blob of 5,000, the
/// check passes on 5,000 and refuses the next chunk, so nothing past the size is taken.
#[test]
fn a_body_longer_than_the_digest_is_refused_as_it_streams() {
    let source = Chunks {
        accepted: AtomicUsize::new(0),
    };
    let digest = Digest {
        hash: "0".repeat(64),
        size_bytes: 5000,
    };
    let error = source.verified(&digest).expect_err("too long");
    assert!(matches!(&error, FetchError::Mismatch { .. }), "{error:?}");
    assert!(
        error.to_string().contains("longer than 5000 bytes"),
        "{error}"
    );
    assert_eq!(source.accepted.load(Ordering::SeqCst), 5000);
    assert_eq!(source.get(&digest).expect("whole").len(), 10_000);
}

/// Catches a failed write to the destination being reported as the source's fault, or lost:
/// the error names the destination's path.
#[test]
fn a_destination_that_cannot_take_the_bytes_is_an_io_error() {
    let mut room = [0_u8; 2];
    let digest = digest_of(b"abc");
    let error = stream_checked(&digest, Path::new("full"), &mut &mut room[..], &mut |out| {
        out.write_all(b"abc").map_err(|e| FetchError::Source {
            digest: label(&digest),
            why: e.to_string(),
        })
    })
    .expect_err("full");
    assert!(matches!(&error, FetchError::Io { .. }), "{error:?}");
    assert!(error.to_string().starts_with("full: "), "{error}");
}

/// Catches an Action, Command or Directory of any size being read into memory: a digest over
/// the limit is refused as invalid without a request.
#[test]
fn a_message_over_the_limit_is_refused_before_any_request() {
    let mem = Memory::default();
    let digest = Digest {
        hash: "0".repeat(64),
        size_bytes: MAX_MESSAGE_BYTES + 1,
    };
    let error = fetch_message::<Directory>(&mem, &digest).expect_err("big");
    assert!(matches!(&error, FetchError::Invalid(_)), "{error:?}");
    assert!(error.to_string().contains("limit"), "{error}");
    assert_eq!(mem.reads(&digest), 0);
}

/// Catches a second path of a blob that already exists being overwritten when the copy is
/// made: the file there is kept and the fetch fails.
#[test]
fn a_copy_does_not_overwrite_a_file_in_the_way() {
    let (mem, root) = tree();
    let dir = scratch("copy-exists");
    std::fs::write(dir.join("copy.txt"), b"mine").expect("pre-existing");
    let error = materialize(&mem, &root, &dir, 4).expect_err("exists");
    assert!(matches!(error, FetchError::Io { .. }), "{error:?}");
    assert_eq!(std::fs::read(dir.join("copy.txt")).expect("kept"), b"mine");
}

fn big() -> Vec<u8> {
    (0..3_u32 << 20)
        .map(|i| u8::try_from(i.wrapping_mul(31) % 251).expect("a byte"))
        .collect()
}

fn big_tree(mem: &mut Memory, bytes: Vec<u8>) -> Digest {
    let blob = mem.put(bytes);
    mem.put_directory(&Directory {
        files: vec![file("big", &blob, true)],
        ..Directory::default()
    })
}

/// Catches a large blob damaged on its way to disk, or one that fails its hash being kept: a
/// 3 MiB blob arrives byte for byte with its mode, and the same blob with its last byte
/// changed is refused and not left.
#[test]
fn a_large_blob_is_streamed_to_disk_and_its_hash_checked() {
    let mut mem = Memory::default();
    let root = big_tree(&mut mem, big());
    let scratch_dir = scratch("big");
    let dir = scratch_dir.join("in");
    materialize(&mem, &root, &dir, 4).expect("materialize");
    assert!(std::fs::read(dir.join("big")).expect("big") == big());
    assert_eq!(mode(&dir.join("big")), 0o755);

    let mut flipped = big();
    *flipped.last_mut().expect("last") ^= 1;
    let blob = digest_of(&big());
    mem.blobs.insert(blob.hash, flipped);
    let scratch_dir = scratch("big-flipped");
    let dir = scratch_dir.join("in");
    let error = materialize(&mem, &root, &dir, 4).expect_err("flipped");
    assert!(matches!(&error, FetchError::Mismatch { .. }), "{error:?}");
    assert!(!dir.join("big").exists());
}

/// Catches the streaming path failing over real HTTP: a Directory and a 3 MiB file are served
/// by the grant and the tree is laid out from them.
#[test]
fn a_tree_is_fetched_over_http() {
    let mut mem = Memory::default();
    let root = big_tree(&mut mem, big());
    let directory = mem.blobs[&root.hash].clone();
    let (base, seen) = serve(vec![("200 OK", directory), ("200 OK", big())]);
    let scratch_dir = scratch("http-tree");
    let dir = scratch_dir.join("in");
    materialize(&plain(&base), &root, &dir, 1).expect("materialize");
    assert!(std::fs::read(dir.join("big")).expect("big") == big());
    let seen = seen.lock().expect("seen");
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].0,
        format!("GET /{}/{} HTTP/1.1", root.hash, root.size_bytes)
    );
}

#[path = "fetch_failure_tests.rs"]
mod failure;

#[path = "fetch_keep_alive_tests.rs"]
mod keep_alive;

#[path = "locate_tests.rs"]
mod locate;
