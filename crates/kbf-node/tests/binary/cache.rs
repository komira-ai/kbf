//! The binary's `--cache-*` flags (`docs/design/worker-cache.md`): the container driver
//! opens the cache, a second owner and a missing budget run uncached, the native
//! driver never touches the directory, and the binary answers as the driver's
//! `__chown-dirs` helper before it parses its flags.

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
