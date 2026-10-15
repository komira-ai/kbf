//! The node's input cache (`docs/design/worker-cache.md`): its flags, and the cache the
//! container driver links leases' input files from.
//!
//! No `--cache-dir`, no cache: the default. Given, the container driver opens it at
//! start (the cache's lock first; a cache another daemon owns is refused) and links
//! every lease's input files from it. A cache that cannot be used never stops the
//! daemon: it warns with the reason (`cache: off:<reason>`) and runs uncached. The
//! native driver never uses one: its actions run as the daemon's user, who owns the
//! cache and could write it.

use std::path::PathBuf;

/// The `--cache-*` flags.
#[derive(Clone, Debug, clap::Args)]
pub(crate) struct CacheArgs {
    /// The directory of the node's input cache (container), which must exist and
    /// should be on the scratch directory's filesystem (otherwise leases get copies,
    /// not links). Needs `--cache-max-bytes`. A cache another daemon owns, or one that
    /// cannot be used, is left alone: the daemon warns and runs uncached.
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,
    /// The most bytes the cache keeps; least recently used entries are evicted past it.
    #[arg(long, value_name = "BYTES")]
    pub cache_max_bytes: Option<u64>,
    /// Below this many free bytes on the cache's filesystem, entries no running lease
    /// links are evicted. 0, the default, sets no floor.
    #[arg(long, value_name = "BYTES", default_value_t = 0)]
    pub cache_min_free_bytes: u64,
    /// How many of a lease's input files are linked, or filled from the CAS, at once.
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(1..))]
    pub cache_link_concurrency: u64,
}

/// The defaults of `--tree-min-files` and `--tree-max-mounts`; `kbf_daemon::trees` has
/// the same two (it does not build on macOS, where these flags are parsed and ignored).
const TREE_MIN_FILES: u64 = 256;
const TREE_MAX_MOUNTS: usize = 8;

/// The `--tree-*` flags: the container driver's shared read-only trees
/// (`docs/design/fast-sandbox.md#4-shared-trees`).
#[derive(Clone, Debug, clap::Args)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct TreeArgs {
    /// The directory of the node's tree store (container), made if missing; it should
    /// be on the cache's filesystem. Needs `--cache-dir` and `--tree-max-bytes`. A
    /// Directory node of an action's input root with at least `--tree-min-files` files,
    /// no output path or working directory in it, is built here once and mounted
    /// read-only into each action that has it, instead of linking its files per action.
    /// A write into such a directory fails (EROFS), where it used to succeed.
    #[arg(long, value_name = "DIR", requires_all = ["tree_max_bytes", "cache_dir"])]
    pub tree_dir: Option<PathBuf>,
    /// The most file bytes of trees kept; least recently used unpinned trees are
    /// evicted past it. A node larger than this is never a tree.
    #[arg(long, value_name = "BYTES", requires = "tree_dir")]
    pub tree_max_bytes: Option<u64>,
    /// The fewest files and symlinks a Directory node holds to be a tree.
    #[arg(long, default_value_t = TREE_MIN_FILES, value_parser = clap::value_parser!(u64).range(1..))]
    pub tree_min_files: u64,
    /// The most trees one action mounts; the rest of its files are linked.
    #[arg(long, default_value_t = TREE_MAX_MOUNTS, value_parser = clap::value_parser!(usize))]
    pub tree_max_mounts: usize,
}

/// What the native driver logs when it is given `--cache-dir`: it never uses one.
pub(crate) fn native_warning(args: &CacheArgs) -> Option<String> {
    args.cache_dir.as_ref().map(|dir| {
        format!(
            "cache: off:native-same-uid ({}): the native driver's actions run as the \
             daemon's user and could write the cache; leases run uncached",
            dir.display()
        )
    })
}

/// The container driver's cache as the flags give it, for the driver to open once
/// Podman's storage passed its checks; `None`, after a warning, when there is none or
/// it cannot be used.
#[cfg(target_os = "linux")]
pub(crate) fn given(
    args: &CacheArgs,
    trees: &TreeArgs,
    scratch: &std::path::Path,
) -> Option<kbf_driver_container::CacheUse> {
    use kbf_daemon::cache::{CacheConfig, Disabled};
    let dir = args.cache_dir.clone()?;
    let shown = dir.display().to_string();
    let link_concurrency = usize::try_from(args.cache_link_concurrency).unwrap_or(usize::MAX);
    let config = (args.cache_max_bytes.ok_or(Disabled::NoBudget)).map(|max_bytes| {
        let mut config = CacheConfig::new(dir, scratch.to_owned(), max_bytes);
        config.min_free_bytes = args.cache_min_free_bytes;
        config.link_concurrency = link_concurrency;
        config
    });
    // The helper is this binary, by the path it was started from: after an update
    // renames a new binary over it, that path names the new one.
    let helper = std::env::current_exe()
        .ok()
        .ok_or(Disabled::Unusable("the daemon's own path".to_owned()));
    match config.and_then(|config| helper.map(|helper| (config, helper))) {
        Ok((config, helper)) => Some(kbf_driver_container::CacheUse {
            config,
            helper,
            trees: tree_config(trees),
        }),
        Err(why) => {
            let reason = why.reason();
            tracing::warn!("cache: off:{reason} ({shown}: {why}); leases run uncached");
            None
        }
    }
}

/// The tree store the `--tree-*` flags ask for, if they do (clap checks that
/// `--tree-dir` comes with `--tree-max-bytes` and `--cache-dir`).
#[cfg(target_os = "linux")]
fn tree_config(args: &TreeArgs) -> Option<kbf_daemon::trees::TreeConfig> {
    let dir = args.tree_dir.clone()?;
    let mut config = kbf_daemon::trees::TreeConfig::new(dir, args.tree_max_bytes?);
    config.min_files = args.tree_min_files;
    config.max_mounts = args.tree_max_mounts;
    Some(config)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// Catches the flags' defaults drifting from the daemon's.
    #[test]
    fn the_tree_flag_defaults_are_the_daemons() {
        assert_eq!(super::TREE_MIN_FILES, kbf_daemon::trees::MIN_FILES);
        assert_eq!(super::TREE_MAX_MOUNTS, kbf_daemon::trees::MAX_MOUNTS);
    }
}
