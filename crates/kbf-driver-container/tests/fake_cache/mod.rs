//! The driver with the node's input cache, against the stand-in `podman`
//! (`support/fake.rs`): inputs linked from the cache, the input root's directories
//! alone handed to the container, an input an output names kept a private copy, the
//! one retry of an exec that found its program busy, and the storage the cache runs
//! on. The fake runs the action as a plain process on the host, so what the container
//! cannot do to a cached input is checked on real Podman (`podman_cache.rs`).
//!
//! A module of `fake_podman.rs`, not a test binary of its own, for the reason
//! `fake_memory` gives: the coverage job counts the driver's lines per test binary.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::support::fake::{Fake, image};
use crate::support::{Spec, blob};
use kbf_daemon::RuntimeError;
use kbf_daemon::cache::{Cache, CacheConfig};
use kbf_driver_container::{CacheUse, MemoryCas, PodmanConfig, PodmanRuntime};

/// A cache in `dir/cache` for leases under `scratch`.
fn cache(dir: &Path, scratch: &Path) -> CacheUse {
    let at = dir.join("cache");
    std::fs::create_dir_all(&at).expect("cache dir");
    let cache = Cache::open(CacheConfig::new(at, scratch.to_owned(), 1 << 30)).expect("open");
    CacheUse {
        cache,
        helper: PathBuf::from("/kbf/bin/kbf-daemon"),
    }
}

/// A fake whose runtime links inputs from a cache of its own.
fn cached(name: &str) -> Fake {
    let dir = crate::support::scratch(&format!("fake-cache-{name}"));
    Fake::with(name, |config| {
        config.cache = Some(cache(&dir, &config.scratch))
    })
}

/// The inode of the cache's entry of `bytes` (plain).
fn entry_inode(fake: &Fake, bytes: &[u8]) -> u64 {
    let digest = kbf_daemon::cas::digest_of(bytes);
    let key = kbf_daemon::cache::Key::of(&digest, false).expect("a key");
    let cache = fake.config.cache.as_ref().expect("a cache");
    let dir = cache
        .cache
        .dir()
        .join("v1/blobs")
        .join(key.prefix())
        .join(key.name());
    std::fs::metadata(dir).expect("the entry").ino()
}

/// Catches the cache not consulted by the driver, an output-named input linked to the
/// cache (in the overlay's upper directory, which the container's root owns, the
/// action could then write the entry), and the whole input root handed to the
/// container's root (every cached inode re-owned: the "`chown -hR 1:1` over `root`"
/// mutant at the fake's level). The action reports the inode and link count of an
/// input and of an output-named input; the second lease fetches nothing.
#[tokio::test]
async fn inputs_are_links_to_the_cache_and_only_directories_change_owner() {
    let fake = cached("links");
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = vec![
        ("src/a.txt", b"input bytes", false),
        ("out/o.txt", b"input bytes", false),
    ];
    spec.outputs = vec!["out/o.txt".to_owned(), "stat".to_owned()];
    let script = r#"stat -c '%i %h' "$ROOT/src/a.txt" "$UPPER/out/o.txt" > "$UPPER/stat""#;
    let first = fake.run(1, &spec, script).await.expect("first");
    assert_eq!(first.exit_code, 0, "{first:?}");
    let entry = entry_inode(&fake, b"input bytes");
    let stat =
        String::from_utf8(blob(&fake.cas, first.output_files[1].digest.as_ref())).expect("utf-8");
    let lines: Vec<&str> = stat.lines().collect();
    assert_eq!(
        lines[0],
        format!("{entry} 2"),
        "the input is not the cache's entry: {stat}"
    );
    assert!(
        !lines[1].starts_with(&entry.to_string()),
        "the output-named input is the entry: {stat}"
    );
    assert!(lines[1].ends_with(" 1"), "{stat}");
    let lease = fake.lease_dir(1);
    let [root, upper, work] =
        ["root", "upper", "work"].map(|d| lease.join(d).display().to_string());
    let owners: Vec<String> = fake
        .events()
        .into_iter()
        .filter(|e| e.starts_with("chown") || e == "start ended")
        .collect();
    assert_eq!(
        owners,
        [
            format!("chown-dirs 1:1 {root}"),
            format!("chown 1:1 {upper} {work}"),
            "start ended".to_owned(),
            format!("chown-dirs 0:0 {root}"),
            format!("chown 0:0 {upper} {work}"),
        ]
    );
    assert!(
        fake.calls()
            .contains(&"unshare /kbf/bin/kbf-daemon".to_owned())
    );
    let inputs = fake.runtime.last_inputs().expect("counted");
    assert_eq!((inputs.files, inputs.cache_misses), (2, 2));
    fake.run(2, &spec, script).await.expect("second");
    let inputs = fake.runtime.last_inputs().expect("counted");
    assert_eq!(
        (inputs.cache_hits, inputs.fetched_bytes),
        (2, 0),
        "{inputs}"
    );
    fake.assert_clean(2);
}

/// C-9, at the fake's level (a real ETXTBSY cannot be timed from a test). Catches an
/// exec that found its program busy failing the lease (the "remove the retry"
/// mutant): `podman start` is run once more and the action's own exit is the result.
#[tokio::test]
async fn a_start_whose_exec_found_the_program_busy_runs_once_more() {
    let fake = cached("busy");
    let spec = Spec::new(&image(), "unused");
    fake.knob("busy-once", "");
    let result = fake
        .run(1, &spec, "exit 4")
        .await
        .expect("ran on the second start");
    assert_eq!(result.exit_code, 4);
    let starts = fake.calls().iter().filter(|c| *c == "start").count();
    assert_eq!(starts, 2);
    fake.assert_clean(1);
}

/// Catches a failed hand-over of the input root's directories ignored, before the
/// container (the action could write nothing) and after it (the outputs would be read
/// as a stranger): either fails the lease as the farm's.
#[tokio::test]
async fn a_failed_directory_hand_over_fails_the_lease() {
    for owner in ["1:1", "0:0"] {
        let fake = cached(&format!("chown-dirs-{}", &owner[..1]));
        fake.knob("chown-fails", owner);
        let outcome = fake.run(1, &Spec::new(&image(), "unused"), "exit 0").await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Failed(why)) if why.starts_with("podman unshare chown-dirs: ")),
            "{owner}: {outcome:?}"
        );
        fake.assert_clean(1);
    }
}

/// Catches the cache used on storage its overlay checks did not pass on: another
/// graph driver, fuse-overlayfs (a `mount_program`), or a `podman info` that fails or
/// answers what cannot be read. Each runs uncached; kernel overlay with other options
/// keeps it.
#[test]
fn the_cache_is_off_on_storage_its_checks_did_not_pass() {
    let fake = Fake::new("graph");
    let base = crate::support::scratch("fake-cache-graph");
    let with = |graph: Option<&str>, fails: bool| -> bool {
        match graph {
            Some(text) => fake.knob("graph", text),
            None => {
                let _ = std::fs::remove_file(fake.state.join("graph"));
            }
        }
        if fails {
            fake.knob("info-fails", "");
        } else {
            let _ = std::fs::remove_file(fake.state.join("info-fails"));
        }
        let mut config: PodmanConfig = fake.config.clone();
        config.cache = Some(cache(
            &base.join(format!("{graph:?}-{fails}").replace(['/', '"', ' ', '{', '}', ':'], "_")),
            &config.scratch,
        ));
        let runtime = PodmanRuntime::new(config, Arc::new(MemoryCas::new())).expect("runtime");
        runtime.cached()
    };
    assert!(with(None, false), "the default kernel overlay");
    assert!(with(
        Some("overlay {\"overlay.mountopt\":\"nodev\"}\n"),
        false
    ));
    assert!(!with(Some("vfs {}\n"), false), "vfs");
    assert!(
        !with(
            Some("overlay {\"overlay.mount_program\":\"/usr/bin/fuse-overlayfs\"}\n"),
            false
        ),
        "fuse-overlayfs"
    );
    assert!(
        !with(Some("overlay {not json\n"), false),
        "unreadable options"
    );
    assert!(!with(None, true), "info failed");
    assert!(!fake.runtime.cached(), "a runtime without a cache");
}
