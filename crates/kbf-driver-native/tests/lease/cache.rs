//! The native driver's input cache (`docs/design/worker-cache.md`, "C2, macOS"): a lease's
//! input files are clones of the node cache's entries, so a blob is fetched once per
//! node, not once per lease; an action may write its inputs without changing the cache;
//! and (macOS) the sandbox keeps the entries out of an action's reach.
//!
//! Off macOS the cache is given a "clone" that copies, as `kbf-daemon`'s `cache_clone`
//! tests do; macOS runs the real `clonefile`.
//!
//! In the `lease` test binary, not one of its own: the driver's generic `prepare` is counted
//! by its best single instantiation, one per test binary, so the cached and the uncached
//! input paths must run in the same binary.

use std::path::Path;
use std::sync::Arc;

use crate::support::{self, MemoryCas, Spec, config, run, runtime, scratch, stdout};
use kbf_daemon::cache::{Cache, CacheConfig, Key, LinkMode};
use kbf_daemon::cas::digest_of;
use kbf_driver_native::NativeConfig;

/// A cache in `dir/cache` for the leases under `dir/leases` ([`config`]), in clone mode.
fn cache_in(dir: &Path) -> Arc<Cache> {
    std::fs::create_dir_all(dir.join("cache")).expect("cache dir");
    std::fs::create_dir_all(dir.join("leases")).expect("leases dir");
    #[allow(unused_mut)] // The link is only replaced off macOS.
    let mut cache: CacheConfig = CacheConfig::new(dir.join("cache"), dir.join("leases"), 1 << 30);
    cache.link_mode = Some(LinkMode::Clone);
    #[cfg(not(target_os = "macos"))]
    {
        cache.link = Arc::new(|from: &Path, to: &Path| {
            let mut source = std::fs::File::open(from)?;
            let mut clone = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(to)?;
            std::io::copy(&mut source, &mut clone)?;
            clone.set_permissions(source.metadata()?.permissions())
        });
    }
    Cache::open(cache).expect("cache")
}

/// [`config`] with `cache`.
fn cached(dir: &Path, cache: &Arc<Cache>) -> NativeConfig {
    let mut config = config(dir);
    config.cache = Some(Arc::clone(cache));
    config
}

/// The cache's entry for an input `bytes` of a [`Spec`] (every input is executable).
fn entry(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let key = Key::of(&digest_of(bytes), true).expect("a key");
    dir.join("cache/v1/blobs")
        .join(key.prefix())
        .join(key.name())
}

/// Catches a native lease that fetches every input again (the driver ignoring its
/// cache: the 20 s per Mojo action, a toolchain's worth of bytes per lease): two leases
/// of the same inputs ask the CAS for each input blob once between them, and both
/// read the right bytes through their own files.
#[tokio::test]
async fn a_second_lease_of_the_same_inputs_fetches_no_blob() {
    let dir = scratch("cache-fetch");
    let cache = cache_in(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(cached(&dir, &cache), &cas);
    let mut spec = Spec::act("cat data");
    spec.inputs = vec![
        ("data".to_owned(), b"toolchain bytes\n".to_vec()),
        ("lib".to_owned(), b"other bytes\n".to_vec()),
    ];
    for seq in [1, 2] {
        let result = run(&rt, &cas, seq, &spec).await.expect("ran");
        assert_eq!(result.exit_code, 0, "lease {seq}");
        assert_eq!(support::stdout(&cas, &result), "toolchain bytes\n");
    }
    for bytes in [&b"toolchain bytes\n"[..], b"other bytes\n"] {
        assert_eq!(cas.reads_of(&digest_of(bytes)), 1, "{bytes:?}");
    }
}

/// Catches an input that is a link to the cache's entry, or read-only: an action
/// that writes its input (a tool that rewrites a file in place) must succeed, as
/// without the cache, and the next lease of the same inputs must see the original
/// bytes, from the cache, not a refetch.
#[tokio::test]
async fn an_action_that_writes_an_input_leaves_the_next_lease_the_original() {
    let dir = scratch("cache-write");
    let cache = cache_in(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(cached(&dir, &cache), &cas);
    let mut spec = Spec::act("append data changed ; cat data");
    spec.inputs = vec![("data".to_owned(), b"original\n".to_vec())];
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", support::stderr(&cas, &result));
    assert_eq!(support::stdout(&cas, &result), "original\nchanged\n");
    assert_eq!(
        std::fs::read(entry(&dir, b"original\n")).expect("entry"),
        b"original\n"
    );
    spec.argv = Spec::act("cat data").argv;
    let again = run(&rt, &cas, 2, &spec).await.expect("ran");
    assert_eq!(stdout(&cas, &again), "original\n");
    assert_eq!(cas.reads_of(&digest_of(b"original\n")), 1);
}

/// Catches the native driver's actions reaching the cache (they run as the cache's
/// owner; only the sandbox, which lets them write nowhere outside their lease, keeps
/// them out): a sandboxed action that chmods, appends to, removes or renames an entry
/// fails and leaves it as it was; the same writes unsandboxed go through, so the
/// refusals are the sandbox's.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_sandboxed_action_cannot_change_a_cache_entry() {
    use kbf_driver_native::network::Isolation;

    let dir = scratch("cache-sandbox");
    let cache = cache_in(&dir);
    let cas = Arc::new(MemoryCas::default());
    let sandboxed = config(&dir);
    assert!(matches!(sandboxed.isolation, Isolation::Sandbox(_)));
    let mut sandboxed = sandboxed;
    sandboxed.cache = Some(Arc::clone(&cache));
    let mut open = cached(&dir.join("unsandboxed"), &cache);
    open.isolation = Isolation::None;
    let (sandboxed, open) = (runtime(sandboxed, &cas), runtime(open, &cas));
    let mut fill = Spec::act("exit 0");
    fill.inputs = vec![("data".to_owned(), b"kept\n".to_vec())];
    run(&sandboxed, &cas, 1, &fill).await.expect("filled");
    let victim = entry(&dir, b"kept\n");
    assert_eq!(std::fs::read(&victim).expect("entry"), b"kept\n");
    let rows = [
        (
            "chmod",
            format!("chmod 600 {0} && echo x >> {0}", victim.display()),
        ),
        ("unlink", format!("rm -f {}", victim.display())),
        ("rename", format!("mv {0} {0}.moved", victim.display())),
    ];
    for (seq, (name, script)) in (2u64..).zip(&rows) {
        let mut attempt = Spec::sh(&format!("{script}; echo $?")).env("PATH", "/usr/bin:/bin");
        attempt.inputs = fill.inputs.clone();
        let result = run(&sandboxed, &cas, seq, &attempt).await.expect("ran");
        assert_ne!(stdout(&cas, &result).trim(), "0", "{name} went through");
        assert_eq!(std::fs::read(&victim).expect("entry"), b"kept\n", "{name}");
        let mode = std::fs::metadata(&victim).expect("entry").permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o555,
            "{name}"
        );
    }
    // The control: unsandboxed, the same writes succeed.
    for (seq, (name, script)) in (20u64..).zip(&rows) {
        let mut attempt = Spec::sh(&format!("{script}; echo $?")).env("PATH", "/usr/bin:/bin");
        attempt.inputs = fill.inputs.clone();
        let result = run(&open, &cas, seq, &attempt).await.expect("ran");
        assert_eq!(stdout(&cas, &result).trim(), "0", "control: {name} failed");
        // Back as it was for the next row.
        let moved = victim.with_extension("moved");
        if moved.exists() {
            std::fs::rename(&moved, &victim).expect("back");
        }
        std::fs::write(&victim, b"kept\n").ok();
        std::fs::set_permissions(&victim, std::os::unix::fs::PermissionsExt::from_mode(0o555))
            .expect("mode");
    }
}

/// What the directory clone does off macOS: the directory and all under it as copies,
/// modes and symlinks kept, to a name that must not exist, as `clonefile` does.
#[cfg(not(target_os = "macos"))]
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target)?;
        }
    }
    std::fs::set_permissions(to, std::fs::metadata(from)?.permissions())
}

/// A tree store in `dir/trees` that clones whole trees, counting the clones; the real
/// `clonefile` on macOS.
fn trees_in(
    dir: &Path,
) -> (
    Arc<kbf_daemon::trees::Trees>,
    Arc<std::sync::atomic::AtomicU64>,
) {
    use kbf_daemon::trees::{Delivery, TreeConfig, Trees};
    use std::sync::atomic::{AtomicU64, Ordering};
    let clones = Arc::new(AtomicU64::new(0));
    let mut config = TreeConfig::new(dir.join("trees"), 1 << 30);
    config.min_files = 4;
    let counted = Arc::clone(&clones);
    #[cfg(target_os = "macos")]
    let Delivery::Clone(real) = Delivery::platform() else {
        unreachable!("macOS clones trees")
    };
    #[cfg(not(target_os = "macos"))]
    let real: Arc<kbf_daemon::trees::CloneDirFn> = Arc::new(copy_dir);
    config.delivery = Delivery::Clone(Arc::new(move |from: &Path, to: &Path| {
        counted.fetch_add(1, Ordering::SeqCst);
        real(from, to)
    }));
    (Arc::new(Trees::open(config).expect("trees")), clones)
}

/// [`cached`] with the tree store `trees`.
fn treed(dir: &Path, cache: &Arc<Cache>, trees: &Arc<kbf_daemon::trees::Trees>) -> NativeConfig {
    let mut config = cached(dir, cache);
    config.trees = Some(Arc::clone(trees));
    config
}

/// Six files of a toolchain under `tc` and one beside it: `tc` is a tree (four files
/// qualify it in these tests).
fn toolchain() -> Vec<(String, Vec<u8>)> {
    let mut inputs: Vec<(String, Vec<u8>)> = (0..6)
        .map(|i| {
            (
                format!("tc/d{}/f{i}", i % 2),
                format!("tool {i}").into_bytes(),
            )
        })
        .collect();
    inputs.push(("top".to_owned(), b"top".to_vec()));
    inputs
}

/// Catches a native lease that does not take its tree as one directory clone (the
/// driver ignoring `trees`: every file cloned again for every lease; or a clone per
/// directory), an input in a tree the action may not write (a tool that rewrites a
/// toolchain file in place), and a write, removal or rename in one lease's copy that
/// reaches the store's tree or the next lease. Two leases of the same inputs make one
/// directory clone each; the first appends to a file, removes another and renames a
/// directory, the second sees all as they were; the tree in the store is as built; each
/// blob was fetched once.
#[tokio::test]
async fn a_lease_gets_its_tree_as_one_clone_and_may_write_it() {
    let dir = scratch("trees-clone");
    let cache = cache_in(&dir);
    let (trees, clones) = trees_in(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(treed(&dir, &cache, &trees), &cas);
    let mut spec = Spec::act(
        "append tc/d0/f0 changed ; remove tc/d1/f1 ; rename tc/d0 tc/moved ; cat tc/moved/f0",
    );
    spec.inputs = toolchain();
    let first = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(first.exit_code, 0, "{}", support::stderr(&cas, &first));
    assert_eq!(stdout(&cas, &first), "tool 0changed\n");
    assert_eq!(clones.load(std::sync::atomic::Ordering::SeqCst), 1);
    spec.argv = Spec::act("cat tc/d0/f0 ; test -e tc/d1/f1 ; test -d tc/d0").argv;
    let second = run(&rt, &cas, 2, &spec).await.expect("ran");
    assert_eq!(second.exit_code, 0, "{}", support::stderr(&cas, &second));
    assert_eq!(stdout(&cas, &second), "tool 0");
    assert_eq!(clones.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(trees.store().stats().builds, 1);
    let tree = tree_in(&dir);
    assert_eq!(std::fs::read(tree.join("d0/f0")).expect("tree"), b"tool 0");
    assert!(tree.join("d1/f1").exists());
    for (_, bytes) in &spec.inputs {
        assert_eq!(cas.reads_of(&digest_of(bytes)), 1, "{bytes:?}");
    }
}

/// The store's one tree.
fn tree_in(dir: &Path) -> std::path::PathBuf {
    let found: Vec<_> = std::fs::read_dir(dir.join("trees"))
        .expect("store")
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    found.into_iter().next().expect("one")
}

/// Catches the native driver's actions reaching the tree store (they run as its owner;
/// only the sandbox, which lets them write nowhere outside their lease, keeps them
/// out): a sandboxed action that chmods, appends to, removes or renames a file of the
/// store's tree, or removes the tree, fails and leaves it as it was; the same writes
/// unsandboxed go through, so the refusals are the sandbox's.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_sandboxed_action_cannot_change_the_tree_store() {
    use kbf_driver_native::network::Isolation;

    let dir = scratch("trees-sandbox");
    let cache = cache_in(&dir);
    let (trees, _) = trees_in(&dir);
    let cas = Arc::new(MemoryCas::default());
    let sandboxed = config(&dir);
    assert!(matches!(sandboxed.isolation, Isolation::Sandbox(_)));
    let mut sandboxed = sandboxed;
    sandboxed.cache = Some(Arc::clone(&cache));
    sandboxed.trees = Some(Arc::clone(&trees));
    let mut open = treed(&dir.join("unsandboxed"), &cache, &trees);
    open.isolation = Isolation::None;
    let (sandboxed, open) = (runtime(sandboxed, &cas), runtime(open, &cas));
    let mut fill = Spec::act("exit 0");
    fill.inputs = toolchain();
    run(&sandboxed, &cas, 1, &fill).await.expect("filled");
    let tree = tree_in(&dir);
    let victim = tree.join("d0/f0");
    assert_eq!(std::fs::read(&victim).expect("file"), b"tool 0");
    let rows = [
        (
            "chmod",
            format!("chmod 600 {0} && echo x >> {0}", victim.display()),
        ),
        ("unlink", format!("rm -f {}", victim.display())),
        ("rename", format!("mv {0} {0}.moved", victim.display())),
        ("rmdir", format!("rm -rf {}", tree.display())),
    ];
    for (seq, (name, script)) in (2u64..).zip(&rows) {
        let mut attempt = Spec::sh(&format!("{script}; echo $?")).env("PATH", "/usr/bin:/bin");
        attempt.inputs = fill.inputs.clone();
        let result = run(&sandboxed, &cas, seq, &attempt).await.expect("ran");
        assert_ne!(stdout(&cas, &result).trim(), "0", "{name} went through");
        assert_eq!(std::fs::read(&victim).expect("file"), b"tool 0", "{name}");
        let mode = std::fs::metadata(&victim).expect("file").permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o555,
            "{name}"
        );
    }
    // The control: unsandboxed, a write to the tree goes through.
    let mut attempt = Spec::sh(&format!(
        "chmod 600 {0} && echo x >> {0}; echo $?",
        victim.display()
    ))
    .env("PATH", "/usr/bin:/bin");
    attempt.inputs = fill.inputs.clone();
    let result = run(&open, &cas, 20, &attempt).await.expect("ran");
    assert_eq!(
        stdout(&cas, &result).trim(),
        "0",
        "control: the write failed"
    );
}
