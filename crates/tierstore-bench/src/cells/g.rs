//! Cell G, the tree read.
//!
//! Quantity: files per second and p50/p99 of a read of the 18,105-file tree (344,233,431 bytes),
//! at 1, 16, 32, 64 and 128 requests in flight with connections kept alive, from each of:
//! - `instance`: through the instance, its objects written first as multipart uploads, which
//!   the instance sends to its cold tier only (the instance has no cold-only mode at the pinned
//!   tag and runs two-tier), so the warm-up pass copies them back and the measured passes read
//!   what the instance then holds;
//! - `direct-cold` and `direct-hot`: the cold and the hot test bucket directly (the hot only when
//!   the run configures the hot role), with the objects written by one PUT each;
//! - `loopback-http`: a static file server on loopback over the same files on local disk;
//! - `local-disk`: the same files read from the local file system.
//!
//! Content is synthetic: the real file count and total, sizes drawn by a seeded generator
//! ([`crate::cells::tree`]), bytes from the seed. The manifest holds digests only
//! (`g-manifest.json` of the result directory); the warm-up pass checks the digest of every file
//! it reads, and every pass checks the length. A read that gives other bytes is a failure.
//! The local files are freshly written, so the two local baselines read from the page cache.
//!
//! Requests to the cold store are held to the cold cap; requests to the instance and to the hot
//! store to the hot cap (the instance's reads are meant to be served from what it holds, and a
//! cold cap on them would measure the cap). Which process saturates: each pass notes the CPU time
//! this tool's process used; the instance's own CPU is not measured by this tool, and is read
//! from its host.
//!
//! Writes under `tierstore-bench/<run id>/g/` of the test buckets, counts the cold bytes against
//! the ledger, and deletes the objects, the local files and stops the file server at the end,
//! also when the run stops early.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::OnceCell;

use crate::cells::engine::{
    Arm, Cap, Charger, Env, Hook, LoadSource, Pace, ProcLoad, Step, hook, micros, run_cell, work,
};
use crate::cells::fileserver::FileServer;
use crate::cells::tree::{FILES, ManifestEntry, TOTAL_BYTES, Tree, TreeFile};
use crate::cells::{CellContext, CellId, CellPlan};
use crate::error::BenchError;
use crate::result::{CellResult, Op};
use crate::role::Role;
use crate::s3::{BenchS3, unix_ms};

/// Clock ticks per second of `/proc/<pid>/stat`, as Linux has it on every platform kbf runs on.
const CLOCK_TICKS: f64 = 100.0;

/// The directory of the local copy of the tree, in the result directory.
const LOCAL_DIR: &str = "g-tree";

/// Where the files of an arm are read from.
#[derive(Clone)]
enum Source {
    /// The local file system.
    Disk { root: PathBuf },
    /// The file server on loopback.
    Http {
        client: reqwest::Client,
        base: String,
    },
    /// A bucket of a store role.
    Store { s3: Arc<BenchS3>, prefix: String },
}

/// What a fetch got.
struct Fetched {
    started_unix_ms: u64,
    latency_us: u64,
    status: Option<u16>,
    body: Option<Bytes>,
}

impl Source {
    async fn fetch(&self, file: &TreeFile) -> Result<Fetched, BenchError> {
        let started_unix_ms = unix_ms();
        let clock = Instant::now();
        let (status, body) = match self {
            Source::Disk { root } => {
                let path = root.join(&file.path);
                let read = tokio::task::spawn_blocking(move || std::fs::read(path)).await;
                let body = read.ok().and_then(Result::ok).map(Bytes::from);
                (body.as_ref().map(|_| 200), body)
            }
            Source::Http { client, base } => {
                match client.get(format!("{base}/{}", file.path)).send().await {
                    Err(_) => (None, None),
                    Ok(response) => {
                        let status = response.status();
                        let body = if status.is_success() {
                            response.bytes().await.ok()
                        } else {
                            None
                        };
                        (Some(status.as_u16()), body)
                    }
                }
            }
            Source::Store { s3, prefix } => {
                let key = format!("{prefix}/{}", file.path);
                let got = s3.get_range(&key, 0, file.size).await?;
                return Ok(Fetched {
                    started_unix_ms: got.started_unix_ms,
                    latency_us: got.latency_us(),
                    status: got.status,
                    body: got.result.ok(),
                });
            }
        };
        Ok(Fetched {
            started_unix_ms,
            latency_us: micros(clock.elapsed()),
            status,
            body,
        })
    }
}

/// The step of one read: good if it gave the file's length and, when `digest` is given, its
/// digest. A read that gave other bytes keeps no status.
fn step_of(role: Role, file: &TreeFile, digest: Option<&str>, fetched: &Fetched) -> Step {
    let intact = fetched.body.as_ref().is_some_and(|body| {
        body.len() as u64 == file.size
            && digest.is_none_or(|want| hex::encode(Sha256::digest(body)) == want)
    });
    let status = if intact || fetched.body.is_none() {
        fetched.status
    } else {
        None
    };
    Step {
        label: "",
        second_quantity: false,
        role,
        op: Op::Get,
        bytes: fetched.body.as_ref().map_or(0, |b| b.len() as u64),
        started_unix_ms: fetched.started_unix_ms,
        latency_us: fetched.latency_us,
        status,
        ok: intact,
        cold_write: 0,
        in_flight: 0,
    }
}

/// The objects of the tree in a store, written once.
struct Stored {
    tree: Arc<Tree>,
    writer: Arc<BenchS3>,
    prefix: String,
    multipart: bool,
    cold: bool,
    charger: Charger,
    ready: OnceCell<()>,
}

impl Stored {
    async fn ensure(&self) -> Result<(), BenchError> {
        self.ready
            .get_or_try_init(|| async {
                for (index, file) in self.tree.files.iter().enumerate() {
                    let key = format!("{}/{}", self.prefix, file.path);
                    let body = Bytes::from(self.tree.content(index));
                    // Counted before it is sent: a put that is cut off may have landed.
                    if self.cold {
                        self.charger.charge(file.size)?;
                    }
                    let put = if self.multipart {
                        self.writer.put_multipart(&key, body).await?
                    } else {
                        self.writer.put(&key, body).await?
                    };
                    if !put.is_ok() {
                        return Err(BenchError::Setup(format!(
                            "g: the store of role {} would not take the tree",
                            self.writer.role()
                        )));
                    }
                }
                Ok(())
            })
            .await
            .map(|_| ())
    }

    async fn clean(&self) -> Result<u64, BenchError> {
        let mut deleted = 0;
        for file in &self.tree.files {
            let key = format!("{}/{}", self.prefix, file.path);
            deleted += u64::from(self.writer.delete(&key).await?.is_ok());
        }
        Ok(deleted)
    }
}

/// CPU time (user and system, in clock ticks) of a `/proc/<pid>/stat` text.
fn cpu_ticks(stat: &str) -> Option<u64> {
    // The name is in parentheses and may hold spaces; the fields that count from 3 follow it.
    let after = stat.rsplit_once(')')?.1;
    let mut fields = after.split_whitespace().skip(11);
    let user: u64 = fields.next()?.parse().ok()?;
    let system: u64 = fields.next()?.parse().ok()?;
    Some(user + system)
}

/// CPU ticks of this process so far, if the system says.
fn own_cpu_ticks() -> Option<u64> {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .as_deref()
        .and_then(cpu_ticks)
}

/// What an arm needs: where it reads, and what it writes first.
struct Reader {
    role: Role,
    source: Source,
    tree: Arc<Tree>,
    digests: Arc<Vec<ManifestEntry>>,
}

impl Reader {
    async fn read(&self, pass: u32, index: u64) -> Result<Vec<Step>, BenchError> {
        let at = usize::try_from(index).expect("an index");
        let file = &self.tree.files[at];
        let digest = (pass == 0).then(|| self.digests[at].sha256.as_str());
        let fetched = self.source.fetch(file).await?;
        Ok(vec![step_of(self.role, file, digest, &fetched)])
    }
}

#[derive(Serialize)]
struct ManifestFile<'a> {
    seed: u64,
    files: &'a [ManifestEntry],
}

/// The arms of the cell and the cleanup that removes what they read.
///
/// # Errors
/// [`BenchError`] if a client cannot be made, the local copy of the tree or the file server
/// cannot be set up, or the manifest cannot be written.
pub async fn arms(
    env: &Env,
    plan: &CellPlan,
    limit: Option<u64>,
) -> Result<(Vec<Arm>, Hook), BenchError> {
    let (files, total) = limit.map_or((FILES, TOTAL_BYTES), |n| {
        (
            usize::try_from(n).expect("a count"),
            n * (TOTAL_BYTES / FILES as u64),
        )
    });
    let tree = Arc::new(Tree::generate(env.config.seed, files, total));
    let digests = Arc::new(tree.manifest());
    let manifest = serde_json::to_string(&ManifestFile {
        seed: tree.seed,
        files: &digests,
    })
    .expect("a manifest serialises");
    let manifest_path = env.config.result_dir.join("g-manifest.json");
    std::fs::write(&manifest_path, manifest).map_err(|source| BenchError::Io {
        path: manifest_path,
        source,
    })?;

    let mut stored = Vec::new();
    for (name, role, multipart, cap, cold) in [
        ("direct-cold", Role::Cold, false, Cap::Cold, true),
        ("direct-hot", Role::Hot, false, Cap::Hot, false),
        ("instance", Role::Instance, true, Cap::Hot, true),
    ] {
        if env.config.stores.contains_key(&role) {
            let s3 = env.s3(role, cap)?;
            let prefix = format!("{}/g/{name}", env.prefix());
            stored.push((
                name,
                Arc::new(Stored {
                    tree: tree.clone(),
                    writer: s3,
                    prefix,
                    multipart,
                    cold,
                    charger: env.charger(),
                    ready: OnceCell::new(),
                }),
            ));
        }
    }

    let root = env.config.result_dir.join(LOCAL_DIR);
    let made = materialise(&tree, &root);
    let server = match made {
        Ok(()) => FileServer::start(root.clone()).await,
        Err(e) => Err(e),
    };
    let server = match server {
        Ok(server) => server,
        Err(e) => {
            std::fs::remove_dir_all(&root).ok();
            return Err(e);
        }
    };
    let client = reqwest::Client::new();
    let base = server.url();

    let mut sources: Vec<(&str, Role, Source, Option<Arc<Stored>>)> = vec![
        (
            "local-disk",
            Role::Generator,
            Source::Disk { root: root.clone() },
            None,
        ),
        (
            "loopback-http",
            Role::Generator,
            Source::Http { client, base },
            None,
        ),
    ];
    for (name, set) in &stored {
        sources.push((
            name,
            set.writer.role(),
            Source::Store {
                s3: set.writer.clone(),
                prefix: set.prefix.clone(),
            },
            Some(set.clone()),
        ));
    }

    let mut arms = Vec::new();
    for (name, role, source, set) in sources {
        for &in_flight in &plan.in_flight {
            let reader = Arc::new(Reader {
                role,
                source: source.clone(),
                tree: tree.clone(),
                digests: digests.clone(),
            });
            arms.push(arm(
                format!("{name}-c{in_flight}"),
                in_flight,
                &reader,
                set.clone(),
                notes(name, files, total),
            ));
        }
    }
    let cleanup = cleanup(stored.into_iter().map(|(_, s)| s).collect(), server, root);
    Ok((arms, cleanup))
}

/// Writes the files of `tree` under `root`.
fn materialise(tree: &Tree, root: &std::path::Path) -> Result<(), BenchError> {
    let io = |path: &std::path::Path| {
        let path = path.to_owned();
        move |source| BenchError::Io { path, source }
    };
    for (index, file) in tree.files.iter().enumerate() {
        let path = root.join(&file.path);
        let parent = path.parent().expect("a file has a directory");
        std::fs::create_dir_all(parent).map_err(io(parent))?;
        std::fs::write(&path, tree.content(index)).map_err(io(&path))?;
    }
    Ok(())
}

fn notes(name: &str, files: usize, total: u64) -> Vec<String> {
    let path = match name {
        "instance" => {
            "through the instance: the objects were written as multipart uploads (cold tier only), the warm-up pass copies them back, the measured passes read what the instance then holds; two-tier instance, no cold-only mode at the pinned tag"
        }
        "direct-cold" => "straight from the cold test bucket, objects written by one PUT each",
        "direct-hot" => "straight from the hot test bucket, objects written by one PUT each",
        "loopback-http" => {
            "from a static file server on loopback over the local files, HTTP/1.1 keep-alive"
        }
        _ => "from the local file system, files freshly written so in the page cache",
    };
    vec![
        format!("g, {path}: a read of {files} synthetic files of {total} bytes in all (real count and total, sizes drawn from a seed, content from the seed)"),
        "test instance and test buckets; the warm-up pass checks every file's digest against the manifest, every pass its length".to_owned(),
    ]
}

fn arm(
    label: String,
    in_flight: u32,
    reader: &Arc<Reader>,
    set: Option<Arc<Stored>>,
    notes: Vec<String>,
) -> Arm {
    let requests = reader.tree.files.len() as u64;
    let once = set
        .as_ref()
        .map_or(0, |s| if s.cold { reader.tree.total() } else { 0 });
    let ticks = Arc::new(AtomicU64::new(0));
    let (before_ticks, after_ticks) = (ticks.clone(), ticks);
    let reader = reader.clone();
    Arm {
        label,
        pace: Pace::Closed,
        in_flight,
        requests,
        cold_bytes_per_pass: 0,
        cold_bytes_once: once,
        work: work(move |unit| {
            let reader = reader.clone();
            async move { reader.read(unit.pass, unit.index).await }
        }),
        before: Some(hook(move |_| {
            let (set, ticks) = (set.clone(), before_ticks.clone());
            async move {
                if let Some(set) = &set {
                    set.ensure().await?;
                }
                ticks.store(own_cpu_ticks().unwrap_or(0), Ordering::SeqCst);
                Ok(vec![])
            }
        })),
        after: Some(hook(move |_| {
            let used = own_cpu_ticks()
                .unwrap_or(0)
                .saturating_sub(after_ticks.load(Ordering::SeqCst));
            std::future::ready(Ok(vec![format!(
                "cpu time of this tool's process in the pass: {:.2} s (user and system, {CLOCK_TICKS} ticks a second assumed)",
                used as f64 / CLOCK_TICKS
            )]))
        })),
        faulted: false,
        proxy: None,
        notes,
    }
}

/// Deletes the objects, stops the file server and removes the local files.
fn cleanup(stored: Vec<Arc<Stored>>, server: FileServer, root: PathBuf) -> Hook {
    let state = Arc::new((stored, Mutex::new(Some(server)), root));
    hook(move |_| {
        let state = state.clone();
        async move {
            let (stored, server, root) = &*state;
            let running = server.lock().unwrap_or_else(PoisonError::into_inner).take();
            running.iter().for_each(FileServer::stop);
            let removed = std::fs::remove_dir_all(root).is_ok();
            let (mut deleted, mut total) = (0, 0);
            for set in stored {
                deleted += set.clean().await?;
                total += set.tree.files.len();
            }
            Ok(vec![format!(
                "cleanup: {deleted} of {total} objects deleted, file server stopped, local files removed: {removed}"
            )])
        }
    })
}

/// Runs the cell: one [`CellResult`] per arm, passes interleaved, pass 0 discarded.
///
/// # Errors
/// [`BenchError`] if the tree cannot be set up, the objects cannot be written, the ledger
/// refuses the cell, the load gate trips or cannot be read, or a file or the request log fails.
pub async fn run(ctx: &CellContext, plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    run_with(ctx, plan, Arc::new(ProcLoad), None).await
}

/// [`run`] with the machine given and the tree cut to `limit` files (of the paper's mean size).
///
/// # Errors
/// As [`run`].
pub async fn run_with(
    ctx: &CellContext,
    plan: &CellPlan,
    load: Arc<dyn LoadSource>,
    limit: Option<u64>,
) -> Result<Vec<CellResult>, BenchError> {
    let env = Env::new(ctx, load)?;
    let (arms, cleanup) = arms(&env, plan, limit).await?;
    run_cell(&env, CellId::G, arms, plan.passes, Some(cleanup)).await
}

#[cfg(test)]
mod tests {
    use objstore_s3::s3::fake::FakeS3;

    use super::*;
    use crate::cells::testkit::{self, Script};
    use crate::result::Window;

    fn plan(in_flight: &[u32]) -> CellPlan {
        CellPlan {
            passes: 2,
            in_flight: in_flight.to_vec(),
            object_bytes: vec![],
            open_loop: true,
        }
    }

    fn temp(name: &str) -> kbf_testkit::dir::TestDir {
        kbf_testkit::dir::TestDir::under(std::env::temp_dir(), name)
    }

    fn file(size: u64) -> TreeFile {
        TreeFile {
            path: "d000/f00000".to_owned(),
            size,
        }
    }

    fn fetched(body: Option<&'static [u8]>, status: Option<u16>) -> Fetched {
        Fetched {
            started_unix_ms: 4,
            latency_us: 90,
            status,
            body: body.map(Bytes::from_static),
        }
    }

    /// Catches a read counted good that is not: a body of another length, one of another content
    /// when its digest is checked, and a read with no body, which keeps its status (a 404 is a
    /// 404) while a body of the wrong bytes keeps none.
    #[test]
    fn a_read_is_good_only_if_its_bytes_are_the_files() {
        let digest = hex::encode(Sha256::digest(b"abc"));
        let good = step_of(
            Role::Cold,
            &file(3),
            Some(&digest),
            &fetched(Some(b"abc"), Some(200)),
        );
        assert!(good.ok);
        assert_eq!(
            (good.bytes, good.status, good.latency_us, good.role),
            (3, Some(200), 90, Role::Cold)
        );
        let unchecked = step_of(
            Role::Cold,
            &file(3),
            None,
            &fetched(Some(b"xyz"), Some(200)),
        );
        assert!(
            unchecked.ok,
            "without a digest the length is what is checked"
        );
        let altered = step_of(
            Role::Cold,
            &file(3),
            Some(&digest),
            &fetched(Some(b"xyz"), Some(200)),
        );
        assert!(!altered.ok);
        assert_eq!(altered.status, None);
        let short = step_of(
            Role::Cold,
            &file(5),
            None,
            &fetched(Some(b"abc"), Some(200)),
        );
        assert!(!short.ok);
        assert_eq!((short.bytes, short.status), (3, None));
        let missing = step_of(Role::Cold, &file(3), None, &fetched(None, Some(404)));
        assert!(!missing.ok);
        assert_eq!((missing.bytes, missing.status), (0, Some(404)));
        let silent = step_of(Role::Cold, &file(3), None, &fetched(None, None));
        assert_eq!(silent.status, None);
    }

    /// Catches the CPU reader taking the wrong fields (a name with spaces and parentheses moves
    /// them) or a short or damaged line for a number.
    #[test]
    fn cpu_ticks_are_the_user_and_system_time_after_the_name() {
        let line = "42 (bench (x) y) S 1 1 1 0 -1 0 0 0 0 0 7 3 0 0 20 0 1 0 100";
        assert_eq!(cpu_ticks(line), Some(10));
        assert_eq!(cpu_ticks("no name here"), None);
        assert_eq!(cpu_ticks("1 (a) S 1 1"), None);
        assert_eq!(cpu_ticks("1 (a) S 1 1 1 0 -1 0 0 0 0 0 x 3"), None);
        assert_eq!(cpu_ticks("1 (a) S 1 1 1 0 -1 0 0 0 0 0 7 y"), None);
        let _ = own_cpu_ticks();
    }

    /// Catches a source that reports what it did not get: a missing file, a server that is not
    /// there, a missing object, each a read with no body and, where there was an answer, its
    /// status.
    #[tokio::test]
    async fn a_source_that_cannot_give_the_file_says_so() {
        let dir = temp("tb-g-sources");
        let missing = file(3);
        let disk = Source::Disk {
            root: dir.path().to_owned(),
        };
        let got = disk.fetch(&missing).await.unwrap();
        assert_eq!((got.body.is_none(), got.status), (true, None));

        let dead = testkit::DeadStore::start().await;
        let http = Source::Http {
            client: reqwest::Client::new(),
            base: dead.endpoint().to_owned(),
        };
        let got = http.fetch(&missing).await.unwrap();
        assert_eq!((got.body.is_none(), got.status), (true, None));

        let server = FileServer::start(dir.path().to_owned()).await.unwrap();
        let http = Source::Http {
            client: reqwest::Client::new(),
            base: server.url(),
        };
        let got = http.fetch(&missing).await.unwrap();
        assert_eq!((got.body.is_none(), got.status), (true, Some(404)));
        server.stop();

        let fake = FakeS3::start().await;
        let ctx = testkit::ctx(dir.path(), &[(Role::Cold, fake.endpoint())], Window::Busy);
        let env = Env::new(&ctx, Script::calm()).unwrap();
        let store = Source::Store {
            s3: env.s3(Role::Cold, Cap::Cold).unwrap(),
            prefix: "g/x".to_owned(),
        };
        let got = store.fetch(&missing).await.unwrap();
        assert_eq!((got.body.is_none(), got.status), (true, Some(404)));
    }

    /// Catches an arm that reads from the wrong place, a tree that is not the one hashed, objects
    /// or files left behind, a file server left running, the cold ledger charged for the hot
    /// store (or not charged for the instance), and a manifest that holds anything but digests:
    /// five sources at two widths over a tree of five files.
    #[tokio::test]
    async fn every_source_reads_the_tree_and_everything_is_removed_after() {
        let dir = temp("tb-g");
        let (cold, hot, instance) = (
            FakeS3::start().await,
            FakeS3::start().await,
            FakeS3::start().await,
        );
        let stores = [
            (Role::Cold, cold.endpoint()),
            (Role::Hot, hot.endpoint()),
            (Role::Instance, instance.endpoint()),
        ];
        let ctx = testkit::ctx(dir.path(), &stores, Window::Busy);
        let results = run_with(&ctx, &plan(&[1, 2]), Script::calm(), Some(5))
            .await
            .unwrap();
        let labels: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(
            labels,
            [
                "direct-cold-c1",
                "direct-cold-c2",
                "direct-hot-c1",
                "direct-hot-c2",
                "instance-c1",
                "instance-c2",
                "local-disk-c1",
                "local-disk-c2",
                "loopback-http-c1",
                "loopback-http-c2"
            ]
        );
        for result in &results {
            assert_eq!(result.cell, CellId::G);
            assert!(
                result
                    .passes
                    .iter()
                    .all(|p| (p.requests, p.failures) == (5, 0)),
                "{}",
                result.arm
            );
            assert!(
                result
                    .notes
                    .iter()
                    .any(|n| n.contains("cpu time of this tool's process"))
            );
        }
        let (c, h, i) = (cold.stats(), hot.stats(), instance.stats());
        assert_eq!((c.single_puts, h.single_puts, i.single_puts), (5, 5, 0));
        assert_eq!((i.parts, i.completed), (5, 5));
        for fake in [&cold, &hot, &instance] {
            assert!(fake.keys().is_empty());
        }
        assert!(!dir.path().join(LOCAL_DIR).exists());
        let ledger = std::fs::read_to_string(dir.path().join("ledger.json")).unwrap();
        assert_eq!(
            ledger,
            format!("{{\"cold_written_bytes\":{}}}", 2 * 5 * 19_013)
        );
        let manifest = std::fs::read_to_string(dir.path().join("g-manifest.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(parsed["files"].as_array().unwrap().len(), 5);
        assert_eq!(parsed["files"][0]["sha256"].as_str().unwrap().len(), 64);
        assert!(
            results[0]
                .notes
                .iter()
                .any(|n| n.starts_with("cleanup: 15 of 15 objects deleted"))
        );
        let instance_note = results[4].notes.join(" ");
        assert!(instance_note.contains("two-tier"), "{instance_note}");
    }

    /// Catches a run with no store still measuring something: the two local baselines run on
    /// their own, from files that are then gone.
    #[tokio::test]
    async fn the_baselines_run_without_any_store() {
        let dir = temp("tb-g-local");
        let ctx = testkit::ctx(dir.path(), &[], Window::Quiet);
        let results = run_with(&ctx, &plan(&[1]), Script::calm(), Some(3))
            .await
            .unwrap();
        let labels: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(labels, ["local-disk-c1", "loopback-http-c1"]);
        assert!(
            results
                .iter()
                .all(|r| r.passes.iter().all(|p| p.window == Window::Quiet))
        );
        assert!(!dir.path().join(LOCAL_DIR).exists());
    }

    /// Catches a store that will not take the tree being a run that read nothing, and the cleanup
    /// not running after it: the setup is named, the local files are gone.
    #[tokio::test]
    async fn a_store_that_will_not_take_the_tree_stops_the_run() {
        let dir = temp("tb-g-dead");
        let dead_store = testkit::DeadStore::start().await;
        let dead = dead_store.endpoint();
        let ctx = testkit::ctx(dir.path(), &[(Role::Cold, dead)], Window::Busy);
        let err = run_with(&ctx, &plan(&[1]), Script::calm(), Some(2))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("store of role cold would not take the tree"),
            "{err}"
        );
        assert!(!dir.path().join(LOCAL_DIR).exists());
    }

    /// Catches a setup that cannot write its files going on: a local directory that cannot be
    /// made, and a manifest that cannot be written, are errors.
    #[tokio::test]
    async fn a_tree_that_cannot_be_written_is_an_error() {
        let dir = temp("tb-g-io");
        std::fs::write(dir.path().join(LOCAL_DIR), "in the way").unwrap();
        let ctx = testkit::ctx(dir.path(), &[], Window::Busy);
        let err = run_with(&ctx, &plan(&[1]), Script::calm(), Some(2))
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");

        let dir = temp("tb-g-io-manifest");
        std::fs::create_dir(dir.path().join("g-manifest.json")).unwrap();
        let ctx = testkit::ctx(dir.path(), &[], Window::Busy);
        let err = run_with(&ctx, &plan(&[1]), Script::calm(), Some(2))
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");
    }

    /// Catches a request of the cell whose record cannot be written being taken for a good one:
    /// the log refuses from its n-th write, for every n, until a run is long enough not to meet
    /// it.
    #[tokio::test]
    async fn a_log_that_fails_at_any_request_stops_the_run() {
        let mut finished = false;
        for taken in 0..400 {
            let dir = temp("tb-g-log");
            let (cold, instance) = (FakeS3::start().await, FakeS3::start().await);
            let stores = [
                (Role::Cold, cold.endpoint()),
                (Role::Instance, instance.endpoint()),
            ];
            let log = Arc::new(crate::requestlog::RequestLog::to_writer(
                testkit::FailAfter(taken),
            ));
            let ctx = testkit::ctx_with_log(dir.path(), &stores, Window::Busy, log);
            match run_with(&ctx, &plan(&[1]), Script::calm(), Some(2)).await {
                Ok(_) => {
                    finished = true;
                    break;
                }
                Err(e) => assert!(matches!(e, BenchError::Log(_)), "{taken}: {e}"),
            }
        }
        assert!(finished);
    }
}
