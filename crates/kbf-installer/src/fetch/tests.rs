//! The parent's checks that need no byte and no child: a fetch that must be refused is refused
//! before a file is touched or a process exists. (The child against real servers is in
//! `tests/fetch_e2e.rs`; the pipe is in `pipe_tests.rs`.)

use super::*;
use crate::fixtures::scratch_dir;

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn limits() -> Limits {
    Limits {
        max_component_bytes: 1 << 20,
        fetch_timeout: Duration::from_secs(60),
    }
}

fn url() -> Source {
    Source::Url {
        base: "https://mirror.example/kbf".to_owned(),
        ca_file: None,
    }
}

/// A program that cannot start: if a check lets the fetch through, the error is `Spawn`, not
/// the refusal the test names.
fn nowhere() -> ChildProgram {
    ChildProgram {
        program: PathBuf::from("/nonexistent/kbf-installer"),
        run_as: None,
    }
}

fn pull(source: &Source, sha: &str, size: u64, replaced: u64) -> Result<Received, FetchError> {
    // One directory per call: the tests run side by side and a scratch directory is wiped when made.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = scratch_dir(&format!("fetch-checks-{n}"));
    let mut out = File::create(dir.join("download")).expect("file");
    fetch_component(
        &nowhere(),
        source,
        limits(),
        &Component { sha256: sha, size },
        (&dir, replaced),
        &mut out,
    )
}

/// The digest is the name the blob is asked for by: anything but 64 lowercase hex digits is
/// refused before a request is made (an upper-case digest would name a second copy; a longer
/// one is a path).
///
/// Mutant seen red: `is_digest` accepting any 64 characters.
#[test]
fn a_digest_that_is_not_lowercase_hex_is_refused_before_anything_starts() {
    for bad in [
        "",
        "abc",
        &SHA.to_uppercase(),
        &format!("{SHA}0"),
        &format!("../{}", &SHA[3..]),
        &SHA.replace('0', "g"),
    ] {
        let got = pull(&url(), bad, 10, 0);
        assert!(
            matches!(got, Err(FetchError::BadDigest(_))),
            "{bad:?}: {got:?}"
        );
        assert_eq!(got.unwrap_err().word(), "hash");
    }
}

/// A size of zero, over the host's cap, or over the hard ceiling is refused with the contract's
/// `size` word before a process exists; one byte under the cap is not refused.
///
/// Catches the declared size trusted: a source could otherwise be asked for a gigabyte the
/// host never agreed to take.
///
/// Mutants seen red: `>` for `>=` at the cap (the exact-cap case); the `min` with the ceiling
/// removed (a cap over 1 GiB in the settings).
#[test]
fn a_size_outside_the_hosts_cap_is_refused_before_anything_starts() {
    for size in [0, (1 << 20) + 1, u64::MAX] {
        let got = pull(&url(), SHA, size, 0);
        assert!(
            matches!(got, Err(FetchError::Size { .. })),
            "{size}: {got:?}"
        );
        assert_eq!(got.unwrap_err().word(), "size");
    }
    // At the cap the check passes and the (absent) program is what fails.
    let at_cap = pull(&url(), SHA, 1 << 20, 0);
    assert!(
        matches!(at_cap, Err(FetchError::Pipe(PipeFault::Spawn(_)))),
        "{at_cap:?}"
    );
    // A settings cap above the ceiling is held to the ceiling.
    let dir = scratch_dir("fetch-ceiling");
    let mut out = File::create(dir.join("download")).expect("file");
    let high = Limits {
        max_component_bytes: u64::MAX,
        fetch_timeout: Duration::from_secs(60),
    };
    let got = fetch_component(
        &nowhere(),
        &url(),
        high,
        &Component {
            sha256: SHA,
            size: MAX_COMPONENT_CEILING + 1,
        },
        (&dir, 0),
        &mut out,
    );
    assert!(
        matches!(got, Err(FetchError::Size { cap, .. }) if cap == MAX_COMPONENT_CEILING),
        "{got:?}"
    );
}

/// Room is the tarball, its extraction (a gigabyte, or ten times the size if smaller) and the
/// release it replaces.
#[test]
fn the_room_needed_is_the_tarball_its_extraction_and_the_release_replaced() {
    assert_eq!(room_needed(100, 0), 100 + 1000);
    assert_eq!(room_needed(100, 7), 100 + 1000 + 7);
    let gib = 1_u64 << 30;
    assert_eq!(room_needed(512 << 20, 0), (512 << 20) + gib);
    assert_eq!(room_needed(u64::MAX, u64::MAX), u64::MAX);
}

/// A disk without the room is `disk`, and the fetch has not started.
///
/// Catches the check being skipped or comparing the wrong way round: a full disk would be
/// found by a failed write half way through a download instead.
///
/// Mutant seen red: `free < need` as `free > need`.
#[test]
fn a_disk_without_room_is_refused_before_anything_starts() {
    let dir = scratch_dir("fetch-disk");
    assert!(ensure_room(&dir, 1).is_ok());
    let got = ensure_room(&dir, u64::MAX);
    assert!(matches!(got, Err(FetchError::Disk { .. })), "{got:?}");
    assert_eq!(got.unwrap_err().word(), "disk");
    // Through the whole step: the replaced release alone is more than any disk has.
    let got = pull(&url(), SHA, 10, u64::MAX / 2);
    assert!(matches!(got, Err(FetchError::Disk { .. })), "{got:?}");
}

/// A source that is not a plain https URL is refused (settings refuse it too; the child checks
/// again, so a bug in either is not one mistake from a request for another host).
///
/// Mutant seen red: the scheme check removed from `https_base`'s caller.
#[test]
fn a_source_that_is_not_a_plain_https_url_is_refused() {
    for base in [
        "http://mirror.example",
        "https://user@mirror.example",
        "https://mirror.example/?q=1",
        "https://mirror.example/#f",
        "https://MIRROR.example",
        "https://mirror.example/kbf/",
        "file:///etc",
        "",
    ] {
        let source = Source::Url {
            base: base.to_owned(),
            ca_file: None,
        };
        let got = pull(&source, SHA, 10, 0);
        assert!(
            matches!(got, Err(FetchError::Source(..))),
            "{base:?}: {got:?}"
        );
    }
    let frontend = |url: &str| {
        Source::Frontend(FrontendSource {
            url: url.to_owned(),
            ca_file: "/x/ca.pem".into(),
            node_cert: "/x/cert.pem".into(),
            node_key: "/x/key.pem".into(),
        })
    };
    for url in [
        "http://front.example:9000",
        "https://front.example:9000/v1",
        "https://u@front.example",
        "https://FRONT.example",
    ] {
        let got = pull(&frontend(url), SHA, 10, 0);
        assert!(
            matches!(got, Err(FetchError::Source(..))),
            "{url:?}: {got:?}"
        );
    }
}

/// Every refusal the parent makes before a byte arrives carries the contract's stable word.
#[test]
fn the_refusal_words_are_the_contracts() {
    let words: Vec<&str> = [
        FetchError::BadDigest(String::new()),
        FetchError::Size { size: 0, cap: 0 },
        FetchError::Disk {
            need: 0,
            free: 0,
            path: PathBuf::new(),
        },
        FetchError::Source(String::new(), ""),
        FetchError::Digest {
            actual: String::new(),
            want: String::new(),
        },
        FetchError::Pipe(PipeFault::Oversize { size: 0 }),
    ]
    .iter()
    .map(FetchError::word)
    .collect();
    assert_eq!(words, ["hash", "size", "disk", "fetch", "hash", "fetch"]);
}

/// The words a person reads when a child ends short: what the child said is in them, when it
/// said anything.
#[test]
fn a_short_read_says_what_the_child_said() {
    use super::pipe::PipeFault;
    let short = |log: &str| {
        PipeFault::Short {
            got: 3,
            size: 10,
            log: log.to_owned(),
        }
        .to_string()
    };
    assert_eq!(
        short(""),
        "the child ended after 3 of the 10 bytes declared"
    );
    assert_eq!(
        short("source unreachable: boom"),
        "the child ended after 3 of the 10 bytes declared (source unreachable: boom)"
    );
}

/// What `poll` answers: ready, not ready, interrupted (look again), or a real failure.
///
/// Catches an interrupted wait taken for a failure (a fetch killed by a stray signal) and a
/// failure taken for "not yet" (a loop that never ends).
#[test]
fn a_poll_that_was_interrupted_is_not_an_error_and_another_failure_is() {
    use super::pipe::polled;
    let eintr = || io::Error::from(io::ErrorKind::Interrupted);
    let other = || io::Error::from(io::ErrorKind::OutOfMemory);
    assert!(polled(1, || unreachable!("no failure")).expect("ready"));
    assert!(!polled(0, || unreachable!("no failure")).expect("not yet"));
    assert!(!polled(-1, eintr).expect("interrupted"));
    assert_eq!(
        polled(-1, other).map_err(|e| e.kind()),
        Err(io::ErrorKind::OutOfMemory)
    );
}

/// What one read of the child's output comes to: the end, bytes, "look again" for a read that
/// would block or was interrupted, and an error for the rest.
#[test]
fn a_read_that_would_block_is_looked_at_again_and_a_failed_one_is_an_error() {
    use super::pipe::{Chunk, chunk};
    assert_eq!(chunk(Ok(0)).expect("end"), Chunk::End);
    assert_eq!(chunk(Ok(7)).expect("data"), Chunk::Data(7));
    for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted] {
        assert_eq!(chunk(Err(kind.into())).expect("again"), Chunk::Again);
    }
    let got = chunk(Err(io::ErrorKind::BrokenPipe.into()));
    assert!(matches!(got, Err(PipeFault::Read(_))), "{got:?}");
}

/// A directory that cannot be measured is a refusal, not a pass.
#[test]
fn a_directory_that_cannot_be_measured_is_a_refusal() {
    let got = ensure_room(Path::new("/nonexistent/kbf-fetch"), 1);
    assert!(
        matches!(got, Err(FetchError::Pipe(PipeFault::Write(_)))),
        "{got:?}"
    );
}

/// A descriptor that cannot be switched to non-blocking is an error, not a pipe read that waits.
#[test]
fn a_descriptor_that_is_not_open_cannot_be_made_non_blocking() {
    use std::os::fd::BorrowedFd;
    // SAFETY: 901 is a number no test of this crate opens; the only use of it is an `fcntl`
    // that reports it is not open.
    let closed = unsafe { BorrowedFd::borrow_raw(901) };
    let got = super::pipe::set_nonblocking(closed);
    assert_eq!(got.map_err(|e| e.raw_os_error()), Err(Some(libc::EBADF)));
}

/// What one read does to the running count: bytes are counted, hashed and written; the end and
/// a read to look at again change nothing; a read past the declared size is refused before it is
/// written; a read that fails and a write that fails are faults.
///
/// Catches a count taken after the write (the oversize byte lands on disk), a hash of the wrong
/// slice, and an interrupted read taken for the end (a short file judged complete).
#[test]
fn a_read_is_counted_hashed_and_written_or_refused() {
    use super::pipe::Tally;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use std::time::Instant;
    struct Refusing;
    impl Write for Refusing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::StorageFull.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut tally = Tally::new(6, Instant::now());
    let mut out = Vec::new();
    assert!(!tally.take(Ok(3), b"abcXXX", &mut out).expect("data"));
    assert!(
        !tally
            .take(Err(io::ErrorKind::WouldBlock.into()), b"", &mut out)
            .expect("again")
    );
    assert!(!tally.take(Ok(3), b"def", &mut out).expect("data"));
    assert!(tally.take(Ok(0), b"", &mut out).expect("end"));
    assert_eq!((tally.count, out.as_slice()), (6, &b"abcdef"[..]));
    assert_eq!(
        tally.hash.clone().finalize()[..],
        Sha256::digest(b"abcdef")[..]
    );
    let got = tally.take(Ok(1), b"g", &mut out);
    assert!(
        matches!(got, Err(PipeFault::Oversize { size: 6 })),
        "{got:?}"
    );
    assert_eq!(out, b"abcdef", "the byte over the size is not written");
    let got = tally.take(Err(io::ErrorKind::BrokenPipe.into()), b"", &mut out);
    assert!(matches!(got, Err(PipeFault::Read(_))), "{got:?}");
    let got = Tally::new(6, Instant::now()).take(Ok(1), b"a", &mut Refusing);
    assert!(matches!(got, Err(PipeFault::Write(_))), "{got:?}");
}

/// The fetch child's program is this program's own file, run as the fetch user's ids: a
/// user that exists gives both, one that does not (or root) is a typed error, not a guess or
/// a panic. The host's fetch user is named by platform: `_kbf-fetch` is not on a Linux box.
///
/// Catches the child started as the wrong file or with no user (it would run as root), a
/// missing fetch user turned into a default, and the platform's name not used.
#[cfg(target_os = "linux")]
#[test]
fn the_child_is_this_program_as_the_fetch_user_or_a_typed_error() {
    let exe = std::env::current_exe().expect("a test has a file");
    let child = ChildProgram::of_this_program_as("nobody").expect("the user nobody exists");
    assert_eq!(child.program, exe);
    assert_eq!(child.run_as, user::lookup("nobody").ok());
    let got = ChildProgram::of_this_program_as("kbf-no-such-user-xyz");
    assert!(
        matches!(&got, Err(ProgramFault::User(UserFault::Missing(name))) if name == "kbf-no-such-user-xyz"),
        "{got:?}"
    );
    let got = ChildProgram::of_this_program_as("root");
    assert!(
        matches!(got, Err(ProgramFault::User(UserFault::Root(_)))),
        "root is never the fetch user"
    );
    let got = ChildProgram::of_this_program(Platform::MacOs);
    assert!(
        matches!(&got, Err(ProgramFault::User(UserFault::Missing(name))) if name == "_kbf-fetch"),
        "{got:?}"
    );
}
