//! [`fetch_cached`] and [`first_pinned`] against a fake transport: what a cache hit, a cache miss
//! and every way a miss can fail install, write, download and wait for.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use kbf_testkit::dir::TestDir;

const TOOL: &[u8] = b"a pinned toolchain's tarball";

const ONE: &str = "https://one.example/zig/zig.tar.xz";
const TWO: &str = "https://two.example/zig/zig.tar.xz";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-fetch-cache-unit"), name)
}

/// A fake transport: answers each URL from `answers` (the first whose URL is that), and records
/// every URL asked for and every pause.
struct Transport {
    answers: Vec<(String, Result<Vec<u8>, String>)>,
    asked: RefCell<Vec<String>>,
    paused: RefCell<Vec<Duration>>,
}

impl Transport {
    fn new(answers: &[(&str, Result<&[u8], &str>)]) -> Transport {
        Transport {
            answers: answers
                .iter()
                .map(|(url, answer)| {
                    let answer = answer.map(<[u8]>::to_vec).map_err(str::to_owned);
                    ((*url).to_owned(), answer)
                })
                .collect(),
            asked: RefCell::new(Vec::new()),
            paused: RefCell::new(Vec::new()),
        }
    }

    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.asked.borrow_mut().push(url.to_owned());
        self.answers
            .iter()
            .find(|(u, _)| u == url)
            .map_or(Err(format!("connection refused by {url}")), |(_, a)| {
                a.clone()
            })
    }

    /// [`fetch_cached`] of `ONE` then `TWO`, pinned to `TOOL`, as is, into `out`, through
    /// `cache`.
    fn fetch(&self, out: &Path, cache: &Path, unpack: &Unpack) -> Result<String, Error> {
        let urls = [ONE.to_owned(), TWO.to_owned()];
        let req = Cached {
            urls: &urls,
            sha256: &Sha256::of(TOOL),
            unpack,
            out,
            cache,
        };
        fetch_cached(&req, &|url| self.get(url), &|d| {
            self.paused.borrow_mut().push(d)
        })
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }

    fn paused(&self) -> Vec<Duration> {
        self.paused.borrow().clone()
    }
}

/// `out` and `cache` paths in `dir`.
fn paths(dir: &TestDir) -> (PathBuf, PathBuf) {
    (
        dir.join("bin").join("zig.tar.xz"),
        dir.join("cache").join("zig.tar.xz"),
    )
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// Asserts that a cache file holding the pinned bytes is installed with no download and no
/// pause, says the cache hit, and is left as it was.
#[test]
fn a_cache_hit_installs_the_cached_download_without_the_network() {
    let dir = scratch("hit");
    let (out, cache) = paths(&dir);
    std::fs::create_dir_all(cache.parent().expect("parent")).expect("mkdir");
    std::fs::write(&cache, TOOL).expect("seed");
    let net = Transport::new(&[(ONE, Ok(b"never asked"))]);
    let line = net.fetch(&out, &cache, &Unpack::Raw).expect("hit");
    assert_eq!(
        line,
        format!(
            "ok: the cache hit: {} has SHA-256 {}; nothing was downloaded; installed {}",
            cache.display(),
            Sha256::of(TOOL),
            out.display()
        )
    );
    assert_eq!(net.asked(), Vec::<String>::new());
    assert_eq!(net.paused(), Vec::<Duration>::new());
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
    assert_eq!(mode(&out), 0o755);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
}

/// Asserts that on a miss the first URL's pinned bytes are kept in the cache file (0644) and
/// installed, after one round with no pause, and that the line says the cache missed.
#[test]
fn a_cache_miss_downloads_once_keeps_the_download_and_installs_it() {
    let dir = scratch("miss");
    let (out, cache) = paths(&dir);
    let net = Transport::new(&[(ONE, Ok(TOOL)), (TWO, Ok(b"never asked"))]);
    let line = net.fetch(&out, &cache, &Unpack::Raw).expect("miss");
    assert_eq!(
        line,
        format!(
            "ok: the cache missed: {ONE} has SHA-256 {}; kept in {}; installed {}",
            Sha256::of(TOOL),
            cache.display(),
            out.display()
        )
    );
    assert_eq!(net.asked(), [ONE]);
    assert_eq!(net.paused(), [Duration::ZERO]);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
    assert_eq!(mode(&cache), 0o644);
    assert!(!part_path(&cache).exists());
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
    // The next run, with the cache restored, makes no download.
    let again = Transport::new(&[]);
    let line = again.fetch(&out, &cache, &Unpack::Raw).expect("hit");
    assert!(line.starts_with("ok: the cache hit: "), "{line}");
    assert_eq!(again.asked(), Vec::<String>::new());
}

/// Asserts that a failing URL and one serving other bytes are passed over, round after round
/// with the pauses of `WAITS`, until a URL gives the pinned bytes; the line names each failed
/// attempt in order.
#[test]
fn a_miss_tries_each_url_in_turn_and_waits_between_rounds() {
    let dir = scratch("retry");
    let (out, cache) = paths(&dir);
    let wrong: &[u8] = b"a truncated tarball";
    // ONE refuses every time; TWO serves other bytes in the first two rounds and then the pin.
    let rounds = RefCell::new(0);
    let urls = [ONE.to_owned(), TWO.to_owned()];
    let asked = RefCell::new(Vec::new());
    let paused = RefCell::new(Vec::new());
    let get = |url: &str| {
        asked.borrow_mut().push(url.to_owned());
        if url == ONE {
            return Err("timeout: global\nafter 300 s".to_owned());
        }
        *rounds.borrow_mut() += 1;
        Ok(if *rounds.borrow() < 3 { wrong } else { TOOL }.to_vec())
    };
    let req = Cached {
        urls: &urls,
        sha256: &Sha256::of(TOOL),
        unpack: &Unpack::Raw,
        out: &out,
        cache: &cache,
    };
    let line = fetch_cached(&req, &get, &|d| paused.borrow_mut().push(d)).expect("third round");
    let refused = format!("{ONE}: timeout: global after 300 s");
    let other = format!("{TWO}: SHA-256 {}, not the pinned one", Sha256::of(wrong));
    let failed = [&refused, &other, &refused, &other, &refused].map(|s| s.as_str());
    assert_eq!(
        line,
        format!(
            "ok: the cache missed: {TWO} has SHA-256 {}, after 5 failed: {}; kept in {}; installed {}",
            Sha256::of(TOOL),
            failed.join("; "),
            cache.display(),
            out.display()
        )
    );
    assert_eq!(*asked.borrow(), [ONE, TWO, ONE, TWO, ONE, TWO]);
    assert_eq!(*paused.borrow(), WAITS);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
}

/// Asserts that a miss no URL serves in any round is refused with every attempt in order, after
/// exactly the pauses of `WAITS` (0 s, 10 s, 30 s), with neither the cache file nor `out`
/// written.
#[test]
fn a_miss_no_url_serves_is_refused_with_each_attempt_and_writes_nothing() {
    let dir = scratch("unavailable");
    let (out, cache) = paths(&dir);
    let net = Transport::new(&[(TWO, Ok(b"not the pin"))]);
    let refused = net.fetch(&out, &cache, &Unpack::Raw).expect_err("refused");
    let mut attempts = Vec::new();
    for _ in WAITS {
        attempts.push(format!("{ONE}: connection refused by {ONE}"));
        attempts.push(format!(
            "{TWO}: SHA-256 {}, not the pinned one",
            Sha256::of(b"not the pin")
        ));
    }
    assert_eq!(
        refused.to_string(),
        format!(
            "the cache {cache:?} missed and no download was the pinned file: {}; nothing was \
             installed",
            attempts.join("; ")
        )
    );
    assert_eq!(
        refused,
        Error::Unavailable {
            cache: cache.clone(),
            attempts
        }
    );
    assert_eq!(net.paused(), WAITS);
    assert_eq!(
        WAITS,
        [
            Duration::ZERO,
            Duration::from_secs(10),
            Duration::from_secs(30)
        ]
    );
    assert_eq!(ATTEMPT_TIMEOUT, Duration::from_secs(300));
    assert!(!cache.exists() && !part_path(&cache).exists());
    assert!(!out.exists());
}

/// Asserts that a cache file of other bytes is refused, named with both digests, with no
/// download, nothing installed and the file left for a person to look at.
#[test]
fn a_cached_file_of_other_bytes_is_refused_without_a_download() {
    let dir = scratch("cached-mismatch");
    let (out, cache) = paths(&dir);
    std::fs::create_dir_all(cache.parent().expect("parent")).expect("mkdir");
    let other: &[u8] = b"a tarball of another version";
    std::fs::write(&cache, other).expect("seed");
    let net = Transport::new(&[(ONE, Ok(TOOL))]);
    let refused = net.fetch(&out, &cache, &Unpack::Raw).expect_err("refused");
    assert_eq!(
        refused.to_string(),
        format!(
            "the cached {cache:?} has SHA-256 {}, not the pinned {}; nothing was installed",
            Sha256::of(other),
            Sha256::of(TOOL)
        )
    );
    assert_eq!(net.asked(), Vec::<String>::new());
    assert!(!out.exists());
    assert_eq!(std::fs::read(&cache).expect("cache"), other);
}

/// Asserts that a cache path that cannot be read (a directory) is refused before any download.
#[test]
fn a_cache_that_cannot_be_read_is_refused_before_a_download() {
    let dir = scratch("cache-dir");
    let (out, cache) = paths(&dir);
    std::fs::create_dir_all(&cache).expect("a directory where the file goes");
    let net = Transport::new(&[(ONE, Ok(TOOL))]);
    let refused = net.fetch(&out, &cache, &Unpack::Raw).expect_err("refused");
    let Error::Cache { path, why } = &refused else {
        panic!("{refused:?}");
    };
    assert_eq!(path, &cache);
    assert_eq!(
        refused.to_string(),
        format!("the cache {cache:?}: {why}; nothing was installed")
    );
    assert_eq!(net.asked(), Vec::<String>::new());
    assert!(!out.exists());
}

/// Asserts that a pinned download that cannot be kept (a directory stands where its part file
/// goes) is an error naming the cache, after the one download, with nothing installed.
#[test]
fn a_download_that_cannot_be_kept_installs_nothing() {
    let dir = scratch("cache-unwritable");
    let (out, cache) = paths(&dir);
    std::fs::create_dir_all(part_path(&cache)).expect("a directory where the part file goes");
    let net = Transport::new(&[(ONE, Ok(TOOL))]);
    let refused = net.fetch(&out, &cache, &Unpack::Raw).expect_err("refused");
    assert!(
        matches!(&refused, Error::Cache { path, .. } if *path == cache),
        "{refused:?}"
    );
    assert_eq!(net.asked(), [ONE]);
    assert!(!cache.exists());
    assert!(!out.exists());
}

/// Asserts that the cached download is unpacked as `--extract` says, on a hit as on a miss, and
/// that a member the archive lacks is refused after the verified download was kept (so the next
/// run makes no download either).
#[test]
fn the_cached_download_is_unpacked_like_any_other() {
    let dir = scratch("unpack");
    let (out, cache) = paths(&dir);
    let mut tar = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_mode(0o755);
    h.set_size(4);
    tar.append_data(&mut h, "zig-1/zig", &b"\x7fELF"[..])
        .expect("file");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, &tar.into_inner().expect("tar")).expect("gzip");
    let archive = gz.finish().expect("gzip");
    let urls = [ONE.to_owned()];
    let get = |_: &str| Ok(archive.clone());
    let fetch = |member: &str| {
        let unpack = Unpack::Tar {
            xz: false,
            member: PathBuf::from(member),
        };
        let req = Cached {
            urls: &urls,
            sha256: &Sha256::of(&archive),
            unpack: &unpack,
            out: &out,
            cache: &cache,
        };
        fetch_cached(&req, &get, &|_| {})
    };
    assert_eq!(
        fetch("zig-1/missing"),
        Err(Error::NoMember(PathBuf::from("zig-1/missing")))
    );
    assert_eq!(std::fs::read(&cache).expect("kept"), archive);
    assert!(!out.exists());
    let line = fetch("zig-1/zig").expect("hit");
    assert!(line.starts_with("ok: the cache hit: "), "{line}");
    assert_eq!(std::fs::read(&out).expect("out"), b"\x7fELF");
}

/// Asserts that the verb's flags take `--url` more than once only with `--cache`.
#[test]
fn several_urls_need_a_cache() {
    let args = |urls: &[&str], cache: Option<&str>| Args {
        url: urls.iter().map(|u| (*u).to_owned()).collect(),
        sha256: Sha256::of(TOOL),
        out: PathBuf::from("out"),
        extract: None,
        member: None,
        cache: cache.map(PathBuf::from),
    };
    assert_eq!(args(&[ONE], None).unpack(), Ok(Unpack::Raw));
    assert_eq!(args(&[ONE, TWO], Some("c")).unpack(), Ok(Unpack::Raw));
    assert_eq!(
        args(&[ONE, TWO], None).unpack(),
        Err(
            "--url is given once without --cache: only a cached fetch tries several URLs"
                .to_owned()
        )
    );
}
