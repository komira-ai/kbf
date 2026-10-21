//! Shared trees delivered by cloning (`Delivery::Clone`, the native driver on macOS): a
//! lease gets a tree as ONE clone of the tree's directory, its own copy-on-write copy,
//! and the files beside the trees as per-file clones of cache entries.
//!
//! On macOS the clones are the real `clonefile` (these tests then show that a write to
//! the lease's copy never reaches the store's tree). Elsewhere both clones are copies
//! that refuse an existing name, as `clonefile` does, so the placing logic (the counts,
//! the modes, the fallback, the build's check) runs on every platform; only a write
//! being kept out of the tree is then not shown, and the macOS CI job shows it.

mod support;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use kbf_daemon::cache::{Cache, LinkMode};
use kbf_daemon::cas::digest_of;
use kbf_daemon::trees::{Delivery, Inputs, TreeConfig, Trees};
use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode, SymlinkNode};
use prost::Message as _;
use support::cache_kit::{CountingCas, FakeClock, Gates, config, lease_dir};
use support::scratch;

const GIB: u64 = 1 << 30;

/// A new file at `to` with the bytes and mode of `from`; an existing name is an error.
#[cfg(not(target_os = "macos"))]
fn copying_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut source = std::fs::File::open(from)?;
    let mut clone = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    std::io::copy(&mut source, &mut clone)?;
    clone.set_permissions(source.metadata()?.permissions())
}

#[cfg(target_os = "macos")]
fn copying_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let source = std::fs::File::open(from)?;
    rustix::fs::fclonefileat(
        &source,
        rustix::fs::CWD,
        to,
        rustix::fs::CloneFlags::empty(),
    )?;
    Ok(())
}

/// What the directory clone does off macOS: the directory and all under it, as copies
/// (modes kept, symlinks kept), to a name that must not exist.
#[cfg(not(target_os = "macos"))]
fn copying_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copying_dir(&source, &target)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&source)?, &target)?;
        } else {
            copying_file(&source, &target)?;
        }
    }
    std::fs::set_permissions(to, std::fs::metadata(from)?.permissions())
}

/// The platform's directory clone: the real one on macOS.
fn dir_clone() -> Arc<kbf_daemon::trees::CloneDirFn> {
    #[cfg(target_os = "macos")]
    {
        match Delivery::platform() {
            Delivery::Clone(clone) => clone,
            Delivery::Mount => unreachable!("macOS clones"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(copying_dir)
    }
}

/// How the kit's directory clone behaves.
#[derive(Clone, Copy)]
enum Dirs {
    /// As the platform does.
    Real,
    /// Refuses with this errno, as a filesystem without `clonefile` does.
    Refuse(i32),
    /// No clone at all: the driver mounts (the Linux container driver's delivery).
    Mount,
}

/// A node's cache (clone mode), tree store (clone delivery) and CAS in a scratch
/// directory. The counters count the file clones the cache made (tree builds and leases
/// alike) and the directory clones.
struct Kit {
    dir: PathBuf,
    cache: Arc<Cache>,
    cas: Arc<CountingCas>,
    trees: Trees,
    file_clones: Arc<AtomicU64>,
    dir_clones: Arc<AtomicU64>,
    /// While set, a file clone leaves one byte too many (a tree built from it fails its
    /// check).
    corrupt: Arc<AtomicBool>,
}

fn kit(name: &str, dirs: Dirs) -> Kit {
    let dir = scratch(name);
    let (file_clones, dir_clones) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let corrupt = Arc::new(AtomicBool::new(false));
    let mut cache_config = config(&dir, GIB, Gates::running(), &FakeClock::at(1000));
    cache_config.link_mode = Some(LinkMode::Clone);
    let (counted, bad) = (Arc::clone(&file_clones), Arc::clone(&corrupt));
    cache_config.link = Arc::new(move |from: &Path, to: &Path| {
        counted.fetch_add(1, Ordering::SeqCst);
        copying_file(from, to)?;
        if bad.load(Ordering::SeqCst) {
            let was = std::fs::metadata(to)?.permissions();
            std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o600))?;
            std::fs::OpenOptions::new()
                .append(true)
                .open(to)?
                .set_len(std::fs::metadata(to)?.len() + 1)?;
            std::fs::set_permissions(to, was)?;
        }
        Ok(())
    });
    let cache = Cache::open(cache_config).expect("open the cache");
    let mut trees = TreeConfig::new(dir.join("trees"), GIB);
    let (counted, real) = (Arc::clone(&dir_clones), dir_clone());
    trees.delivery = Delivery::Clone(Arc::new(move |from: &Path, to: &Path| {
        counted.fetch_add(1, Ordering::SeqCst);
        match dirs {
            Dirs::Real => real(from, to),
            Dirs::Refuse(errno) => Err(std::io::Error::from_raw_os_error(errno)),
            Dirs::Mount => unreachable!("a mount delivery never clones"),
        }
    }));
    if matches!(dirs, Dirs::Mount) {
        trees.delivery = Delivery::Mount;
    }
    Kit {
        trees: Trees::open(trees).expect("open the store"),
        cas: Arc::new(CountingCas::default()),
        cache,
        file_clones,
        dir_clones,
        corrupt,
        dir,
    }
}

impl Kit {
    async fn try_lease(
        &self,
        root: &Digest,
        name: &str,
    ) -> Result<(PathBuf, Inputs), kbf_daemon::tree::TreeError> {
        let dir = lease_dir(&self.dir, name);
        let inputs = self
            .trees
            .materialize(&*self.cas, &self.cache, root, &dir, &[], "")
            .await?;
        Ok((dir, inputs))
    }

    async fn lease(&self, root: &Digest, name: &str) -> (PathBuf, Inputs) {
        self.try_lease(root, name).await.expect("materialize")
    }

    fn files(&self) -> u64 {
        self.file_clones.load(Ordering::SeqCst)
    }

    fn dirs(&self) -> u64 {
        self.dir_clones.load(Ordering::SeqCst)
    }
}

/// `count` files under `prefix`, in a few subdirectories (one of them an empty file, and
/// one in 20 executable); `tag` makes their bytes (and so the tree's digest) its own.
fn files(prefix: &str, count: usize, tag: &str) -> Vec<(String, Vec<u8>, bool)> {
    (0..count)
        .map(|i| {
            let bytes = if i == 3 {
                Vec::new()
            } else {
                format!("{tag} file {i}").into_bytes()
            };
            (format!("{prefix}/d{}/f{i}", i % 7), bytes, i % 20 == 0)
        })
        .collect()
}

/// Five small files beside the tree.
fn small() -> Vec<(String, Vec<u8>, bool)> {
    ["src/a.c", "src/b.c", "BUILD", "pkg/x.h", "pkg/y.h"]
        .iter()
        .map(|p| ((*p).to_owned(), format!("small {p}").into_bytes(), false))
        .collect()
}

/// A root of a 300-file toolchain at `tc` and the small files; returns it.
fn input(kit: &Kit, tag: &str) -> Digest {
    let mut all = files("tc", 300, tag);
    all.extend(small());
    kit.cas.tree(&all)
}

/// The digest of the directory tree at `path` computed from its bytes: the Directory
/// message it would be (symlinks, executable bits and contents included).
fn digest_of_dir(path: &Path) -> Digest {
    let mut names: Vec<_> = std::fs::read_dir(path)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    names.sort();
    let mut directory = Directory::default();
    for name in names {
        let at = path.join(&name);
        let name = name.to_string_lossy().into_owned();
        let meta = std::fs::symlink_metadata(&at).expect("meta");
        if meta.file_type().is_symlink() {
            directory.symlinks.push(SymlinkNode {
                name,
                target: std::fs::read_link(&at)
                    .expect("link")
                    .to_string_lossy()
                    .into_owned(),
                ..SymlinkNode::default()
            });
        } else if meta.is_dir() {
            directory.directories.push(DirectoryNode {
                name,
                digest: Some(digest_of_dir(&at)),
            });
        } else {
            directory.files.push(FileNode {
                name,
                digest: Some(digest_of(&std::fs::read(&at).expect("read"))),
                is_executable: meta.mode() & 0o111 != 0,
                ..FileNode::default()
            });
        }
    }
    digest_of(&directory.encode_to_vec())
}

/// The store's one tree directory.
fn the_tree(kit: &Kit) -> PathBuf {
    let mut trees: Vec<PathBuf> = std::fs::read_dir(kit.dir.join("trees"))
        .expect("store")
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    assert_eq!(trees.len(), 1, "{trees:?}");
    trees.remove(0)
}

/// Every regular file under `dir`.
fn regular_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(here) = stack.pop() {
        for entry in std::fs::read_dir(&here).expect("read_dir") {
            let entry = entry.expect("entry");
            let kind = entry.file_type().expect("type");
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                found.push(entry.path());
            }
        }
    }
    found
}

async fn wait_for(mut done: impl FnMut() -> bool) {
    for _ in 0..400 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("not done in 10 s");
}

fn hidden(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default()
}

/// Catches a lease that still clones a tree's files one by one (the per-file path with
/// the tree built beside it), a tree cloned more than once per lease (once per
/// directory level, or again for the second lease), a tree built again for the second
/// lease, a mount returned for a driver that has none to make, and a lease root that is
/// not the input root. Two leases of a root with a 300-file tree and five small files:
/// each makes exactly one directory clone and clones only the five small files
/// (the tree's 300 were cloned once, building it), the tree is built once, nothing is
/// mounted or left pinned, and each lease's directory is the Directory it was asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_is_one_directory_clone_per_lease_and_no_file_clones() {
    let kit = kit("trees-clone-one", Dirs::Real);
    let root = input(&kit, "one");
    let (first, one) = kit.lease(&root, "lease-1").await;
    // 300 tree files (299 distinct blobs plus the empty file, which is created, not
    // cloned: 299 clones) and the 5 small files.
    assert_eq!((one.trees_built, one.trees_cloned, kit.dirs()), (1, 1, 1));
    assert!(one.mounts.is_empty() && one.trees.is_empty());
    assert_eq!(one.tree_files, 300);
    assert_eq!(kit.files(), 299 + 5);
    assert_eq!(one.stats.files, 5, "only the files outside the tree");
    assert_eq!(digest_of_dir(&first), root);
    assert_eq!(kit.trees.store().stats().pinned, 0, "a clone needs no pin");

    let (second, two) = kit.lease(&root, "lease-2").await;
    assert_eq!((two.trees_built, two.trees_cloned), (0, 1));
    assert_eq!(kit.dirs(), 2, "one directory clone per lease");
    assert_eq!(
        kit.files(),
        299 + 5 + 5,
        "the second lease cloned its 5 small files"
    );
    assert_eq!(two.stats.files, 5);
    assert_eq!(digest_of_dir(&second), root);
    let stats = kit.trees.store().stats();
    assert_eq!((stats.builds, stats.hits, stats.trees), (1, 1, 1));
}

/// Catches a lease whose copy of the tree is read-only or a mount's empty directory, and
/// the lease's modes differing from the per-file path (an input is 0644, 0755 executable;
/// a tool that rewrites an input in place must work): every file in the lease's tree is
/// owner-writable, executables are 0755 and the rest 0644, every directory is
/// searchable and writable, and the empty file too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leases_copy_of_a_tree_is_moded_as_inputs_are() {
    let kit = kit("trees-clone-modes", Dirs::Real);
    let root = input(&kit, "modes");
    let (dir, _) = kit.lease(&root, "lease-1").await;
    let mut seen = 0;
    for file in regular_files(&dir.join("tc")) {
        let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o7777;
        assert!(
            mode == 0o644 || mode == 0o755,
            "{}: {mode:o}",
            file.display()
        );
        seen += 1;
    }
    assert_eq!(seen, 300);
    let executable = dir.join("tc/d0/f0");
    assert_eq!(
        std::fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let empty = dir.join("tc/d3/f3");
    assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0);
    for sub in ["tc", "tc/d0"] {
        let mode = std::fs::metadata(dir.join(sub))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o700, 0o700, "{sub}: {mode:o}");
    }
}

/// Catches a tree in the store that has a write bit (the file clones left writable as
/// leases' are, or an empty file created 0644): it must stay as the cache's own entries
/// are, 0444 and 0555, so that nothing but the filesystem's copy-on-write stands between
/// a lease and the tree. Every file in the store's tree has no write bit, the executable
/// ones are 0555 and the others 0444.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_trees_files_in_the_store_have_no_write_bit() {
    let kit = kit("trees-clone-store-modes", Dirs::Real);
    let root = input(&kit, "store");
    let _ = kit.lease(&root, "lease-1").await;
    let tree = the_tree(&kit);
    let files = regular_files(&tree);
    assert_eq!(files.len(), 300);
    for file in files {
        let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o7777;
        assert!(
            mode == 0o444 || mode == 0o555,
            "{}: {mode:o}",
            file.display()
        );
    }
    assert_eq!(
        digest_of_dir(&tree),
        digest_of_dir(&kit.dir.join("leases/lease-1/tc"))
    );
}

/// Catches the action's changes reaching the tree or the next lease (on macOS: a
/// directory "clone" that is a link or a rename, or one that shares inodes): in one
/// lease's copy of the tree, write into a file, delete a file, rename a directory, make
/// a new file and chmod; the store's tree is the digest it was built as, and the next
/// lease's copy is the input root again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writing_in_a_leases_copy_leaves_the_tree_and_the_next_lease_intact() {
    let kit = kit("trees-clone-writes", Dirs::Real);
    let root = input(&kit, "writes");
    let (one, _) = kit.lease(&root, "lease-1").await;
    let tc = one.join("tc");
    std::fs::OpenOptions::new()
        .append(true)
        .open(tc.join("d1/f1"))
        .expect("an input is writable")
        .set_len(2)
        .expect("write");
    std::fs::write(tc.join("d2/f2"), b"rewritten").expect("rewrite");
    std::fs::remove_file(tc.join("d4/f4")).expect("delete");
    std::fs::rename(tc.join("d5"), tc.join("d5-moved")).expect("rename a directory");
    std::fs::write(tc.join("d6/new"), b"new").expect("new file");
    std::fs::set_permissions(tc.join("d0/f0"), std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_ne!(digest_of_dir(&one), root);
    let tree = the_tree(&kit);
    let (_, two) = kit.lease(&root, "lease-2").await;
    assert_eq!(two.trees_built, 0, "the tree is the one built before");
    assert_eq!(digest_of_dir(&kit.dir.join("leases/lease-2")), root);
    // The tree, as built: its files are what the Directory messages name.
    let again = digest_of_dir(&tree);
    assert_eq!(again, digest_of_dir(&kit.dir.join("leases/lease-2/tc")));
    assert_eq!(kit.trees.store().stats().builds, 1);
}

/// Catches a tree that fails its check being named in the store or left behind (a bad
/// tree handed to every later lease): a file clone of the build that has one byte too
/// many fails the lease with the tree's check, the store holds nothing (no tree, no
/// `.tmp-*`), no directory clone was attempted, and the next lease builds a good tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_that_fails_its_check_is_discarded() {
    let kit = kit("trees-clone-bad", Dirs::Real);
    let root = input(&kit, "bad");
    kit.corrupt.store(true, Ordering::SeqCst);
    let failed = kit
        .try_lease(&root, "lease-1")
        .await
        .expect_err("a bad tree");
    let shown = failed.to_string();
    assert!(
        shown.contains("is not the file its Directory names"),
        "{shown}"
    );
    kit.corrupt.store(false, Ordering::SeqCst);
    wait_for(|| hidden(&kit.dir.join("trees")).is_empty()).await;
    assert_eq!(kit.trees.store().stats().trees, 0);
    assert!(regular_files(&kit.dir.join("trees")).is_empty());
    assert_eq!(kit.dirs(), 0);
    // The entries the bad clones came from are the cache's, and good: a new lease builds.
    let (dir, inputs) = kit.lease(&root, "lease-2").await;
    assert_eq!((inputs.trees_built, inputs.trees_cloned), (1, 1));
    assert_eq!(digest_of_dir(&dir), root);
}

/// Catches a refused directory clone failing the lease or leaving a hole (the empty
/// mount point where the tree should be), and a fallback that does not write the tree's
/// files as inputs: with the filesystem refusing `ENOTSUP`, a lease still has the whole
/// input root (its digest is the root's), every tree file is writable as an input is,
/// none is counted as cloned, the directory clone was tried once per lease, and the
/// tree is not rebuilt. `EXDEV` is the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_directory_clone_writes_the_trees_files_one_by_one() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    for (name, errno) in [("enotsup", libc::ENOTSUP), ("exdev", libc::EXDEV)] {
        let kit = kit(&format!("trees-clone-refused-{name}"), Dirs::Refuse(errno));
        let root = input(&kit, name);
        let (one, first) = kit.lease(&root, "lease-1").await;
        assert_eq!((first.trees_built, first.trees_cloned), (1, 0), "{name}");
        assert_eq!(digest_of_dir(&one), root, "{name}");
        assert_eq!(
            first.stats.files,
            5 + 300,
            "{name}: the tree's files are written too"
        );
        for file in regular_files(&one.join("tc")) {
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
            assert!(
                mode == 0o644 || mode == 0o755,
                "{name}: {}: {mode:o}",
                file.display()
            );
        }
        let (two, second) = kit.lease(&root, "lease-2").await;
        assert_eq!((second.trees_built, kit.dirs()), (0, 2), "{name}");
        assert_eq!(digest_of_dir(&two), root, "{name}");
    }
}

/// Catches an error that is not a refusal being swallowed into the per-file path (a
/// lease that quietly loses its tree's speed, or worse, half a tree): a directory clone
/// that fails `EACCES` fails the lease, naming the path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_clone_that_fails_otherwise_fails_the_lease() {
    let kit = kit("trees-clone-fails", Dirs::Refuse(libc::EACCES));
    let root = input(&kit, "fails");
    let failed = kit.try_lease(&root, "lease-1").await.expect_err("EACCES");
    let shown = failed.to_string();
    assert!(shown.contains("lease-1/tc"), "{shown}");
}

/// Catches a tree store that evicts a tree a lease's clone still needs, or a clone that
/// holds a pin forever: after the lease is cloned its tree is unpinned, so a store too
/// small for a second tree evicts the first, and the first lease's copy is whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clone_does_not_pin_its_tree_and_outlives_its_eviction() {
    let dir = scratch("trees-clone-evict");
    let mut cache_config = config(&dir, GIB, Gates::running(), &FakeClock::at(1000));
    cache_config.link_mode = Some(LinkMode::Clone);
    cache_config.link = Arc::new(copying_file);
    let cache = Cache::open(cache_config).expect("cache");
    let mut tree_config = TreeConfig::new(dir.join("trees"), 4000);
    tree_config.delivery = Delivery::Clone(dir_clone());
    let trees = Trees::open(tree_config).expect("store");
    let cas = CountingCas::default();
    let (a, b) = (
        cas.tree(&files("tc", 300, "a")),
        cas.tree(&files("tc", 300, "b")),
    );
    for (n, root) in [&a, &b].into_iter().enumerate() {
        let to = lease_dir(&dir, &format!("lease-{n}"));
        trees
            .materialize(&cas, &cache, root, &to, &[], "")
            .await
            .expect("lease");
    }
    let stats = trees.store().stats();
    assert_eq!((stats.trees, stats.pinned, stats.evictions), (1, 0, 1));
    assert_eq!(digest_of_dir(&dir.join("leases/lease-0")), a);
    assert_eq!(digest_of_dir(&dir.join("leases/lease-1")), b);
}

/// Catches the delivery's `Debug` (the store's own is derived through it) naming the
/// wrong arm, and the platform's default not being the one the driver relies on: a
/// mount on Linux, a clone on macOS.
#[test]
fn the_delivery_names_itself_and_defaults_by_platform() {
    assert_eq!(format!("{:?}", Delivery::Mount), "Mount");
    assert_eq!(format!("{:?}", Delivery::Clone(dir_clone())), "Clone");
    let want = if cfg!(target_os = "macos") {
        "Clone"
    } else {
        "Mount"
    };
    assert_eq!(format!("{:?}", Delivery::platform()), want);
}

/// A root whose tree `tool` holds 300 unique files, two files of one blob and three
/// symlinks, and a file `beside` of that same blob outside it.
fn linked(kit: &Kit) -> Digest {
    let cas = &kit.cas.blobs;
    let blob = cas.insert(&b"same bytes"[..]);
    let mut tool = Directory::default();
    for i in 0..300 {
        tool.files.push(FileNode {
            name: format!("f{i:03}"),
            digest: Some(cas.insert(format!("unique {i}").into_bytes())),
            ..FileNode::default()
        });
    }
    for name in ["same1", "same2"] {
        tool.files.push(FileNode {
            name: name.to_owned(),
            digest: Some(blob.clone()),
            ..FileNode::default()
        });
    }
    tool.files.sort_by(|a, b| a.name.cmp(&b.name));
    tool.symlinks = [
        ("abs", "/etc/hostname"),
        ("rel", "f000"),
        ("up", "../beside"),
    ]
    .iter()
    .map(|(name, target)| SymlinkNode {
        name: (*name).to_owned(),
        target: (*target).to_owned(),
        ..SymlinkNode::default()
    })
    .collect();
    let tool = cas.message(&tool);
    cas.message(&Directory {
        files: vec![FileNode {
            name: "beside".to_owned(),
            digest: Some(blob),
            ..FileNode::default()
        }],
        directories: vec![DirectoryNode {
            name: "tool".to_owned(),
            digest: Some(tool),
        }],
        ..Directory::default()
    })
}

/// Catches a symlink in a tree turned into a file or followed (the mode walk chmodding
/// what a link points at, outside the lease), and a blob the tree shares with a file
/// beside it lost or written twice when the tree falls back to files: with the directory
/// clone working and refused, the lease's directory is the root's digest (symlinks kept,
/// every name present), and the links' targets are untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn symlinks_stay_links_and_a_shared_blob_is_written_at_every_name() {
    for (name, dirs) in [
        ("real", Dirs::Real),
        ("refused", Dirs::Refuse(libc::ENOTSUP)),
    ] {
        let kit = kit(&format!("trees-clone-links-{name}"), dirs);
        let root = linked(&kit);
        let (dir, inputs) = kit.lease(&root, "lease-1").await;
        assert_eq!(
            inputs.trees_cloned,
            u64::from(matches!(dirs, Dirs::Real)),
            "{name}"
        );
        assert_eq!(digest_of_dir(&dir), root, "{name}");
        let link = dir.join("tool/abs");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(dir.join("tool/same2")).unwrap(),
            b"same bytes"
        );
        assert_eq!(std::fs::read(dir.join("beside")).unwrap(), b"same bytes");
    }
}

/// Catches clone delivery leaking into the mount delivery (the container driver's): with
/// `Delivery::Mount` a lease's tree is a mount for the driver, pinned, with an empty
/// directory at its path, and no clone is made. Kept in this binary with the clone tests:
/// the daemon's coverage counts the best single copy of `materialize`, which must run
/// both deliveries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mount_delivery_hands_the_tree_to_the_driver_and_clones_nothing() {
    let kit = kit("trees-clone-mount", Dirs::Mount);
    let root = input(&kit, "mount");
    let (dir, inputs) = kit.lease(&root, "lease-1").await;
    assert_eq!((inputs.trees_cloned, kit.dirs()), (0, 0));
    assert_eq!(inputs.mounts.len(), 1);
    assert_eq!(inputs.mounts[0].path, "tc");
    assert_eq!(inputs.mounts[0].source, the_tree(&kit));
    assert_eq!(inputs.trees.len(), 1, "the tree is pinned for the driver");
    assert!(std::fs::read_dir(dir.join("tc")).unwrap().next().is_none());
    assert_eq!(kit.trees.store().stats().pinned, 1);
}
