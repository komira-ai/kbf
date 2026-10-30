//! The driver with the node's input cache, against the stand-in `crun`
//! (`support/fake.rs`): inputs linked from the cache, the input root's directories
//! alone handed to the container, an input an output names kept a private copy, a cache
//! that cannot be opened, and the shared trees. The fake runs the action as a plain
//! process on the host, so what the container cannot do to a cached input is checked on
//! real `crun` (`crun_cache.rs`).
//!
//! A module of `fake_crun.rs`, not a test binary of its own, for the reason
//! `fake_memory` gives: the coverage job counts the driver's lines per test binary.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::support::fake::{Fake, image};
use crate::support::{Spec, blob};
use kbf_daemon::RuntimeError;
use kbf_daemon::cache::CacheConfig;
use kbf_driver_container::{CacheUse, ContainerConfig, ContainerRuntime, MemoryCas};

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
    .owning(dir)
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
/// container's root (every cached inode re-owned: the "`chown -hR 1001:1001` over `root`"
/// defect at the fake's level). The action reports the inode and link count of an
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
        .filter(|e| e.starts_with("chown") || e == "run ended")
        .collect();
    let rw = lease.join("rw").display().to_string();
    assert_eq!(
        owners,
        [
            format!("chown-dirs 1001:1001 {root}"),
            format!("chown 1001:1001 {upper} {work} {rw}"),
            "run ended".to_owned(),
            format!("chown-dirs 0:0 {root}"),
            format!("chown 0:0 {upper} {work} {rw}"),
        ]
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

/// Catches a cache that cannot be opened failing the node or the lease: the runtime
/// warns and runs uncached, and a lease still runs. (The cache's directory is under a
/// file, so it cannot be made.)
#[test]
fn a_cache_that_cannot_be_opened_runs_uncached() {
    let base = crate::support::scratch("fake-cache-unopenable");
    std::fs::write(base.join("file"), b"x").expect("a file");
    let fake = Fake::new("unopenable");
    let mut config: ContainerConfig = fake.config.clone();
    let mut given = cache(&base, &config.scratch);
    given.config.dir = base.join("file/cache");
    let shown = format!("{given:?}");
    assert!(
        shown.contains(&format!("{:?}", given.config.dir)) && shown.contains("/kbf/bin/kbf-daemon"),
        "a CacheUse shown without its directory or helper: {shown}"
    );
    config.cache = Some(given);
    let runtime = ContainerRuntime::new(config, Arc::new(MemoryCas::new())).expect("runtime");
    assert!(!runtime.cached());
}

/// What `--no-input-cache` gives the driver: no cache in its config. Catches a driver
/// without a cache that records input statistics, or writes inputs wrong: two actions
/// with the same input each get its bytes straight from the CAS (the action copies the
/// input into an output, whose bytes are compared), neither is counted as a cache hit,
/// and the input is a private file of its own (one link), as a cache link would not be.
/// The cached counterpart is `inputs_are_links_to_the_cache_and_only_directories_change_owner`.
#[tokio::test]
async fn without_a_cache_each_action_gets_its_inputs_from_the_cas() {
    let fake = Fake::new("no-cache");
    assert!(!fake.runtime.cached());
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = vec![("src/a.txt", b"input bytes", false)];
    spec.outputs = vec!["seen".to_owned(), "stat".to_owned()];
    for seq in [1, 2] {
        let steps = [
            "copy",
            "$UPPER/seen",
            "$ROOT/src/a.txt",
            ";",
            "inode-links",
            "$UPPER/stat",
            "$ROOT/src/a.txt",
        ];
        let result = fake.run(seq, &spec, &steps).await.expect("run");
        assert_eq!(result.exit_code, 0, "{result:?}");
        let seen = blob(&fake.cas, result.output_files[0].digest.as_ref());
        assert_eq!(seen, b"input bytes");
        let stat = String::from_utf8(blob(&fake.cas, result.output_files[1].digest.as_ref()))
            .expect("utf-8");
        assert!(
            stat.trim_end().ends_with(" 1"),
            "linked from somewhere: {stat}"
        );
        assert_eq!(fake.runtime.last_inputs(), None);
        fake.assert_clean(seq);
    }
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
    .owning(dir)
}

/// The bind mounts of shared trees in the last bundle: source, destination and options
/// of each mount below the exec root other than the overlay.
fn tree_mounts(fake: &Fake) -> Vec<(String, String, Vec<String>)> {
    let config = fake.config_json();
    let text = |v: &serde_json::Value| v.as_str().expect("a string").to_owned();
    config["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .filter(|m| {
            let destination = m["destination"].as_str().unwrap_or_default();
            destination.starts_with("/kbf/root/")
        })
        .map(|m| {
            let options = m["options"].as_array().expect("options");
            (
                text(&m["source"]),
                text(&m["destination"]),
                options.iter().map(&text).collect(),
            )
        })
        .collect()
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
/// bind mount of the store's tree at its path under the exec root (the "drop the
/// mounts" defect leaves an empty directory the action finds nothing in; the "drop
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

    let mounts = tree_mounts(&fake);
    assert_eq!(mounts.len(), 1, "{mounts:?}");
    let (source, destination, options) = &mounts[0];
    assert_eq!(destination, "/kbf/root/toolchain");
    assert_eq!(options, &["rbind", "ro", "nosuid", "nodev"]);
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
    let mounts = tree_mounts(&fake);
    assert!(mounts.is_empty(), "{mounts:?}");
    assert_eq!(fake.runtime.tree_stats().expect("a store").builds, 0);
    fake.assert_clean(1);
}

/// Catches the lease dropping its pins before the container is gone (the "drop
/// `lease.trees`" defect: a tree mounted into a running action could be evicted under
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

/// Catches a failed build of a tree leaving anything behind: a blob of the toolchain
/// that arrives corrupt fails the lease before the container is run, nothing is
/// left in the scratch directory, and the store names no tree (the next lease with the
/// blob right builds it).
#[tokio::test]
async fn a_corrupt_blob_in_a_tree_fails_the_lease_and_names_no_tree() {
    let fake = treed("corrupt");
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = toolchain_inputs(300);
    spec.outputs = vec!["seen".to_owned()];
    let bad = kbf_daemon::cas::digest_of(b"zig file 7");
    let steps = ["copy", "$UPPER/seen", "$ROOT/toolchain/d1/f1"];
    // The Action first: the CAS holds the toolchain once the action is stored.
    fake.action(&steps);
    let action = crate::support::store_action(&fake.cas, &spec);
    let run = |seq| {
        let work = crate::support::work(
            seq,
            action.clone(),
            kbf_types::Resources::new(2000, 1 << 30),
        );
        let runtime = Arc::clone(&fake.runtime);
        async move { kbf_daemon::Runtime::run(&*runtime, work).await }
    };
    fake.cas.corrupt(&bad, b"not what the digest names");
    let failed = run(1).await.expect_err("a corrupt blob");
    assert!(!matches!(failed, RuntimeError::Killed), "{failed:?}");
    assert_eq!(fake.runtime.tree_stats().expect("a store").trees, 0);
    fake.assert_clean(1);
    fake.cas.corrupt(&bad, b"zig file 7");
    let result = run(2).await.expect("with the blob right");
    assert_eq!(result.exit_code, 0, "{result:?}");
    assert_eq!(fake.runtime.tree_stats().expect("a store").trees, 1);
}

/// Catches counts the heartbeat reads that do not follow the cache: a runtime with no
/// cache reporting counts (zeros would read as "a cache that did nothing"), the first
/// lease's two inputs not counted as two fills with their bytes, the second lease's not
/// counted as two hits with theirs (and as fills again), and the tree store's own builds
/// and hits missing from the same report.
#[tokio::test]
async fn the_runtime_reports_the_fills_then_the_hits_of_its_leases() {
    use kbf_daemon::Runtime as _;
    let plain = Fake::new("counts-plain");
    assert_eq!(plain.runtime.input_cache(), None, "no cache, no counts");

    let fake = treed("counts");
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = toolchain_inputs(300);
    spec.outputs = vec!["seen".to_owned()];
    let steps = ["copy", "$UPPER/seen", "$ROOT/toolchain/d1/f1"];
    let key_bytes: u64 = spec
        .inputs
        .iter()
        .map(|(_, bytes, _)| bytes.len() as u64)
        .sum();
    assert_eq!(fake.runtime.input_cache().map(|c| c.fills), Some(0));

    fake.run(1, &spec, &steps).await.expect("first");
    let first = fake.runtime.input_cache().expect("counts");
    assert_eq!(
        (first.fills, first.fill_bytes),
        (301, key_bytes),
        "{first:?}"
    );
    assert_eq!((first.hits, first.hit_bytes), (0, 0), "{first:?}");
    assert_eq!(
        first.trees.map(|t| (t.builds, t.hits)),
        Some((1, 0)),
        "{first:?}"
    );

    fake.run(2, &spec, &steps).await.expect("second");
    let second = fake.runtime.input_cache().expect("counts");
    assert_eq!(
        (second.fills, second.fill_bytes),
        (301, key_bytes),
        "{second:?}"
    );
    assert_eq!(
        second.hits, 1,
        "only the small file is the lease's own: {second:?}"
    );
    assert_eq!(second.hit_bytes, 5, "{second:?}");
    assert_eq!(
        second.trees.map(|t| (t.builds, t.hits)),
        Some((1, 1)),
        "{second:?}"
    );
    assert_eq!(
        (second.evictions, second.evicted_bytes),
        (0, 0),
        "{second:?}"
    );
}
