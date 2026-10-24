//! What `fetch_component` refuses before a child exists or a byte is written, as a program that
//! links the library sees it. The program named as the child is not there, so a check that let a
//! fetch through would show as a failed start (`PipeFault::Spawn`) and not as the refusal each test
//! names. (The same checks are run inside the library's own tests; the child against real servers
//! is in `tests/fetch_e2e.rs`.)

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_installer::fetch::pipe::{PipeFault, Received};
use kbf_installer::fetch::{
    ChildProgram, Component, FetchError, FrontendSource, Limits, MAX_COMPONENT_CEILING, Source,
    ensure_room, fetch_component,
};

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn scratch(name: &str) -> PathBuf {
    kbf_host::testkit::scratch(&format!("fetch-refusals-{name}"))
}

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

fn frontend(url: &str) -> Source {
    Source::Frontend(FrontendSource {
        url: url.to_owned(),
        ca_file: "/x/ca.pem".into(),
        node_cert: "/x/cert.pem".into(),
        node_key: "/x/key.pem".into(),
    })
}

/// A program that cannot start.
fn nowhere() -> ChildProgram {
    ChildProgram {
        program: PathBuf::from("/nonexistent/kbf-installer"),
        run_as: None,
    }
}

/// `fetch_component` of `size` bytes named `sha` from `source`, into a new file of `dir`, with
/// `replaced` bytes of release to be replaced.
fn pull(
    dir: &Path,
    source: &Source,
    sha: &str,
    size: u64,
    replaced: u64,
) -> Result<Received, FetchError> {
    let mut out = File::create(dir.join("download")).expect("file");
    fetch_component(
        &nowhere(),
        source,
        limits(),
        &Component { sha256: sha, size },
        (dir, replaced),
        &mut out,
    )
}

/// Asserts that a digest that is not 64 lowercase hex digits is refused as `hash` before a
/// request is made: empty, short, upper case, one digit too long, a path, a letter past `f`.
#[test]
fn a_digest_that_is_not_lowercase_hex_is_refused() {
    let dir = scratch("digest");
    for bad in [
        "",
        "abc",
        &SHA.to_uppercase(),
        &format!("{SHA}0"),
        &format!("../{}", &SHA[3..]),
        &SHA.replace('0', "g"),
    ] {
        let got = pull(&dir, &url(), bad, 10, 0);
        assert!(
            matches!(got, Err(FetchError::BadDigest(_))),
            "{bad:?}: {got:?}"
        );
        assert_eq!(got.unwrap_err().word(), "hash");
    }
}

/// Asserts that a declared size of zero, over the host's cap or over the hard ceiling is refused
/// as `size`, naming the cap in force (the ceiling when the host's cap is above it), and that a
/// size at the cap is not refused.
#[test]
fn a_size_outside_the_hosts_cap_is_refused() {
    let dir = scratch("size");
    for size in [0, (1 << 20) + 1, u64::MAX] {
        let got = pull(&dir, &url(), SHA, size, 0);
        assert!(
            matches!(got, Err(FetchError::Size { cap, .. }) if cap == 1 << 20),
            "{size}: {got:?}"
        );
        assert_eq!(got.unwrap_err().word(), "size");
    }
    let at_cap = pull(&dir, &url(), SHA, 1 << 20, 0);
    assert!(
        matches!(at_cap, Err(FetchError::Pipe(PipeFault::Spawn(_)))),
        "{at_cap:?}"
    );
    let high = Limits {
        max_component_bytes: u64::MAX,
        fetch_timeout: Duration::from_secs(60),
    };
    let mut out = File::create(dir.join("download")).expect("file");
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

/// Asserts that a disk without the room is refused as `disk` (alone, and through the whole step
/// with a release to replace that no disk could hold), and that a directory that cannot be
/// measured is a write fault and not a pass.
#[test]
fn a_disk_without_room_is_refused() {
    let dir = scratch("disk");
    assert!(ensure_room(&dir, 1).is_ok());
    let got = ensure_room(&dir, u64::MAX);
    assert!(matches!(got, Err(FetchError::Disk { .. })), "{got:?}");
    assert_eq!(got.unwrap_err().word(), "disk");
    let got = pull(&dir, &url(), SHA, 10, u64::MAX / 2);
    assert!(matches!(got, Err(FetchError::Disk { .. })), "{got:?}");
    let got = ensure_room(Path::new("/nonexistent/kbf-fetch"), 1);
    assert!(
        matches!(got, Err(FetchError::Pipe(PipeFault::Write(_)))),
        "{got:?}"
    );
}

/// Asserts that a source that is not a plain https address in the form settings write is refused
/// before a child exists, whichever kind it is: a `url` source with another scheme, user
/// information, a query, a fragment, an upper-case host, a trailing slash or nothing, and a
/// `frontend` source with another scheme, a path, user information or an upper-case host.
#[test]
fn a_source_that_is_not_a_plain_https_url_is_refused() {
    let dir = scratch("source");
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
        let got = pull(&dir, &source, SHA, 10, 0);
        assert!(
            matches!(got, Err(FetchError::Source(..))),
            "{base:?}: {got:?}"
        );
        assert_eq!(got.unwrap_err().word(), "fetch");
    }
    for address in [
        "http://front.example:9000",
        "https://front.example:9000/v1",
        "https://u@front.example",
        "https://FRONT.example",
    ] {
        let got = pull(&dir, &frontend(address), SHA, 10, 0);
        assert!(
            matches!(got, Err(FetchError::Source(..))),
            "{address:?}: {got:?}"
        );
    }
}
