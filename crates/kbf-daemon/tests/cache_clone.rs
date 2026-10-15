//! The input cache in clone mode (`LinkMode::Clone`, `docs/design/worker-cache.md`, "C2,
//! macOS"): every name of a lease is its own writable file cloned from the cache's
//! entry, never a link to it. On macOS the clone is the real `clonefile`; elsewhere
//! these tests give the cache a "clone" that copies (and refuses to overwrite, as
//! `clonefile` does), so the placing logic runs on every platform.

mod support;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kbf_daemon::cache::{Cache, CacheConfig, LinkMode};
use support::cache_kit::{
    CountingCas, FakeClock, Gates, config, digest, entry_path, file, key, lease, stop,
};
use support::scratch;

const GIB: u64 = 1 << 30;

fn meta(path: &Path) -> std::fs::Metadata {
    std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// What `clonefile` does, for the platforms that have none: a new file (an existing
/// name is an error) with the source's bytes and mode.
#[cfg(not(target_os = "macos"))]
fn copying_clone(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut source = std::fs::File::open(from)?;
    let mut clone = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    std::io::copy(&mut source, &mut clone)?;
    clone.set_permissions(source.metadata()?.permissions())
}

/// A cache in `dir` in clone mode, its clone `copying_clone` off macOS.
fn clone_config(dir: &Path, gates: &Arc<Gates>) -> CacheConfig {
    #[allow(unused_mut)] // The link is only replaced off macOS.
    let mut config = config(dir, GIB, Arc::clone(gates), &FakeClock::at(1000));
    config.link_mode = Some(LinkMode::Clone);
    #[cfg(not(target_os = "macos"))]
    {
        config.link = Arc::new(copying_clone);
    }
    config
}

/// Catches a name that is a link to the entry (an action writing an input would change
/// the cache for every later lease), a clone left read-only as the entry is, or two
/// names of one blob sharing an inode: three names of two blobs, one blob twice. Every
/// name is its own inode with 0644 (0755 executable) and its own bytes to write; the
/// entry stays one link at 0444 (0555) with its bytes; a write to a name changes
/// neither the other name nor the next lease; the second lease fetches nothing.
#[tokio::test]
async fn every_name_is_its_own_writable_file_and_a_write_never_reaches_the_entry() {
    let dir = scratch("clone-own");
    let gates = Gates::new();
    let cache = Cache::open(clone_config(&dir, &gates)).expect("open");
    assert_eq!(cache.link_mode(), LinkMode::Clone);
    let cas = CountingCas::default();
    let root = cas.tree(&[
        file("a", b"shared bytes", false),
        file("b", b"shared bytes", false),
        file("x", b"#!tool\n", true),
    ]);
    let (one, first) = lease(&cas, &cache, &root, &dir, "one")
        .await
        .expect("first");
    let plain = meta(&entry_path(&dir, &key(b"shared bytes", false)));
    let tool = meta(&entry_path(&dir, &key(b"#!tool\n", true)));
    for (name, mode, entry) in [
        ("a", 0o644, &plain),
        ("b", 0o644, &plain),
        ("x", 0o755, &tool),
    ] {
        let got = meta(&one.join(name));
        assert_eq!(got.permissions().mode() & 0o7777, mode, "{name}");
        assert_ne!(got.ino(), entry.ino(), "{name} is the entry");
        assert_eq!(got.nlink(), 1, "{name} is linked");
    }
    assert_ne!(meta(&one.join("a")).ino(), meta(&one.join("b")).ino());
    assert_eq!((plain.nlink(), tool.nlink()), (1, 1), "the entry is linked");
    assert_eq!(plain.permissions().mode() & 0o7777, 0o444);
    assert_eq!(tool.permissions().mode() & 0o7777, 0o555);
    assert_eq!(first.link_fallbacks, 0);

    std::fs::write(one.join("a"), b"changed").expect("an input is writable");
    let entry = entry_path(&dir, &key(b"shared bytes", false));
    assert_eq!(std::fs::read(&entry).expect("entry"), b"shared bytes");
    assert_eq!(std::fs::read(one.join("b")).expect("b"), b"shared bytes");

    let (two, second) = lease(&cas, &cache, &root, &dir, "two")
        .await
        .expect("second");
    assert_eq!(std::fs::read(two.join("a")).expect("a"), b"shared bytes");
    assert_eq!(cas.reads_of(&digest(b"shared bytes")), 1);
    assert_eq!(cas.reads_of(&digest(b"#!tool\n")), 1);
    assert_eq!((second.cache_hits, second.fetched_bytes), (3, 0));
    #[cfg(target_os = "macos")]
    assert_eq!(meta(&two.join("b")).mtime(), kbf_daemon::cache::FIXED_MTIME);
    stop(cache, &gates);
}

/// Catches a clone the filesystem refuses (`ENOTSUP`: not APFS; `EXDEV`) failing the
/// lease: each name is a private copy, writable, counted as one fallback per key.
#[tokio::test]
async fn a_clone_the_filesystem_refuses_is_a_private_copy() {
    for (errno, name) in [(libc::ENOTSUP, "enotsup"), (libc::EXDEV, "exdev")] {
        let dir = scratch(&format!("clone-refused-{name}"));
        let gates = Gates::new();
        let mut config = clone_config(&dir, &gates);
        config.link =
            Arc::new(move |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(errno)));
        let cache = Cache::open(config).expect("open");
        let cas = CountingCas::default();
        let root = cas.tree(&[file("a", b"bytes", true), file("b", b"bytes", true)]);
        let (at, stats) = lease(&cas, &cache, &root, &dir, "one")
            .await
            .expect("written");
        assert_eq!(stats.link_fallbacks, 1, "{name}");
        for path in ["a", "b"] {
            let got = meta(&at.join(path));
            assert_eq!(got.permissions().mode() & 0o7777, 0o755, "{name} {path}");
            assert_eq!(got.nlink(), 1, "{name} {path}");
            assert_eq!(std::fs::read(at.join(path)).expect("read"), b"bytes");
        }
        assert_ne!(meta(&at.join("a")).ino(), meta(&at.join("b")).ino());
        stop(cache, &gates);
    }
}

/// Catches a clone refused for another reason (`EPERM`) being copied over, hiding a
/// filesystem that is wrong, instead of failing the lease and naming the path.
#[tokio::test]
async fn a_clone_refused_for_another_reason_fails_the_lease() {
    let dir = scratch("clone-eperm");
    let gates = Gates::new();
    let mut config = clone_config(&dir, &gates);
    config.link =
        Arc::new(|_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(libc::EPERM)));
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    let outcome = lease(
        &cas,
        &cache,
        &cas.tree(&[file("a", b"bytes", false)]),
        &dir,
        "one",
    )
    .await;
    assert!(
        matches!(&outcome, Err(kbf_daemon::tree::TreeError::Io { path, .. }) if path.ends_with("a")),
        "{outcome:?}"
    );
    stop(cache, &gates);
}

/// Catches an entry gone between its check and its clone (an eviction, or someone
/// removing it) failing the lease, and the names already cloned in that attempt left
/// behind to make the retry fail with `EEXIST`: the clone of the second name finds the
/// entry removed; the key is settled again (fetched a second time), every name is
/// placed once, and the bytes are right.
#[tokio::test]
async fn an_entry_that_vanishes_while_cloning_is_fetched_again() {
    let dir = scratch("clone-gone");
    let gates = Gates::new();
    let mut config = clone_config(&dir, &gates);
    let real = Arc::clone(&config.link);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    config.link = Arc::new(move |from: &Path, to: &Path| {
        if counted.fetch_add(1, Ordering::SeqCst) == 1 {
            std::fs::remove_file(from)?;
        }
        real(from, to)
    });
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    let root = cas.tree(&[
        file("a", b"bytes", false),
        file("b", b"bytes", false),
        file("c", b"bytes", false),
    ]);
    let (at, _) = lease(&cas, &cache, &root, &dir, "one")
        .await
        .expect("written");
    for path in ["a", "b", "c"] {
        assert_eq!(std::fs::read(at.join(path)).expect("read"), b"bytes");
    }
    assert_eq!(cas.reads_of(&digest(b"bytes")), 2);
    stop(cache, &gates);
}

/// Catches a cache that cannot keep a fill (its `tmp/` is not writable) failing the
/// lease, or handing it names that are links or read-only: the bytes are written to
/// each name, as without the cache, each its own writable file.
#[tokio::test]
async fn a_cache_that_cannot_keep_a_fill_still_gives_writable_files() {
    let dir = scratch("clone-io");
    let gates = Gates::new();
    let cache = Cache::open(clone_config(&dir, &gates)).expect("open");
    let cas = CountingCas::default();
    let tmp = dir.join("cache/v1/tmp");
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    let outcome = lease(
        &cas,
        &cache,
        &cas.tree(&[file("a", b"no room", false), file("b", b"no room", true)]),
        &dir,
        "full",
    )
    .await;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let (at, _) = outcome.expect("the lease ran without the cache");
    for (path, mode) in [("a", 0o644), ("b", 0o755)] {
        let got = meta(&at.join(path));
        assert_eq!(got.permissions().mode() & 0o7777, mode, "{path}");
        assert_eq!(got.nlink(), 1, "{path}");
        assert_eq!(std::fs::read(at.join(path)).expect("read"), b"no room");
    }
    assert!(!cache.holds(&key(b"no room", false)));
    stop(cache, &gates);
}
