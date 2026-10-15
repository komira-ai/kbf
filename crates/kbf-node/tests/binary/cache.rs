//! The binary's `--cache-*` flags (`docs/design/worker-cache.md`): the container driver
//! opens the cache, a second owner and a missing budget run uncached, the native
//! driver opens it on macOS and never touches the directory on Linux, and the binary
//! answers as the driver's `__chown-dirs` helper before it parses its flags.

use super::{BIN, one_native_daemon, runs_until_sigterm_logged, tls};

/// Whether a line of `log` holds every one of `words`.
fn logged(log: &[String], words: &[&str]) -> bool {
    log.iter()
        .any(|line| words.iter().all(|w| line.contains(w)))
}

/// The flags of a container daemon with the stub `podman`, a stand-in cgroup tree and
/// a full subordinate id range, run as `name`, with `cache` added.
#[cfg(target_os = "linux")]
fn container(
    name: &str,
    scratch: &std::path::Path,
    cache: &[String],
) -> (Vec<String>, std::path::PathBuf) {
    let ids = super::id_files(&format!("{name}-ids"), "65536");
    let (_, cgroup) = super::cgroup_tree(&tls(&format!("{name}-cgroup")), "cpu memory pids");
    let mut flags = vec![
        "--driver=container".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--scratch={}", scratch.display()),
        format!("--id-files={}", ids.display()),
        "--actions-memory-max-gib=1".to_owned(),
    ];
    flags.extend(cgroup);
    flags.extend_from_slice(cache);
    let (bin, _) = super::podman_stub(&format!("{name}-bin"));
    (flags, bin)
}

/// C-16 and contract 3, at the daemon's level. Catches the container driver not opening
/// `--cache-dir`, a daemon that exits without saving its index, a cache another daemon
/// owns used or stopping the start, and `--cache-dir` without a budget taken as an
/// unbounded cache: the first daemon opens it; once the snapshot written right after
/// load is there, this test removes it, so the one found after SIGTERM is the clean
/// stop's (the next timed snapshot is 5 minutes away, the first pass's end minutes);
/// while this test holds the cache's lock, a daemon warns `off:locked` and serves;
/// without `--cache-max-bytes` it warns `off:no-budget`.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_container_node_opens_its_cache_and_runs_uncached_without_it() {
    #[cfg(target_os = "linux")]
    {
        use kbf_daemon::cache::{Cache, CacheConfig};
        let scratch = tls("cache-scratch").join("leases");
        let dir = tls("cache-dir").join("cache");
        std::fs::create_dir_all(&dir).expect("cache dir");
        let cache = [
            format!("--cache-dir={}", dir.display()),
            "--cache-max-bytes=1000000".to_owned(),
        ];
        let (flags, bin) = container("cache-open", &scratch, &cache);
        let snap = dir.join("v1/index.snap");
        let after_load = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !snap.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no snapshot after load"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::fs::remove_file(&snap).expect("remove the snapshot written after load");
        };
        let (status, log) =
            super::runs_until_sigterm_logged_then("cache-open", &flags, Some(&bin), after_load);
        assert!(status.success(), "{status}: {log:#?}");
        assert!(
            logged(&log, &["cache: ", "0 entries, link mode hardlink"]),
            "{log:#?}"
        );
        assert!(snap.exists(), "no index saved at the clean stop");
        let held = Cache::open(CacheConfig::new(dir.clone(), scratch.clone(), 1000))
            .expect("this test owns it");
        let (flags, bin) = container("cache-locked", &scratch, &cache);
        let (status, log) = runs_until_sigterm_logged("cache-locked", &flags, Some(&bin));
        assert!(status.success(), "{status}: {log:#?}");
        assert!(
            logged(&log, &["cache: off:locked", "leases run uncached"]),
            "{log:#?}"
        );
        drop(held);
        let (flags, bin) = container("cache-budget", &scratch, &cache[..1]);
        let (status, log) = runs_until_sigterm_logged("cache-budget", &flags, Some(&bin));
        assert!(status.success(), "{status}: {log:#?}");
        assert!(logged(&log, &["cache: off:no-budget"]), "{log:#?}");
    }
}

/// C-19. Catches the native driver using a cache its own actions could write (the
/// "enable the cache on Linux native" mutant): given `--cache-dir` and a budget, it
/// warns `off:native-same-uid` and never touches the directory (no lock, no layout).
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS native nodes open the cache: see the clone tests below"
)]
fn a_native_node_never_opens_its_cache_dir() {
    let dir = tls("native-cache");
    let cache = dir.join("cache");
    std::fs::create_dir_all(&cache).expect("cache dir");
    let flags = [
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--scratch={}", dir.join("leases").display()),
        format!("--cache-dir={}", cache.display()),
        "--cache-max-bytes=1000000".to_owned(),
    ];
    let one = one_native_daemon();
    let (status, log) = runs_until_sigterm_logged("native-cache", &flags, None);
    drop(one);
    assert!(status.success(), "{status}: {log:#?}");
    assert!(
        logged(&log, &["cache: off:native-same-uid", "leases run uncached"]),
        "{log:#?}"
    );
    let left: Vec<_> = std::fs::read_dir(&cache).expect("list").collect();
    assert!(
        left.is_empty(),
        "the native daemon touched its cache directory: {left:?}"
    );
}

/// C-19 on macOS. Catches the native driver not opening `--cache-dir` (every lease
/// would fetch its inputs again), a cache without a budget or inside the scratch
/// directory (which lease sweeps clean) used anyway, and a cache that is not opened in
/// clone mode: a macOS native node opens the cache and logs `link mode clone`; without
/// `--cache-max-bytes` it warns `off:no-budget`, with the cache under `--scratch` it
/// warns `off:unusable`, and in both it serves and leaves the directory without a
/// layout.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "only a macOS native node opens its cache"
)]
fn a_macos_native_node_opens_its_cache_in_clone_mode() {
    let dir = tls("native-cache-mac");
    let cache = dir.join("cache");
    let inside = dir.join("leases").join("cache");
    for path in [&cache, &inside] {
        std::fs::create_dir_all(path).expect("cache dir");
    }
    let flags = |name: &str, cache: &std::path::Path, budget: bool| {
        let mut flags = vec![
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            format!("--scratch={}", dir.join("leases").display()),
            format!("--cache-dir={}", cache.display()),
        ];
        if budget {
            flags.push("--cache-max-bytes=1000000".to_owned());
        }
        (name.to_owned(), flags)
    };
    let one = one_native_daemon();
    let (name, opened) = flags("native-cache-open", &cache, true);
    let (status, log) = runs_until_sigterm_logged(&name, &opened, None);
    assert!(status.success(), "{status}: {log:#?}");
    assert!(logged(&log, &["cache:", "link mode clone"]), "{log:#?}");
    assert!(cache.join("v1").is_dir(), "the cache has no layout");
    for (name, cache, budget, off) in [
        (
            "native-cache-nobudget",
            dir.join("nobudget"),
            false,
            "off:no-budget",
        ),
        ("native-cache-inside", inside.clone(), true, "off:unusable"),
    ] {
        std::fs::create_dir_all(&cache).expect("cache dir");
        let (name, flags) = flags(name, &cache, budget);
        let (status, log) = runs_until_sigterm_logged(&name, &flags, None);
        assert!(status.success(), "{status}: {log:#?}");
        assert!(
            logged(&log, &["cache:", off, "leases run uncached"]),
            "{log:#?}"
        );
        assert!(
            !cache.join("v1").exists(),
            "{off}: the daemon made a layout"
        );
    }
    drop(one);
}

/// The native node's tree store on macOS. Catches the store not opened (every lease
/// would clone each toolchain file), one opened without a cache to build from (clap
/// refuses `--tree-dir` without `--cache-dir`, so this is a cache that is off) or inside
/// the scratch directory (which lease sweeps clean): with a cache and a store outside
/// `--scratch` it logs the store and makes it; with the cache under `--scratch` (off) it
/// warns `trees: off:no-cache`, with the store under `--scratch` `trees: off:inside-scratch`,
/// and in both it serves and makes no store.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "only a macOS native node opens a tree store"
)]
fn a_macos_native_node_opens_its_tree_store() {
    let dir = tls("native-trees-mac");
    let cache = dir.join("cache");
    let inside = dir.join("leases").join("cache");
    for path in [&cache, &inside] {
        std::fs::create_dir_all(path).expect("cache dir");
    }
    let flags = |cache: &std::path::Path, trees: &std::path::Path| {
        vec![
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            format!("--scratch={}", dir.join("leases").display()),
            format!("--tree-dir={}", trees.display()),
            "--tree-max-bytes=1000000".to_owned(),
            format!("--cache-dir={}", cache.display()),
            "--cache-max-bytes=1000000".to_owned(),
        ]
    };
    let one = one_native_daemon();
    let store = dir.join("trees");
    let (status, log) =
        runs_until_sigterm_logged("native-trees-open", &flags(&cache, &store), None);
    assert!(status.success(), "{status}: {log:#?}");
    assert!(
        logged(&log, &["trees: ", "0 trees", "cloned whole"]),
        "{log:#?}"
    );
    assert!(store.is_dir(), "the store was not made");
    for (name, with, at, off) in [
        (
            "native-trees-nocache",
            &inside,
            dir.join("nocache-trees"),
            "off:no-cache",
        ),
        (
            "native-trees-inside",
            &cache,
            dir.join("leases").join("trees"),
            "off:inside-scratch",
        ),
    ] {
        let (status, log) = runs_until_sigterm_logged(name, &flags(with, &at), None);
        assert!(status.success(), "{status}: {log:#?}");
        assert!(logged(&log, &["trees:", off, "one by one"]), "{log:#?}");
        assert!(!at.exists(), "{off}: the daemon made the store");
    }
    drop(one);
}

/// Catches a binary that parses its flags before it answers as the container driver's
/// `__chown-dirs` helper (clap would refuse the mode and the missing `--driver`): the
/// daemon's own ids are given and the walk succeeds; arguments it cannot read exit 2.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn the_binary_runs_as_the_chown_dirs_helper_before_it_parses_its_flags() {
    let dir = tls("chown-dirs");
    std::fs::create_dir_all(dir.join("sub")).expect("dir");
    let ids = format!("{}:{}", rustix_ids().0, rustix_ids().1);
    let run = |args: &[&str]| {
        std::process::Command::new(BIN)
            .args(args)
            .output()
            .expect("spawn")
    };
    let ok = run(&["__chown-dirs", &ids, &dir.display().to_string()]);
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let bad = run(&["__chown-dirs", "kbf"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("usage"));
}

/// This process's uid and gid.
fn rustix_ids() -> (u32, u32) {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(tls("ids")).expect("a directory of ours");
    (meta.uid(), meta.gid())
}

/// Catches the `--tree-*` flags accepted without what they need (clap's `requires`):
/// a store without a budget would be unbounded, one without `--cache-dir` would have
/// no cache to link its files from, and a budget without a store does nothing; a
/// threshold of 0 would make every directory a tree. Each exits 2 naming the missing
/// flag, before any driver starts.
#[test]
fn the_tree_flags_need_their_store_budget_and_cache() {
    let run = |flags: &[&str]| {
        let mut args = vec!["--driver=native", "--cas=https://127.0.0.1:1"];
        args.extend_from_slice(flags);
        let out = std::process::Command::new(BIN)
            .args(&args)
            .output()
            .expect("run");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    for (flags, names) in [
        (&["--tree-dir=/t", "--cache-dir=/c"][..], "--tree-max-bytes"),
        (&["--tree-dir=/t", "--tree-max-bytes=9"][..], "--cache-dir"),
        (&["--tree-max-bytes=9"][..], "--tree-dir"),
        (
            &[
                "--tree-dir=/t",
                "--tree-max-bytes=9",
                "--cache-dir=/c",
                "--tree-min-files=0",
            ][..],
            "--tree-min-files",
        ),
    ] {
        let (code, stderr) = run(flags);
        assert_eq!(code, Some(2), "{flags:?}: {stderr}");
        assert!(stderr.contains(names), "{flags:?}: {stderr}");
    }
}

/// Catches the tree store not opened by the container driver (the `trees` field of the
/// cache the flags give it), and the native driver using one: the container daemon logs
/// `trees: <dir>: 0 trees` and has made the store's directory 0700; the native daemon
/// warns `trees: off:native` and never makes it.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_container_node_opens_its_tree_store_and_a_native_one_never_does() {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = tls("trees-scratch").join("leases");
        let base = tls("trees-dirs");
        let (cache, trees) = (base.join("cache"), base.join("trees"));
        std::fs::create_dir_all(&cache).expect("cache dir");
        let flags = [
            format!("--cache-dir={}", cache.display()),
            "--cache-max-bytes=1000000".to_owned(),
            format!("--tree-dir={}", trees.display()),
            "--tree-max-bytes=1000000".to_owned(),
            "--tree-min-files=7".to_owned(),
            "--tree-max-mounts=3".to_owned(),
        ];
        let (flags, bin) = container("trees-open", &scratch, &flags);
        let (status, log) = runs_until_sigterm_logged("trees-open", &flags, Some(&bin));
        assert!(status.success(), "{status}: {log:#?}");
        assert!(
            logged(&log, &["trees: ", "0 trees"]) && logged(&log, &["min_files=7 max_mounts=3"]),
            "{log:#?}"
        );
        // Searchable by others (a rootless runtime's container root), not listable.
        let mode = std::fs::metadata(&trees)
            .expect("store dir")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o711);

        let native = base.join("native-trees");
        let flags = [
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            format!("--scratch={}", base.join("native-leases").display()),
            format!("--cache-dir={}", cache.display()),
            "--cache-max-bytes=1000000".to_owned(),
            format!("--tree-dir={}", native.display()),
            "--tree-max-bytes=1000000".to_owned(),
        ];
        let one = one_native_daemon();
        let (status, log) = runs_until_sigterm_logged("trees-native", &flags, None);
        drop(one);
        assert!(status.success(), "{status}: {log:#?}");
        assert!(logged(&log, &["trees: off:native"]), "{log:#?}");
        assert!(!native.exists(), "the native daemon made the store");
    }
}
