//! The platform-neutral probes (`kbf_testkit::probe`), over a fake [`NodeView`] on a temporary
//! directory. Each probe is shown to return `Ok` on a clean tree and `Err` on the leak it
//! exists for; a probe that could not fail would let a leaking product pass the black-box
//! suites (`kbf-daemon-test`).

use std::collections::BTreeSet;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use kbf_testkit::canary::{canary, noise};
use kbf_testkit::dir::TestDir;
use kbf_testkit::node_view::{Isolation, NodeView, Proc};
use kbf_testkit::probe::{
    self, Kind, LEASE_PREFIX, Snapshot, bytes_below, canary_outside_digests, clean_since,
    clean_since_within, files_containing, paths_named, read_dir, scan, walk, walk_pruned,
    world_writable,
};

fn dir(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn put(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn names(map: &std::collections::BTreeMap<PathBuf, probe::Entry>) -> Vec<String> {
    map.keys().map(|p| p.display().to_string()).collect()
}

/// Catches: a canary that repeats across seeds (two actions' canaries would match each
/// other), is not the length asked for, or changes between runs (a failing run could not be
/// repeated); and the same for the binary noise.
#[test]
fn canary_and_noise_are_exact_in_length_and_differ_by_seed() {
    for n in [1, 15, 16, 17, 4096] {
        let c = canary(7, n);
        assert_eq!(c.len(), n);
        assert!(c.bytes().all(|b| b.is_ascii_hexdigit()), "{c}");
        assert_eq!(noise(7, n).len(), n);
    }
    assert_eq!(canary(7, 64), canary(7, 64));
    assert_ne!(canary(7, 64), canary(8, 64));
    assert_eq!(noise(7, 64), noise(7, 64));
    assert_ne!(noise(7, 64), noise(8, 64));
}

/// Catches: a walk that follows a link out of the tree, misses a kind, lists a pruned
/// directory's contents, reports a missing root or a file as a tree, or skips what it
/// cannot read (a mode-000 directory is an error, never skipped).
#[test]
fn a_walk_lists_every_path_by_kind_and_fails_on_what_it_cannot_read() {
    let tmp = dir("walk");
    let root = tmp.path();
    put(&root.join("a/file"), "x");
    put(&root.join("pruned/inside"), "x");
    symlink("a/file", root.join("link")).expect("link");
    let fifo = std::ffi::CString::new(root.join("fifo").to_str().expect("utf8")).expect("c");
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let all = walk(root).expect("a walk");
    assert_eq!(
        names(&all),
        ["a", "a/file", "fifo", "link", "pruned", "pruned/inside"]
    );
    assert_eq!(all[Path::new("a")].kind, Kind::Dir);
    assert!(all[Path::new("a")].is_dir());
    assert_eq!(all[Path::new("a/file")].kind, Kind::File);
    assert_eq!(all[Path::new("a/file")].len, 1);
    assert_eq!(all[Path::new("link")].kind, Kind::Link);
    assert_eq!(all[Path::new("fifo")].kind, Kind::Other);
    assert_eq!(all[Path::new("fifo")].mode, 0o600);

    let pruned = walk_pruned(root, &|abs| abs.ends_with("pruned")).expect("a walk");
    assert_eq!(names(&pruned), ["a", "a/file", "fifo", "link", "pruned"]);

    // Paths that go while the tree is being walked are gone, not an error. Whenever it is
    // asked about a directory, the prune closure removes twenty files and a directory the
    // walk may have yet to reach (a file listed after the directory it is asked about, and a
    // directory already waiting to be entered).
    for n in 0..20 {
        put(&root.join(format!("late{n:02}")), "x");
    }
    put(&root.join("gone/inside"), "x");
    let raced = walk_pruned(root, &|_| {
        for n in 0..20 {
            let _ = std::fs::remove_file(root.join(format!("late{n:02}")));
        }
        let _ = std::fs::remove_dir_all(root.join("gone"));
        false
    })
    .expect("a walk of a tree that changes");
    assert!(names(&raced).contains(&"a/file".to_owned()), "{raced:?}");

    let missing = walk(&root.join("nope")).expect_err("missing");
    assert!(missing.contains("nope"), "{missing}");
    let file = walk(&root.join("a/file")).expect_err("not a directory");
    assert!(file.contains("not a directory"), "{file}");
    // A name below a file is not "gone": it is an error.
    let below = walk(&root.join("a/file/x")).expect_err("not a directory");
    assert!(below.contains("a/file/x"), "{below}");
    let listing = read_dir(&root.join("nope")).expect_err("missing");
    assert!(listing.contains("nope"), "{listing}");
    assert_eq!(read_dir(root).expect("a listing").len(), 4);

    std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o000))
        .expect("chmod");
    let readable = std::fs::read_dir(root.join("a")).is_ok();
    let denied = walk(root);
    std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    if !readable {
        let error = denied.expect_err("an unreadable directory is an error");
        assert!(error.contains("/a"), "{error}");
    }
}

/// Catches: a byte count that counts a hard link twice, ignores a directory's files, or
/// passes a missing path.
#[test]
fn bytes_below_counts_each_inode_once() {
    let tmp = dir("bytes");
    let root = tmp.path();
    put(&root.join("one"), &"x".repeat(5000));
    std::fs::hard_link(root.join("one"), root.join("two")).expect("link");
    let (alloc, apparent) = bytes_below(root).expect("bytes");
    let (_, file_apparent) = bytes_below(&root.join("one")).expect("a file");
    assert_eq!(file_apparent, 5000);
    assert!((5000..10_000).contains(&apparent), "{apparent}");
    assert!(alloc >= 4096, "{alloc}");
    assert!(bytes_below(&root.join("nope")).is_err());
}

/// Catches: a world-writable probe that misses `o+w` on the root, on a file or on a
/// directory, flags a link (whose mode is always 0777), or passes a missing root.
#[test]
fn world_writable_names_every_path_anyone_may_write() {
    let tmp = dir("ww");
    let root = tmp.path();
    put(&root.join("safe"), "x");
    put(&root.join("open/file"), "x");
    symlink("safe", root.join("link")).expect("link");
    let mode = |p: &Path, m| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).expect("chmod")
    };
    mode(root, 0o755);
    mode(&root.join("open"), 0o777);
    mode(&root.join("open/file"), 0o666);
    assert_eq!(
        world_writable(root).expect("a walk"),
        [root.join("open"), root.join("open/file")]
    );
    mode(root, 0o757);
    assert_eq!(world_writable(root).expect("a walk")[0], root);
    mode(root, 0o755);
    assert!(
        world_writable(&root.join("nope"))
            .expect_err("missing")
            .contains("nope")
    );
}

/// Catches: a name search that matches part of a name, or stops at the first directory.
#[test]
fn paths_named_finds_exact_names_at_any_depth() {
    let tmp = dir("named");
    let root = tmp.path();
    put(&root.join("a/needle"), "x");
    put(&root.join("b/c/needle"), "x");
    put(&root.join("b/needles"), "x");
    let mut found = paths_named(root, "needle").expect("a walk");
    found.sort();
    assert_eq!(found, [root.join("a/needle"), root.join("b/c/needle")]);
}

/// Catches: a scan that misses a file holding the needle, hits a file too short to hold
/// it or one that does not, enters a pruned path, or skips a file it cannot read.
#[test]
fn a_scan_finds_the_needle_in_every_readable_file_outside_the_pruned_paths() {
    let tmp = dir("scan");
    let root = tmp.path();
    put(&root.join("hit"), "before NEEDLE after");
    put(&root.join("deep/hit"), "NEEDLE");
    put(&root.join("short"), "NEED");
    put(&root.join("other"), "nothing here at all");
    put(&root.join("skip/hit"), "NEEDLE");
    put(&root.join("locked"), "NEEDLE");
    put(&root.join("pruned-file"), "NEEDLE");
    put(&root.join("vanish"), "NEEDLE");
    let no_prune = |_: &Path| false;
    // `vanish` goes between the walk and the read: gone, not an error.
    let mut hits = scan(&[root.to_owned()], b"NEEDLE", &|abs| {
        if abs.ends_with("vanish") {
            std::fs::remove_file(abs).expect("remove");
        }
        abs.ends_with("skip") || abs.ends_with("pruned-file")
    })
    .expect("a scan");
    hits.sort();
    assert_eq!(
        hits,
        [root.join("deep/hit"), root.join("hit"), root.join("locked")]
    );
    assert_eq!(
        scan(&[root.join("deep")], b"NEEDLE", &no_prune)
            .expect("a scan")
            .len(),
        1
    );

    std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
        .expect("chmod");
    let readable = std::fs::read(root.join("locked")).is_ok();
    let locked = scan(&[root.to_owned()], b"NEEDLE", &no_prune);
    std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o644))
        .expect("chmod");
    if !readable {
        assert!(locked.expect_err("unreadable").contains("locked"));
    }
}

/// A node on a directory: `data` is its scratch directory (`cache` below it is kept),
/// `hot` a content store, `helper-state` a directory that must stay empty, `cg` its
/// cgroups, and `kbf.toml` and `etc` are what the node has by design.
struct Fake {
    root: PathBuf,
    procs: Mutex<Vec<BTreeSet<Proc>>>,
    users: Mutex<BTreeSet<String>>,
    sessions: Mutex<BTreeSet<String>>,
    homes: Mutex<BTreeSet<String>>,
    /// The look (counting from 1) at which `processes` fails; 0 for none.
    fail_look: usize,
    looks: Mutex<usize>,
}

impl Fake {
    fn new(root: &Path) -> Self {
        for sub in [
            "data/leases",
            "data/cache",
            "hot",
            "helper-state",
            "cg",
            "etc",
        ] {
            std::fs::create_dir_all(root.join(sub)).expect("mkdir");
        }
        put(&root.join("kbf.toml"), "x");
        Self {
            root: root.to_owned(),
            procs: Mutex::new(Vec::new()),
            users: Mutex::default(),
            sessions: Mutex::default(),
            homes: Mutex::default(),
            fail_look: 0,
            looks: Mutex::default(),
        }
    }

    fn at(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The processes the next looks return, one set per look; the last repeats.
    fn show(&self, looks: Vec<Vec<u32>>) {
        *self.procs.lock().expect("lock") = looks
            .into_iter()
            .map(|pids| {
                pids.into_iter()
                    .map(|pid| Proc {
                        pid,
                        state: 'S',
                        cmd: format!("cmd{pid}"),
                    })
                    .collect()
            })
            .collect();
    }

    fn names_in(&self, rel: &str, keep: impl Fn(&str) -> bool) -> Result<BTreeSet<String>, String> {
        Ok(read_dir(&self.at(rel))?
            .into_iter()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| keep(n))
            .collect())
    }
}

impl NodeView for Fake {
    fn scratch(&self) -> PathBuf {
        self.at("data")
    }
    fn isolation(&self) -> Isolation {
        Isolation::Nothing
    }
    fn lease_dirs(&self) -> Result<BTreeSet<String>, String> {
        self.names_in("data/leases", |_| true)
    }
    fn lease_cgroups(&self) -> Result<BTreeSet<String>, String> {
        self.names_in("cg", |n| n.starts_with(LEASE_PREFIX))
    }
    fn processes(&self) -> Result<BTreeSet<Proc>, String> {
        let look = {
            let mut looks = self.looks.lock().expect("lock");
            *looks += 1;
            *looks
        };
        if look == self.fail_look {
            return Err("no /proc".to_owned());
        }
        let mut looks = self.procs.lock().expect("lock");
        Ok(if looks.len() > 1 {
            looks.remove(0)
        } else {
            looks.first().cloned().unwrap_or_default()
        })
    }
    fn slot_users(&self) -> Result<BTreeSet<String>, String> {
        Ok(self.users.lock().expect("lock").clone())
    }
    fn session_leftovers(&self) -> Result<BTreeSet<String>, String> {
        Ok(self.sessions.lock().expect("lock").clone())
    }
    fn home_dirs(&self) -> Result<BTreeSet<String>, String> {
        Ok(self.homes.lock().expect("lock").clone())
    }
    fn kept(&self, rel: &Path) -> bool {
        rel.starts_with("cache")
    }
    fn stores(&self) -> Vec<PathBuf> {
        vec![self.at("hot"), self.at("data/cache")]
    }
    fn empty_between_leases(&self) -> Vec<PathBuf> {
        vec![self.at("helper-state")]
    }
    fn content_addressed(&self) -> Vec<PathBuf> {
        vec![self.at("data/cache")]
    }
    fn surroundings(&self) -> PathBuf {
        self.root.clone()
    }
    fn surroundings_exempt(&self, rel: &Path) -> bool {
        ["data", "hot", "cg"].iter().any(|e| rel.starts_with(e))
    }
    fn surroundings_expected(&self, rel: &Path) -> bool {
        ["etc", "kbf.toml", "helper-state"]
            .iter()
            .any(|e| rel == Path::new(e))
    }
}

fn residue(node: &Fake, before: &Snapshot) -> Vec<String> {
    Snapshot::take(node).expect("a look").residue_since(before)
}

/// Catches: a snapshot that reports residue on an untouched node, or one that misses a
/// lease directory, a cgroup, a user, a session, a home, a file left in a directory that
/// must stay empty, a new path, a path that grew (in the scratch directory and around it),
/// or an unexpected path around the scratch directory. Each finding names what it saw, and a
/// kept directory is not residue.
#[test]
fn a_snapshot_names_every_kind_of_residue_and_nothing_on_a_clean_node() {
    let tmp = dir("snapshot");
    let node = Fake::new(tmp.path());
    put(&node.at("data/cache/kept"), "kept on purpose");
    put(&node.at("hot/blob"), &"b".repeat(3000));
    put(&node.at("data/old"), "o");
    let before = Snapshot::take(&node).expect("a baseline");
    assert!(before.kept > 0, "{before:?}");
    assert!(before.stores.1 >= 3000, "{before:?}");
    assert!(before.files.contains_key(Path::new("old")));
    assert!(!before.files.contains_key(Path::new("cache/kept")));
    assert_eq!(residue(&node, &before), Vec::<String>::new());

    // Kept stays kept, and a store may grow.
    put(&node.at("data/cache/more"), "more");
    put(&node.at("hot/blob2"), "more");
    assert_eq!(residue(&node, &before), Vec::<String>::new());

    put(&node.at("data/leases/kbf-lease-1/x"), "x");
    std::fs::create_dir_all(node.at(&format!("cg/{LEASE_PREFIX}1"))).expect("mkdir");
    put(&node.at("data/stray"), "s");
    put(&node.at("data/old"), "grown by a byte or more");
    put(&node.at("helper-state/job/x"), "x");
    put(&node.at("surprise"), "s");
    put(&node.at("etc/new"), "n");
    node.users.lock().expect("lock").insert("u1".to_owned());
    node.sessions.lock().expect("lock").insert("sim".to_owned());
    node.homes
        .lock()
        .expect("lock")
        .insert("home-u1".to_owned());
    let found = residue(&node, &before).join("\n");
    for want in [
        "lease directory left: kbf-lease-1",
        "lease cgroup left: kbf-lease-1",
        "slot user left: u1",
        "session left: sim",
        "home directory left: home-u1",
        "left in a directory that must be empty: helper-state/job",
        "new path: stray",
        "grew: old",
        "unexpected path outside the scratch dir: surprise",
        "new path outside the scratch dir: etc/new",
    ] {
        assert!(found.contains(want), "no `{want}` in:\n{found}");
    }

    // A path that grew outside the scratch directory, and one that was already unexpected
    // at the baseline (every lease may rewrite it).
    let tmp2 = dir("snapshot-around");
    let node2 = Fake::new(tmp2.path());
    put(&node2.at("etc/conf"), "a");
    let base2 = Snapshot::take(&node2).expect("a baseline");
    put(&node2.at("etc/conf"), "grown larger than before");
    assert!(
        residue(&node2, &base2)
            .join("\n")
            .contains("grew outside the scratch dir: etc/conf (1 -> 24 bytes)")
    );
    put(&node2.at("loose"), "l");
    let base3 = Snapshot::take(&node2).expect("a baseline");
    assert!(
        residue(&node2, &base3)
            .join("\n")
            .contains("unexpected path outside the scratch dir: loose")
    );
}

/// Catches: growth that only shows in the allocated bytes (a sparse file filled in, same
/// apparent length) going unseen, a kept path that is a link being counted as a store, and a
/// content store outside the scratch directory that is not there being an error.
#[test]
fn a_snapshot_sees_allocation_growth_and_tolerates_a_missing_outside_store() {
    let tmp = dir("snapshot-alloc");
    let node = Fake::new(tmp.path());
    let sparse = node.at("data/sparse");
    std::fs::File::create(&sparse)
        .expect("create")
        .set_len(8192)
        .expect("set_len");
    let before = Snapshot::take(&node).expect("a baseline");
    std::fs::write(&sparse, vec![1u8; 8192]).expect("fill");
    assert_eq!(
        residue(&node, &before),
        ["grew: sparse (8192 -> 8192 bytes)"]
    );

    let around = node.at("etc/sparse");
    std::fs::File::create(&around)
        .expect("create")
        .set_len(8192)
        .expect("set_len");
    let before = Snapshot::take(&node).expect("a baseline");
    std::fs::write(&around, vec![1u8; 8192]).expect("fill");
    assert!(
        residue(&node, &before)
            .join("\n")
            .contains("grew outside the scratch dir: etc/sparse (8192 -> 8192 bytes)")
    );

    std::fs::remove_dir(node.at("hot")).expect("rmdir");
    std::fs::remove_dir(node.at("data/cache")).expect("rmdir");
    symlink("nowhere", node.at("data/cache")).expect("link");
    let snapshot = Snapshot::take(&node).expect("a store that is not there is skipped");
    assert_eq!(snapshot.kept, 0);
}

/// Catches: a snapshot of a node whose directory is missing passing as clean (a probe on a
/// wrong path must not pass).
#[test]
fn a_snapshot_fails_where_an_area_is_missing() {
    let tmp = dir("snapshot-missing");
    let node = Fake::new(tmp.path());
    std::fs::remove_dir(node.at("helper-state")).expect("rmdir");
    assert!(
        Snapshot::take(&node)
            .expect_err("missing")
            .contains("helper-state")
    );
}

/// Catches: `clean_since` that passes with a process left over, fails for a process of the
/// baseline, gives up on a process that ends within the settle time, reports a node it
/// could not look at as clean, or returns Ok over file residue.
#[test]
fn clean_since_gives_processes_time_to_end_and_reports_the_rest_at_once() {
    let tmp = dir("clean");
    let node = Fake::new(tmp.path());
    node.show(vec![vec![5]]);
    let before = Snapshot::take(&node).expect("a baseline");
    assert_eq!(before.pids, BTreeSet::from([5]));
    // Process 5 was there before: not residue. Process 6 ends within the settle time.
    node.show(vec![vec![5, 6], vec![5, 6], vec![5]]);
    assert_eq!(clean_since(&node, &before), Ok(()));
    // Process 6 stays.
    node.show(vec![vec![5, 6]]);
    let error = clean_since_within(&node, &before, Duration::from_millis(120)).expect_err("left");
    assert_eq!(error, "process left: 6 (state S) cmd6");
    // File residue is reported with it.
    put(&node.at("data/stray"), "s");
    let error = clean_since_within(&node, &before, Duration::ZERO).expect_err("left");
    assert!(
        error.contains("new path: stray; process left: 6"),
        "{error}"
    );
    // The look of the snapshot fails (the baseline was the first), then the look of the poll.
    for fail_look in [2, 3] {
        let tmp = dir("clean-fails");
        let mut node = Fake::new(tmp.path());
        let before = Snapshot::take(&node).expect("a baseline");
        node.fail_look = fail_look;
        let error = clean_since(&node, &before).expect_err("cannot look");
        assert_eq!(error, "cannot look: no /proc", "look {fail_look}");
    }
}

/// Catches: a canary search that looks inside the content stores (which hold content by
/// design), or misses a lease's output; and a copy in a content-addressed store that is
/// named for something other than its digest, or one that is.
#[test]
fn files_containing_skips_the_stores_and_canary_outside_digests_skips_the_named_ones() {
    let tmp = dir("containing");
    let node = Fake::new(tmp.path());
    put(&node.at("hot/blob"), "SECRET");
    put(&node.at("data/leases/l1/stdout"), "SECRET");
    put(&node.at("data/cache/entry"), "SECRET");
    let mut hits = files_containing(&node, tmp.path(), b"SECRET").expect("a scan");
    hits.sort();
    assert_eq!(hits, [node.at("data/leases/l1/stdout")]);

    put(&node.at("data/cache/named-6"), "SECRET");
    put(&node.at("data/cache/copy-1"), "SECRET");
    let mut leaks =
        canary_outside_digests(&node, b"SECRET", &|abs| abs.ends_with("named-6")).expect("a scan");
    leaks.sort();
    assert_eq!(
        leaks,
        [node.at("data/cache/copy-1"), node.at("data/cache/entry")]
    );
}
