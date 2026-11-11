use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;

use kbf_testkit::dir::TestDir;

use super::*;

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-debs-unit"), name)
}

const A: &[u8] = b"package a";
const B: &[u8] = b"package b";

/// A pins file with two packages for each architecture, served by two mirrors.
fn pins_text() -> String {
    let table = |arch: &str| {
        format!(
            "[{arch}]\nmirrors = [\"https://one.example/ubuntu\", \"https://two.example/ubuntu\"]\n\
             debs = [\n\
             {{ package = \"a\", version = \"1.0-1\", path = \"pool/main/a/a/a_1.0-1_{arch}.deb\", sha256 = \"{}\" }},\n\
             {{ package = \"b\", version = \"2:3.1\", path = \"pool/main/b/b_3.1_all.deb\", sha256 = \"{}\" }},\n\
             ]\n",
            Sha256::of(A),
            Sha256::of(B)
        )
    };
    format!("# pins\n{}\n{}", table("amd64"), table("arm64"))
}

/// `pins_text()` with `from` replaced by `to` (which must occur in it).
fn pins_with(from: &str, to: &str) -> String {
    let text = pins_text();
    assert!(text.contains(from), "{from}");
    text.replace(from, to)
}

/// The amd64 file name of `a`.
const A_NAME: &str = "a_1.0-1_amd64.deb";
/// The file name of `b`.
const B_NAME: &str = "b_3.1_all.deb";

/// A fake mirror: answers each URL from `answers` (the first whose URL is that), and records
/// every URL asked for and every pause.
struct Mirror {
    answers: Vec<(String, Result<Vec<u8>, String>)>,
    asked: RefCell<Vec<String>>,
    paused: RefCell<Vec<Duration>>,
}

impl Mirror {
    fn new(answers: &[(&str, Result<&[u8], &str>)]) -> Mirror {
        Mirror {
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
            .map_or(Err(format!("404 for {url}")), |(_, a)| a.clone())
    }

    /// Runs `debs` for amd64 with the pins file `pins` (written to `dir/pins.toml`) into
    /// `dir/debs`.
    fn run(&self, dir: &Path, pins: &str) -> Result<Vec<String>, Error> {
        let file = dir.join("pins.toml");
        std::fs::write(&file, pins).expect("pins");
        fill(
            &file,
            Arch::Amd64,
            &dir.join("debs"),
            &|url| self.get(url),
            &|wait| self.paused.borrow_mut().push(wait),
        )
    }
}

const ONE_A: &str = "https://one.example/ubuntu/pool/main/a/a/a_1.0-1_amd64.deb";
const TWO_A: &str = "https://two.example/ubuntu/pool/main/a/a/a_1.0-1_amd64.deb";
const ONE_B: &str = "https://one.example/ubuntu/pool/main/b/b_3.1_all.deb";
const TWO_B: &str = "https://two.example/ubuntu/pool/main/b/b_3.1_all.deb";

/// The files in `dir`, sorted, with their contents.
fn files(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut found: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .expect("read")
        .map(|e| {
            let e = e.expect("entry");
            let name = e.file_name().to_string_lossy().into_owned();
            (name, std::fs::read(e.path()).expect("file"))
        })
        .collect();
    found.sort();
    found
}

/// Asserts that Rust's two Linux architectures map to their Debian names and any other is
/// refused by name, and that this host's is one of them.
#[test]
fn an_architecture_is_amd64_or_arm64() {
    assert_eq!(Arch::of("x86_64"), Ok(Arch::Amd64));
    assert_eq!(Arch::of("aarch64"), Ok(Arch::Arm64));
    assert_eq!(Arch::of("riscv64"), Err(Error::Arch("riscv64".to_owned())));
    assert_eq!(
        Error::Arch("riscv64".to_owned()).to_string(),
        "this host's architecture \"riscv64\" is neither x86_64 nor aarch64"
    );
    assert_eq!(Arch::Amd64.name(), "amd64");
    assert_eq!(Arch::Arm64.name(), "arm64");
    assert!(Arch::host().is_ok());
}

/// Asserts that each architecture's table is read in order, with an epoch left out of the
/// file name and an `all` package accepted.
#[test]
fn a_pins_file_gives_each_architecture_its_table() {
    let text = pins_text();
    for (arch, a) in [(Arch::Amd64, A_NAME), (Arch::Arm64, "a_1.0-1_arm64.deb")] {
        let pins = parse(&text, arch).expect("pins");
        assert_eq!(
            pins.mirrors,
            ["https://one.example/ubuntu", "https://two.example/ubuntu"]
        );
        let names: Vec<&str> = pins.debs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, [a, B_NAME]);
        assert_eq!(pins.debs[0].sha256, Sha256::of(A));
        assert_eq!(pins.debs[1].path, "pool/main/b/b_3.1_all.deb");
    }
}

/// Asserts that a pins file breaking a rule is refused with the reason: not TOML, an unknown
/// architecture or key, no table for the host, no mirror, a mirror that is not plain HTTPS,
/// no package, a digest that is not one, a path outside `pool/`, climbing out of it or with an
/// empty part, a file name its package, version and architecture do not make, and one file
/// pinned twice.
#[test]
fn a_pins_file_that_breaks_a_rule_is_refused() {
    let first =
        "{ package = \"a\", version = \"1.0-1\", path = \"pool/main/a/a/a_1.0-1_amd64.deb\"";
    let cases: Vec<(String, &str)> = vec![
        ("[amd64\n".to_owned(), "unclosed table"),
        (pins_with("[arm64]", "[i386]"), "unknown field `i386`"),
        (
            pins_with("mirrors = [", "colour = 1\nmirrors = ["),
            "unknown field `colour`",
        ),
        (
            pins_with(", sha256 = ", ", size = 1, sha256 = "),
            "unknown field `size`",
        ),
        (
            pins_text()
                .split_once("[arm64]")
                .map(|(_, arm64)| format!("[arm64]{arm64}"))
                .unwrap_or_default(),
            "no [amd64] table",
        ),
        (
            pins_with(
                "[\"https://one.example/ubuntu\", \"https://two.example/ubuntu\"]",
                "[]",
            ),
            "[amd64] names no mirror",
        ),
        (
            pins_with(
                "https://one.example/ubuntu\"",
                "http://one.example/ubuntu\"",
            ),
            "the mirror \"http://one.example/ubuntu\" is not an https:// URL",
        ),
        (
            pins_with("https://one.example/ubuntu\"", "https://\""),
            "the mirror \"https://\" is not",
        ),
        (
            pins_with(
                "https://one.example/ubuntu\"",
                "https://one.example/ubuntu/\"",
            ),
            "the mirror \"https://one.example/ubuntu/\" is not",
        ),
        (
            pins_with(
                "https://one.example/ubuntu\"",
                "https://one.example/ubuntu?x\"",
            ),
            "the mirror \"https://one.example/ubuntu?x\" is not",
        ),
        (
            pins_with(
                "https://one.example/ubuntu\"",
                "https://user@one.example/ubuntu\"",
            ),
            "the mirror \"https://user@one.example/ubuntu\" is not",
        ),
        (
            pins_text()
                .split_once("debs = [")
                .map(|(head, _)| format!("{head}debs = []\n"))
                .unwrap_or_default(),
            "[amd64] pins no package",
        ),
        (
            pins_with(
                &format!("sha256 = \"{}\"", Sha256::of(A)),
                "sha256 = \"00\"",
            ),
            "not a SHA-256: \"00\"",
        ),
        (
            pins_with(
                "path = \"pool/main/a/a/a_1.0-1_amd64.deb\"",
                "path = \"dists/a_1.0-1_amd64.deb\"",
            ),
            "the path \"dists/a_1.0-1_amd64.deb\" of a 1.0-1 is not pool/.../a_1.0-1_amd64.deb",
        ),
        (
            pins_with(
                "pool/main/a/a/a_1.0-1_amd64.deb",
                "pool/../a_1.0-1_amd64.deb",
            ),
            "the path \"pool/../a_1.0-1_amd64.deb\"",
        ),
        (
            pins_with("pool/main/a/a/a_1.0-1_amd64.deb", "pool//a_1.0-1_amd64.deb"),
            "the path \"pool//a_1.0-1_amd64.deb\"",
        ),
        (
            pins_with(
                "pool/main/a/a/a_1.0-1_amd64.deb",
                "pool/main/a/a/a_1.0-2_amd64.deb",
            ),
            "the path \"pool/main/a/a/a_1.0-2_amd64.deb\" of a 1.0-1",
        ),
        (
            pins_with(
                "pool/main/a/a/a_1.0-1_amd64.deb",
                "pool/main/a/a/a_1.0-1_arm64.deb",
            ),
            "is not pool/.../a_1.0-1_amd64.deb (or the `all` package)",
        ),
        (
            pins_with(
                first,
                &format!("{first}, sha256 = \"{}\" }},\n{first}", Sha256::of(A)),
            ),
            "a_1.0-1_amd64.deb is pinned twice",
        ),
    ];
    for (text, says) in cases {
        let refused = parse(&text, Arch::Amd64).expect_err(says);
        assert!(refused.contains(says), "{says}: {refused}");
    }
}

/// Asserts that with every pinned file already in the directory (the cache restored it) and
/// matching, nothing is downloaded or paused for and each file is reported as cached.
#[test]
fn cached_files_that_match_are_kept_without_a_download() {
    let dir = scratch("cached");
    let debs = dir.path().join("debs");
    std::fs::create_dir_all(&debs).expect("debs");
    std::fs::write(debs.join(A_NAME), A).expect("a");
    std::fs::write(debs.join(B_NAME), B).expect("b");
    let mirror = Mirror::new(&[]);
    let lines = mirror.run(dir.path(), &pins_text()).expect("ok");
    assert_eq!(
        lines,
        [
            format!("ok: the cached {A_NAME} has SHA-256 {}", Sha256::of(A)),
            format!("ok: the cached {B_NAME} has SHA-256 {}", Sha256::of(B)),
        ]
    );
    assert!(mirror.asked.borrow().is_empty());
    assert!(mirror.paused.borrow().is_empty());
}

/// Asserts that a missing directory is made and filled from the first mirror, one download
/// per file, written whole with no `.part` file left and mode 0644, and that a second run
/// finds both files and downloads nothing.
#[test]
fn a_cache_miss_downloads_each_file_from_the_first_mirror() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("miss");
    let mirror = Mirror::new(&[(ONE_A, Ok(A)), (ONE_B, Ok(B))]);
    let lines = mirror.run(dir.path(), &pins_text()).expect("ok");
    assert_eq!(
        lines,
        [
            format!("ok: {ONE_A} has SHA-256 {}", Sha256::of(A)),
            format!("ok: {ONE_B} has SHA-256 {}", Sha256::of(B)),
        ]
    );
    assert_eq!(*mirror.asked.borrow(), [ONE_A, ONE_B]);
    assert_eq!(*mirror.paused.borrow(), [Duration::ZERO, Duration::ZERO]);
    let debs = dir.path().join("debs");
    assert_eq!(
        files(&debs),
        [
            (A_NAME.to_owned(), A.to_vec()),
            (B_NAME.to_owned(), B.to_vec())
        ]
    );
    let mode = std::fs::metadata(debs.join(A_NAME))
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644);
    let again = Mirror::new(&[]);
    again.run(dir.path(), &pins_text()).expect("ok");
    assert!(again.asked.borrow().is_empty());
}

/// Asserts that a cache that restored only some files gets the rest downloaded, and the
/// restored one is not asked for.
#[test]
fn a_partial_cache_downloads_only_the_missing_file() {
    let dir = scratch("partial");
    let debs = dir.path().join("debs");
    std::fs::create_dir_all(&debs).expect("debs");
    std::fs::write(debs.join(A_NAME), A).expect("a");
    let mirror = Mirror::new(&[(ONE_B, Ok(B))]);
    let lines = mirror.run(dir.path(), &pins_text()).expect("ok");
    assert_eq!(
        lines,
        [
            format!("ok: the cached {A_NAME} has SHA-256 {}", Sha256::of(A)),
            format!("ok: {ONE_B} has SHA-256 {}", Sha256::of(B)),
        ]
    );
    assert_eq!(*mirror.asked.borrow(), [ONE_B]);
}

/// Asserts that when the first mirror fails (a DNS failure, its line breaks folded) or gives
/// other bytes, the next one is tried in the same round, and the line names each failed
/// attempt.
#[test]
fn a_failed_mirror_falls_back_to_the_next() {
    let dir = scratch("fallback");
    let mirror = Mirror::new(&[
        (
            ONE_A,
            Err("dns error: failed to lookup address\r\ninformation"),
        ),
        (TWO_A, Ok(A)),
        (ONE_B, Ok(b"not b")),
        (TWO_B, Ok(B)),
    ]);
    let lines = mirror.run(dir.path(), &pins_text()).expect("ok");
    assert_eq!(
        lines,
        [
            format!(
                "ok: {TWO_A} has SHA-256 {}, after 1 failed: {ONE_A}: dns error: failed to \
                 lookup address  information",
                Sha256::of(A)
            ),
            format!(
                "ok: {TWO_B} has SHA-256 {}, after 1 failed: {ONE_B}: SHA-256 {}, not the \
                 pinned one",
                Sha256::of(B),
                Sha256::of(b"not b")
            ),
        ]
    );
    assert_eq!(
        files(&dir.path().join("debs")),
        [
            (A_NAME.to_owned(), A.to_vec()),
            (B_NAME.to_owned(), B.to_vec())
        ]
    );
}

/// Asserts that a file no mirror gives in any round is refused with every attempt in order,
/// after exactly the pauses of `WAITS` (three rounds, 0 s, 10 s and 30 s), with nothing
/// written for it.
#[test]
fn every_round_failing_is_refused_with_each_attempt() {
    let dir = scratch("unavailable");
    let mirror = Mirror::new(&[(ONE_B, Ok(B))]);
    let refused = mirror.run(dir.path(), &pins_text()).expect_err("refused");
    let mut attempts = Vec::new();
    for _ in WAITS {
        attempts.push(format!("{ONE_A}: 404 for {ONE_A}"));
        attempts.push(format!("{TWO_A}: 404 for {TWO_A}"));
    }
    assert_eq!(
        refused,
        Error::Unavailable {
            name: A_NAME.to_owned(),
            attempts: attempts.clone(),
        }
    );
    assert_eq!(
        refused.to_string(),
        format!(
            "{A_NAME}: no download was the pinned file: {}; nothing was installed",
            attempts.join("; ")
        )
    );
    assert_eq!(*mirror.paused.borrow(), WAITS);
    assert_eq!(files(&dir.path().join("debs")), []);
    assert_eq!(
        WAITS,
        [
            Duration::ZERO,
            Duration::from_secs(10),
            Duration::from_secs(30)
        ]
    );
    assert_eq!(ATTEMPT_TIMEOUT, Duration::from_secs(120));
}

/// Asserts that every mirror failing in the first round is tried again after the next pause,
/// and a second round's success is kept.
#[test]
fn a_later_round_may_succeed() {
    let dir = scratch("later");
    let calls = RefCell::new(0);
    let file = dir.path().join("pins.toml");
    std::fs::write(&file, pins_text()).expect("pins");
    let paused = RefCell::new(Vec::new());
    let lines = fill(
        &file,
        Arch::Amd64,
        &dir.path().join("debs"),
        &|url| {
            *calls.borrow_mut() += 1;
            match (*calls.borrow(), url) {
                (3.., ONE_A) => Ok(A.to_vec()),
                (_, ONE_B) => Ok(B.to_vec()),
                _ => Err("connection reset".to_owned()),
            }
        },
        &|wait| paused.borrow_mut().push(wait),
    )
    .expect("ok");
    assert_eq!(
        lines[0],
        format!(
            "ok: {ONE_A} has SHA-256 {}, after 2 failed: {ONE_A}: connection reset; {TWO_A}: \
             connection reset",
            Sha256::of(A)
        )
    );
    assert_eq!(
        *paused.borrow(),
        [Duration::ZERO, Duration::from_secs(10), Duration::ZERO]
    );
}

/// Asserts that a cached file whose SHA-256 is another is refused and left as it is, before any
/// download, and that a file no pin names (a stale `.part` too) is refused before anything is
/// read or downloaded.
#[test]
fn a_wrong_cached_file_and_a_stray_file_are_refused() {
    let dir = scratch("wrong");
    let debs = dir.path().join("debs");
    std::fs::create_dir_all(&debs).expect("debs");
    std::fs::write(debs.join(A_NAME), b"tampered").expect("a");
    let mirror = Mirror::new(&[(ONE_A, Ok(A)), (ONE_B, Ok(B))]);
    let refused = mirror.run(dir.path(), &pins_text());
    assert_eq!(
        refused,
        Err(Error::CachedMismatch {
            path: debs.join(A_NAME),
            expected: Sha256::of(A),
            actual: Sha256::of(b"tampered"),
        })
    );
    assert_eq!(
        refused.expect_err("mismatch").to_string(),
        format!(
            "the cached {:?} has SHA-256 {}, not the pinned {}; nothing was installed",
            debs.join(A_NAME),
            Sha256::of(b"tampered"),
            Sha256::of(A)
        )
    );
    assert_eq!(std::fs::read(debs.join(A_NAME)).expect("a"), b"tampered");
    assert!(mirror.asked.borrow().is_empty());

    std::fs::write(debs.join(A_NAME), A).expect("a");
    for stray in ["zz.deb", "b_3.1_all.deb.part"] {
        std::fs::write(debs.join(stray), b"x").expect("stray");
        let refused = mirror.run(dir.path(), &pins_text()).expect_err("stray");
        assert_eq!(
            refused.to_string(),
            format!("{debs:?} holds {stray:?}, which no pin names; nothing was installed")
        );
        std::fs::remove_file(debs.join(stray)).expect("rm");
    }
    assert!(mirror.asked.borrow().is_empty());
}

/// Asserts the failures of the file system, each naming its path: no pins file, a directory
/// that is a regular file, a pinned name that is a directory (reading it fails), and a
/// download that cannot be written.
#[test]
fn file_system_failures_name_their_path() {
    let dir = scratch("fs");
    let mirror = Mirror::new(&[(ONE_A, Ok(A)), (ONE_B, Ok(B))]);
    let missing = dir.path().join("nothing.toml");
    let refused = fill(
        &missing,
        Arch::Amd64,
        &dir.path().join("debs"),
        &|url| mirror.get(url),
        &|_| {},
    )
    .expect_err("no pins");
    assert!(
        matches!(&refused, Error::Pins { path, why } if *path == missing && why.contains("No such file")),
        "{refused:?}"
    );
    assert!(
        refused
            .to_string()
            .starts_with(&format!("the pins file {missing:?}: ")),
        "{refused}"
    );

    let not_toml = dir.path().join("bad.toml");
    std::fs::write(&not_toml, "[amd64\n").expect("bad");
    let refused = fill(
        &not_toml,
        Arch::Amd64,
        dir.path(),
        &|_| Ok(Vec::new()),
        &|_| {},
    )
    .expect_err("bad pins");
    assert!(
        matches!(&refused, Error::Pins { path, .. } if *path == not_toml),
        "{refused:?}"
    );

    let file = dir.path().join("debs");
    std::fs::write(&file, b"").expect("a file");
    let refused = mirror.run(dir.path(), &pins_text()).expect_err("not a dir");
    assert!(
        matches!(&refused, Error::Dir { path, .. } if *path == file),
        "{refused:?}"
    );
    assert!(
        refused.to_string().ends_with("; nothing was installed"),
        "{refused}"
    );
    std::fs::remove_file(&file).expect("rm");

    // `a` is a directory: reading it fails, and it is not replaced.
    std::fs::create_dir_all(file.join(A_NAME)).expect("dir a");
    let refused = mirror.run(dir.path(), &pins_text()).expect_err("a dir");
    assert!(
        matches!(&refused, Error::Dir { path, .. } if *path == file.join(A_NAME)),
        "{refused:?}"
    );
    std::fs::remove_dir(file.join(A_NAME)).expect("rm");

    // The download's part file cannot be written (its name became a directory while the file
    // was downloading): refused naming the package's file, and nothing is installed under it.
    let part = file.join(format!("{A_NAME}.part"));
    let pins = dir.path().join("pins.toml");
    let refused = fill(
        &pins,
        Arch::Amd64,
        &file,
        &|_| {
            std::fs::create_dir_all(&part).expect("part dir");
            Ok(A.to_vec())
        },
        &|_| {},
    )
    .expect_err("unwritable");
    assert!(
        matches!(&refused, Error::Dir { path, .. } if *path == file.join(A_NAME)),
        "{refused:?}"
    );
    assert!(!file.join(A_NAME).exists());
}

/// Asserts that the HTTPS download refuses a plain `http://` URL, with no request made, and
/// says so in one line.
#[test]
fn the_download_is_https_only() {
    let get = https(&[]);
    let refused = get("http://127.0.0.1/pool/a.deb").expect_err("http");
    assert!(
        refused.starts_with("download http://127.0.0.1/pool/a.deb: "),
        "{refused}"
    );
    assert!(!refused.contains('\n'), "{refused}");
}

/// Asserts that the verb's own entry refuses a missing pins file by its path, after taking
/// this host's architecture.
#[test]
fn the_verb_reads_the_pins_file_it_is_given() {
    let dir = scratch("verb");
    let pins = dir.path().join("none.toml");
    let refused = run(&Args {
        pins: pins.clone(),
        dir: dir.path().join("debs"),
    })
    .expect_err("no pins");
    assert!(
        matches!(&refused, Error::Pins { path, .. } if *path == pins),
        "{refused:?}"
    );
}

/// Asserts that the repository's pins of musl-tools read for both architectures: the three
/// packages of musl 1.2.4-2, from Canonical's archive first and then two mirrors run by
/// others, so that a Canonical outage leaves a way to them.
#[test]
fn the_musl_tools_pins_read_for_both_architectures() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/debs/musl-tools.toml"
    ))
    .expect("the pins");
    for (arch, mirrors) in [
        (
            Arch::Amd64,
            [
                "https://archive.ubuntu.com/ubuntu",
                "https://mirrors.edge.kernel.org/ubuntu",
                "https://mirrors.mit.edu/ubuntu",
            ],
        ),
        (
            Arch::Arm64,
            [
                "https://ports.ubuntu.com/ubuntu-ports",
                "https://mirrors.mit.edu/ubuntu-ports",
                "https://mirrors.ocf.berkeley.edu/ubuntu-ports",
            ],
        ),
    ] {
        let pins = parse(&text, arch).expect("pins");
        assert_eq!(pins.mirrors, mirrors);
        let names: Vec<String> = pins.debs.iter().map(|d| d.name.clone()).collect();
        let a = arch.name();
        assert_eq!(
            names,
            [
                format!("musl_1.2.4-2_{a}.deb"),
                format!("musl-dev_1.2.4-2_{a}.deb"),
                format!("musl-tools_1.2.4-2_{a}.deb"),
            ]
        );
    }
}
