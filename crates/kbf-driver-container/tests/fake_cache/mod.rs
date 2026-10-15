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
use kbf_daemon::cache::CacheConfig;
use kbf_driver_container::{CacheUse, MemoryCas, PodmanConfig, PodmanRuntime};

/// A cache in `dir/cache` for leases under `scratch`, for the runtime to open.
fn cache(dir: &Path, scratch: &Path) -> CacheUse {
    let at = dir.join("cache");
    std::fs::create_dir_all(&at).expect("cache dir");
    CacheUse {
        config: CacheConfig::new(at, scratch.to_owned(), 1 << 30),
        helper: PathBuf::from("/kbf/bin/kbf-daemon"),
        trees: None,
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
    let dir = (cache.config.dir)
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
    let steps = [
        "inode-links",
        "$UPPER/stat",
        "$ROOT/src/a.txt",
        "$UPPER/out/o.txt",
    ];
    let first = fake.run(1, &spec, &steps).await.expect("first");
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
    fake.run(2, &spec, &steps).await.expect("second");
    let inputs = fake.runtime.last_inputs().expect("counted");
    assert_eq!(
        (inputs.cache_hits, inputs.fetched_bytes),
        (2, 0),
        "{inputs}"
    );
    fake.assert_clean(2);
}

/// Catches the private copies chosen from output paths without the working directory
/// (outputs are relative to it, input paths to the input root): with the working
/// directory `pkg`, the input `pkg/out/o.txt` that the output `out/o.txt` names is a
/// private copy, not the cache's entry, while `pkg/src/a.txt` is the entry.
#[tokio::test]
async fn an_output_named_input_under_the_working_directory_is_a_private_copy() {
    let fake = cached("workdir");
    let mut spec = Spec::new(&image(), "unused");
    spec.working_directory = "pkg".to_owned();
    spec.inputs = vec![
        ("pkg/src/a.txt", b"input bytes", false),
        ("pkg/out/o.txt", b"input bytes", false),
    ];
    spec.outputs = vec!["out/o.txt".to_owned(), "stat".to_owned()];
    let steps = [
        "inode-links",
        "$UPPER/pkg/stat",
        "$ROOT/pkg/src/a.txt",
        "$UPPER/pkg/out/o.txt",
    ];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{result:?}");
    let entry = entry_inode(&fake, b"input bytes");
    let stat =
        String::from_utf8(blob(&fake.cas, result.output_files[1].digest.as_ref())).expect("utf-8");
    let lines: Vec<&str> = stat.lines().collect();
    assert_eq!(lines[0], format!("{entry} 2"), "{stat}");
    assert_eq!(
        lines[1].split(' ').collect::<Vec<_>>()[1],
        "1",
        "the output-named input under the working directory is the entry: {stat}"
    );
    assert!(!lines[1].starts_with(&format!("{entry} ")), "{stat}");
    fake.assert_clean(1);
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
        .run(1, &spec, &["exit", "4"])
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
        let outcome = fake
            .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
            .await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Failed(why)) if why.starts_with("podman unshare chown-dirs: ")),
            "{owner}: {outcome:?}"
        );
        fake.assert_clean(1);
    }
}

/// Catches the cache used on storage its overlay checks did not pass on: another
/// graph driver, fuse-overlayfs (a `mount_program`), or a `podman info` that fails or
/// answers what cannot be read. Each runs uncached and leaves the cache directory as
/// it found it (the "open the cache before the check" mutant takes its lock and lays
/// it out); kernel overlay with other options keeps it.
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
        let given = cache(
            &base.join(format!("{graph:?}-{fails}").replace(['/', '"', ' ', '{', '}', ':'], "_")),
            &config.scratch,
        );
        let dir = given.config.dir.clone();
        let shown = format!("{given:?}");
        assert!(
            shown.contains(&format!("{dir:?}")) && shown.contains("/kbf/bin/kbf-daemon"),
            "a CacheUse shown without its directory or helper: {shown}"
        );
        config.cache = Some(given);
        let runtime = PodmanRuntime::new(config, Arc::new(MemoryCas::new())).expect("runtime");
        let touched = std::fs::read_dir(&dir).expect("list").next().is_some();
        assert_eq!(
            touched,
            runtime.cached(),
            "{graph:?} {fails}: the cache directory was touched by a runtime without its cache, or not laid out by one with it"
        );
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

/// A fake whose runtime has a cache and a tree store of its own.
fn treed(name: &str) -> Fake {
    let dir = crate::support::scratch(&format!("fake-trees-{name}"));
    Fake::with(name, |config| {
        let mut given = cache(&dir, &config.scratch);
        given.trees = Some(kbf_daemon::trees::TreeConfig::new(
            dir.join("trees"),
            1 << 30,
        ));
        config.cache = Some(given);
    })
}

/// A toolchain of `count` files under `toolchain/`, and one small file beside it.
fn toolchain_inputs(count: usize) -> Vec<crate::support::Input> {
    let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    let mut inputs: Vec<crate::support::Input> = (0..count)
        .map(|i| {
            let path = leak(format!("toolchain/d{}/f{i}", i % 7));
            let bytes: &'static [u8] = leak(format!("zig file {i}")).as_bytes();
            (path, bytes, false)
        })
        .collect();
    inputs.push(("src/a.txt", b"small", false));
    inputs
}

/// Catches shared trees not reaching the container: the large directory is a read-only
/// `--volume` of the store's tree at its path under the exec root (the "drop the
/// mounts" mutant leaves an empty directory the action finds nothing in; the "drop
/// `:ro`" one is the string below), its 300 files are not written for the lease (the
/// cache counted 1 file, the small one), the action reads the tree's bytes through the
/// mount, the second lease builds nothing, and the lease's scratch directory is clean
/// while the tree stays for the next one.
#[tokio::test]
async fn a_large_directory_is_mounted_read_only_and_not_linked() {
    let fake = treed("mounts");
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = toolchain_inputs(300);
    spec.outputs = vec!["seen".to_owned()];
    let steps = ["copy", "$UPPER/seen", "$ROOT/toolchain/d1/f1"];
    let first = fake.run(1, &spec, &steps).await.expect("first");
    assert_eq!(first.exit_code, 0, "{first:?}");
    assert_eq!(
        blob(&fake.cas, first.output_files[0].digest.as_ref()),
        b"zig file 1"
    );
    let inputs = fake.runtime.last_inputs().expect("counted");
    assert_eq!(
        inputs.files, 1,
        "only the small file is the lease's own: {inputs}"
    );

    let args = std::fs::read_to_string(fake.state.join("create.args")).expect("create.args");
    let volumes: Vec<&str> = args
        .lines()
        .filter(|a| a.starts_with("--volume="))
        .collect();
    assert_eq!(volumes.len(), 2, "{args}");
    let (source, rest) = volumes[1]
        .strip_prefix("--volume=")
        .and_then(|v| v.split_once(":/kbf/root/toolchain:"))
        .expect("the tree's volume");
    assert_eq!(rest, "ro,nosuid,nodev");
    assert!(source.contains("/trees/"), "{source}");
    assert!(Path::new(source).is_dir(), "the tree outlives the lease");
    fake.assert_clean(1);

    let stats = fake.runtime.tree_stats().expect("a store");
    assert_eq!(
        (stats.builds, stats.hits, stats.trees),
        (1, 0, 1),
        "{stats:?}"
    );
    fake.run(2, &spec, &steps).await.expect("second");
    let stats = fake.runtime.tree_stats().expect("a store");
    assert_eq!((stats.builds, stats.hits), (1, 1), "{stats:?}");
    fake.assert_clean(2);
}

/// Catches an output under a toolchain directory left to the tree: with the output
/// `toolchain/d1/o`, nothing is mounted and the container gets no second volume.
#[tokio::test]
async fn an_output_under_a_large_directory_keeps_it_out_of_the_mounts() {
    let fake = treed("output");
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = toolchain_inputs(300);
    spec.outputs = vec!["toolchain/d1/o".to_owned()];
    let steps = ["write-lines", "$UPPER/toolchain/d1/o", "made"];
    let result = fake.run(1, &spec, &steps).await.expect("run");
    assert_eq!(result.exit_code, 0, "{result:?}");
    let args = std::fs::read_to_string(fake.state.join("create.args")).expect("create.args");
    assert_eq!(
        args.lines().filter(|a| a.starts_with("--volume=")).count(),
        1,
        "{args}"
    );
    assert_eq!(fake.runtime.tree_stats().expect("a store").builds, 0);
    fake.assert_clean(1);
}

/// Catches the lease dropping its pins before the container is gone (the "drop
/// `lease.trees`" mutant: a tree mounted into a running action could be evicted under
/// it): while the action runs, the store reports its tree pinned; once the run is over,
/// not pinned, and the tree is still there for the next lease.
#[tokio::test]
async fn a_tree_is_pinned_while_its_container_runs() {
    let fake = treed("pinned");
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = toolchain_inputs(300);
    fake.action(&["wait-for", "$ROOT/go", "30"]);
    let action = crate::support::store_action(&fake.cas, &spec);
    let runtime = Arc::clone(&fake.runtime);
    let work = crate::support::work(1, action, kbf_types::Resources::new(2000, 1 << 30));
    let run = tokio::spawn(async move { kbf_daemon::Runtime::run(&*runtime, work).await });
    fake.wait_for_start().await;
    let stats = fake.runtime.tree_stats().expect("a store");
    assert_eq!((stats.trees, stats.pinned), (1, 1), "{stats:?}");
    std::fs::write(fake.lease_dir(1).join("root/go"), b"").expect("release the action");
    let result = run.await.expect("joined").expect("ran");
    assert_eq!(result.exit_code, 0, "{result:?}");
    let stats = fake.runtime.tree_stats().expect("a store");
    assert_eq!((stats.trees, stats.pinned), (1, 0), "{stats:?}");
    fake.assert_clean(1);
}
