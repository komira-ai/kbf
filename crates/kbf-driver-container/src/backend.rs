//! The container runtime behind the driver's six steps: [`Backend`], with two
//! implementations, Podman ([`PodmanBackend`]) and `crun` driven directly (`oci`).
//!
//! The lease code in `runtime` does everything that does not depend on the runtime:
//! the CAS, the input root, the lease cgroup and its limits, the watch, the OOM
//! classification, output collection, cleaning. It asks a backend for what does:
//! whether the image is usable, who may write the lease's directories, making and
//! starting the container, how it ended, signalling it, removing what it left.
//!
//! [`Engine`] is the closed set of backends the node can be configured with
//! (`--container-runtime`), cheap to clone into a lease.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_daemon::cache::Disabled;
use tokio::process::Child;

use crate::image::{ImageCheckError, ImageRef, ManifestKind, manifest_file, manifest_kind};
use crate::oci::{Oci, RunEnd};
use crate::podman::{ContainerSpec, Ended, Podman};
use crate::program::Said;

/// Which runtime a node runs its containers with (`--container-runtime`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContainerRuntime {
    /// Rootless Podman: `podman create` and `podman start` per action.
    #[default]
    Podman,
    /// `crun` run directly from a bundle the driver writes, with no Podman on the
    /// action path (`oci`).
    Oci,
}

impl std::str::FromStr for ContainerRuntime {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "podman" => Ok(Self::Podman),
            "oci" => Ok(Self::Oci),
            other => Err(format!(
                "{other:?} is not a container runtime: podman or oci"
            )),
        }
    }
}

/// The directories a lease hands between the daemon and the action's root.
pub(crate) struct Dirs<'a> {
    /// The lease's scratch directory.
    pub lease: &'a Path,
    /// The input root (the overlay's lower layer).
    pub root: &'a Path,
    pub upper: &'a Path,
    pub work: &'a Path,
    /// With the input cache, the daemon's binary: the input root is then given by its
    /// directories alone ([`crate::chown_dirs`]), never its files.
    pub helper: Option<&'a Path>,
}

/// What the runtime-specific part of a lease runs as, from `start` until it ends.
#[derive(Debug)]
pub(crate) enum Running {
    /// `podman start --attach`.
    Process(Child),
    /// `crun run`, driven by a thread of its own.
    Run(Run),
}

/// A `crun run` driven by the `runc` crate's async client in a thread of its own, on a
/// one-thread runtime that thread owns.
///
/// A thread rather than a task of the daemon's runtime: the lease's clean runs inside
/// `Drop`, blocking, and must wait for `crun` to be gone before it removes anything
/// (kbf#157). A task is polled by the runtime the clean is blocking, which on a
/// one-thread runtime, or when every worker is cleaning, would never finish; a thread
/// that owns the child can always be waited for. The cost is one OS thread per running
/// action, for the length of the action.
#[derive(Debug)]
pub(crate) struct Run {
    thread: Option<std::thread::JoinHandle<()>>,
    ended: Option<tokio::sync::oneshot::Receiver<RunEnd>>,
    abort: Option<tokio::sync::oneshot::Sender<()>>,
    end: Option<RunEnd>,
}

impl Run {
    /// Runs `work` (a `crun run`) on a new thread named `name`. Dropping `work`, which
    /// aborting does, kills `crun` (`oci::crun::KillOnDrop`).
    pub(crate) fn spawn<F>(name: String, work: F) -> Result<Self, String>
    where
        F: std::future::Future<Output = RunEnd> + Send + 'static,
    {
        Self::spawn_with(
            std::thread::Builder::new().name(name),
            one_thread_runtime,
            work,
        )
    }

    /// [`Run::spawn`] on `thread`, with `runtime` making the thread's runtime: the two
    /// places it can fail, each a test's to cause.
    fn spawn_with<F>(
        thread: std::thread::Builder,
        runtime: fn() -> std::io::Result<tokio::runtime::Runtime>,
        work: F,
    ) -> Result<Self, String>
    where
        F: std::future::Future<Output = RunEnd> + Send + 'static,
    {
        let (end_tx, end_rx) = tokio::sync::oneshot::channel();
        let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<()>();
        let body = move || {
            let end = match runtime() {
                Ok(runtime) => runtime.block_on(async {
                    tokio::select! {
                        end = work => end,
                        _ = abort_rx => RunEnd::Failed("crun run was aborted".to_owned()),
                    }
                }),
                Err(e) => RunEnd::Failed(format!("the runtime of the crun thread: {e}")),
            };
            let _ = end_tx.send(end);
        };
        let thread = thread
            .spawn(body)
            .map_err(|e| format!("start the crun thread: {e}"))?;
        Ok(Self {
            thread: Some(thread),
            ended: Some(end_rx),
            abort: Some(abort_tx),
            end: None,
        })
    }

    /// How the run ended, once [`Running::wait`] returned.
    pub(crate) fn end(&self) -> Option<&RunEnd> {
        self.end.as_ref()
    }
}

/// The runtime of a `crun` thread: one thread, with the drivers `crun`'s child needs.
fn one_thread_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

impl Running {
    /// Waits until the start has ended. Cancel-safe.
    pub(crate) async fn wait(&mut self) {
        match self {
            Self::Process(child) => {
                let _ = child.wait().await;
            }
            Self::Run(run) => {
                if let Some(ended) = run.ended.as_mut() {
                    let end = ended.await.unwrap_or_else(|_| {
                        RunEnd::Failed("the crun thread ended without an answer".to_owned())
                    });
                    run.end = Some(end);
                    run.ended = None;
                }
            }
        }
    }

    /// Ends the start by force: SIGKILL to `podman start`, or the abort of the run,
    /// which kills `crun`.
    pub(crate) async fn force(&mut self) {
        match self {
            Self::Process(child) => {
                let _ = child.kill().await;
            }
            Self::Run(run) => {
                if let Some(abort) = run.abort.take() {
                    let _ = abort.send(());
                }
                self.wait().await;
            }
        }
    }

    /// How the `crun run` ended, once [`Running::wait`] returned; `None` for a Podman
    /// start or a run not waited for.
    pub(crate) fn run_end(&self) -> Option<&RunEnd> {
        match self {
            Self::Run(run) => run.end(),
            Self::Process(_) => None,
        }
    }

    /// Whether the lease cgroup must be killed before this start is waited for: a
    /// `crun run` ends when its container does, and kills nothing itself.
    pub(crate) fn needs_cgroup_kill(&self) -> bool {
        matches!(self, Self::Run(_))
    }

    /// Ends the start and waits at most `limit` for it to be gone, polling every `pause`.
    /// Blocking, without the async runtime: a dropped run's clean runs inside `Drop`.
    ///
    /// For `podman start`, killed here (PID reuse: kept on [`Child`], see `runtime::reap`).
    /// For `crun run`, the caller has killed the lease cgroup, which ends the container
    /// and so `crun`; one still there at `limit` is aborted, which kills it.
    pub(crate) fn reap(&mut self, limit: Duration, pause: Duration) -> Result<(), String> {
        match self {
            Self::Process(child) => crate::runtime::reap(child, limit, pause),
            Self::Run(run) => {
                let Some(thread) = run.thread.take() else {
                    return Ok(());
                };
                let deadline = Instant::now() + limit;
                let mut aborted = false;
                while !thread.is_finished() {
                    if Instant::now() >= deadline {
                        if aborted {
                            return Err(format!(
                                "crun run still runs {limit:?} after its cgroup was killed"
                            ));
                        }
                        if let Some(abort) = run.abort.take() {
                            let _ = abort.send(());
                        }
                        aborted = true;
                    }
                    std::thread::sleep(pause);
                }
                let _ = thread.join();
                Ok(())
            }
        }
    }
}

/// What the lease code needs of a container runtime.
pub(crate) trait Backend {
    /// Checks that this node can run `image` and that its digest names an image, not an
    /// index.
    async fn check_pinned(&self, image: &ImageRef) -> Result<(), ImageCheckError>;

    /// Whether the input cache may be used with this runtime's storage. Blocking.
    fn cache_usable(&self) -> Result<(), Disabled>;

    /// Gives the lease's directories to `owner` (`uid:gid` in the container's user
    /// namespace as the runtime names it: `1:1` is the action's root, `0:0` the daemon).
    async fn give(&self, owner: &str, dirs: &Dirs<'_>) -> Result<(), String>;

    /// Creates the container for `spec` (it does not run yet).
    async fn create(&self, spec: &ContainerSpec) -> Result<(), String>;

    /// Starts the container, its stdout and stderr written to the given files.
    fn start(
        &self,
        spec: &ContainerSpec,
        stdout: std::fs::File,
        stderr: std::fs::File,
    ) -> Result<Running, String>;

    /// How the container ended, after [`Running::wait`] returned.
    async fn ended(&self, spec: &ContainerSpec, running: &Running) -> Result<Ended, String>;

    /// Where this runtime's own words about a container that did not run are.
    fn said(&self, spec: &ContainerSpec, stderr: &Path) -> Said;

    /// Sends SIGTERM to the container's main process.
    async fn terminate(&self, name: &str) -> Result<(), String>;

    /// Removes the container, killing it first if it still runs. Blocking.
    fn remove(&self, name: &str) -> Result<(), String>;

    /// Removes `dir`, whatever ids own the files in it. Blocking.
    fn remove_dir(&self, dir: &Path) -> Result<(), String>;

    /// The names of the lease containers this runtime still holds for `owner`. Blocking.
    fn owned(&self, owner: &str) -> Result<Vec<String>, String>;
}

/// The backends a node can run.
#[derive(Clone, Debug)]
pub(crate) enum Engine {
    Podman(Arc<PodmanBackend>),
    Oci(Arc<Oci>),
}

macro_rules! each {
    ($engine:expr, $b:ident => $call:expr) => {
        match $engine {
            Engine::Podman($b) => $call,
            Engine::Oci($b) => $call,
        }
    };
}

impl Backend for Engine {
    async fn check_pinned(&self, image: &ImageRef) -> Result<(), ImageCheckError> {
        each!(self, b => b.check_pinned(image).await)
    }

    fn cache_usable(&self) -> Result<(), Disabled> {
        each!(self, b => b.cache_usable())
    }

    async fn give(&self, owner: &str, dirs: &Dirs<'_>) -> Result<(), String> {
        each!(self, b => b.give(owner, dirs).await)
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<(), String> {
        each!(self, b => b.create(spec).await)
    }

    fn start(
        &self,
        spec: &ContainerSpec,
        stdout: std::fs::File,
        stderr: std::fs::File,
    ) -> Result<Running, String> {
        each!(self, b => b.start(spec, stdout, stderr))
    }

    async fn ended(&self, spec: &ContainerSpec, running: &Running) -> Result<Ended, String> {
        each!(self, b => b.ended(spec, running).await)
    }

    fn said(&self, spec: &ContainerSpec, stderr: &Path) -> Said {
        each!(self, b => b.said(spec, stderr))
    }

    async fn terminate(&self, name: &str) -> Result<(), String> {
        each!(self, b => b.terminate(name).await)
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        each!(self, b => b.remove(name))
    }

    fn remove_dir(&self, dir: &Path) -> Result<(), String> {
        each!(self, b => b.remove_dir(dir))
    }

    fn owned(&self, owner: &str) -> Result<Vec<String>, String> {
        each!(self, b => b.owned(owner))
    }
}

/// Rootless Podman as the backend: the driver's behaviour before the OCI backend.
#[derive(Debug)]
pub(crate) struct PodmanBackend {
    pub(crate) podman: Podman,
    /// The image store's per-image directories, asked of Podman once.
    images: tokio::sync::OnceCell<PathBuf>,
}

impl PodmanBackend {
    pub(crate) fn new(podman: Podman) -> Self {
        Self {
            podman,
            images: tokio::sync::OnceCell::new(),
        }
    }

    /// Whether Podman's storage holds `image` as one image's manifest, not an index:
    /// by reading the manifest the store keeps under that digest.
    pub(crate) async fn check_store(&self, image: &ImageRef) -> Result<(), ImageCheckError> {
        let Some(id) = self
            .podman
            .image_id(&image.to_string())
            .await
            .map_err(ImageCheckError::Failed)?
        else {
            return Err(ImageCheckError::Missing(image.clone()));
        };
        let images = self
            .images
            .get_or_try_init(|| self.podman.images_dir())
            .await
            .map_err(ImageCheckError::Failed)?;
        let path = images.join(&id).join(manifest_file(image.digest()));
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            ImageCheckError::Failed(format!(
                "the image store holds no manifest {} for image {id}: {e}",
                image.digest()
            ))
        })?;
        match manifest_kind(&bytes, image.digest()).map_err(ImageCheckError::Failed)? {
            ManifestKind::Image => Ok(()),
            ManifestKind::Index => Err(ImageCheckError::Index(image.clone())),
        }
    }
}

impl Backend for PodmanBackend {
    async fn check_pinned(&self, image: &ImageRef) -> Result<(), ImageCheckError> {
        self.check_store(image).await
    }

    /// The cache's overlay checks passed on the kernel's `overlay` graph driver, with no
    /// `mount_program` (fuse-overlayfs) in its options.
    fn cache_usable(&self) -> Result<(), Disabled> {
        let (driver, options) = self.podman.graph_driver().map_err(Disabled::GraphDriver)?;
        if driver != "overlay" || options.get("overlay.mount_program").is_some() {
            return Err(Disabled::GraphDriver(format!("{driver} {options}")));
        }
        Ok(())
    }

    /// Everything under the directories, or with the cache only the input root's
    /// directories: its files are links to the cache's entries, which must stay the
    /// daemon's (unmapped in the container, so the action cannot write them); the upper
    /// and work directories hold only the lease's own files.
    async fn give(&self, owner: &str, dirs: &Dirs<'_>) -> Result<(), String> {
        match dirs.helper {
            Some(helper) => {
                self.podman.chown_dirs(helper, owner, dirs.root).await?;
                self.podman.chown(owner, &[dirs.upper, dirs.work]).await
            }
            None => {
                self.podman
                    .chown(owner, &[dirs.root, dirs.upper, dirs.work])
                    .await
            }
        }
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<(), String> {
        self.podman.create(spec).await
    }

    fn start(
        &self,
        spec: &ContainerSpec,
        stdout: std::fs::File,
        stderr: std::fs::File,
    ) -> Result<Running, String> {
        self.podman
            .start(&spec.name, stdout, stderr)
            .map(Running::Process)
    }

    async fn ended(&self, spec: &ContainerSpec, _running: &Running) -> Result<Ended, String> {
        self.podman.ended(&spec.name).await
    }

    fn said(&self, _spec: &ContainerSpec, stderr: &Path) -> Said {
        Said::podman(stderr)
    }

    async fn terminate(&self, name: &str) -> Result<(), String> {
        self.podman.signal(name, "TERM").await
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        self.podman.remove_blocking(name)
    }

    fn remove_dir(&self, dir: &Path) -> Result<(), String> {
        self.podman.unshare_remove_blocking(dir)
    }

    fn owned(&self, owner: &str) -> Result<Vec<String>, String> {
        self.podman.owned(owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_fails() -> std::io::Result<tokio::runtime::Runtime> {
        Err(std::io::Error::other("no epoll"))
    }

    /// Catches a runtime flag spelled differently from what the daemon's docs say, or
    /// an unknown runtime being taken for one.
    #[test]
    fn the_runtime_flag_parses_podman_and_oci_only() {
        assert_eq!("podman".parse(), Ok(ContainerRuntime::Podman));
        assert_eq!("oci".parse(), Ok(ContainerRuntime::Oci));
        assert_eq!(ContainerRuntime::default(), ContainerRuntime::Podman);
        let refused = "runc"
            .parse::<ContainerRuntime>()
            .expect_err("not a runtime");
        assert!(refused.contains("podman or oci"), "{refused}");
    }

    /// Catches a run whose answer is lost: the thread's own failures (no runtime, no
    /// thread), a panic in the work (the answer's sender dropped), and an abort, each
    /// ending the run with a reason instead of a hang.
    #[tokio::test]
    async fn a_run_always_ends_with_an_answer() {
        let build = |name: &str| std::thread::Builder::new().name(name.to_owned());
        let idle = || async { std::future::pending::<RunEnd>().await };

        let mut run = Running::Run(
            Run::spawn_with(build("no-runtime"), runtime_fails, idle()).expect("started"),
        );
        run.wait().await;
        assert_end(&run, "the runtime of the crun thread: no epoll");
        run.reap(Duration::from_secs(5), Duration::from_millis(1))
            .expect("gone");

        let too_big = std::thread::Builder::new().stack_size(usize::MAX / 2);
        let refused = Run::spawn_with(too_big, one_thread_runtime, idle()).err();
        assert!(refused.is_some_and(|e| e.starts_with("start the crun thread: ")));

        let mut run = Running::Run(
            Run::spawn_with(build("panics"), one_thread_runtime, async {
                panic!("crun task panicked");
                #[allow(unreachable_code)]
                RunEnd::Exited(0)
            })
            .expect("started"),
        );
        run.wait().await;
        assert_end(&run, "the crun thread ended without an answer");
        run.reap(Duration::from_secs(5), Duration::from_millis(1))
            .expect("gone");

        let mut run = Running::Run(
            Run::spawn_with(build("aborted"), one_thread_runtime, idle()).expect("started"),
        );
        assert!(run.needs_cgroup_kill());
        run.force().await;
        assert_end(&run, "crun run was aborted");
        run.reap(Duration::from_secs(5), Duration::from_millis(1))
            .expect("gone");
    }

    fn assert_end(run: &Running, says: &str) {
        let Running::Run(run) = run else {
            panic!("not a run");
        };
        let Some(RunEnd::Failed(why)) = run.end() else {
            panic!("{:?}", run.end());
        };
        assert_eq!(why, says);
    }

    /// Catches a clean that gives up on a `crun` it cannot stop without saying so: a
    /// run stuck in a blocking call outlasts the limit, is aborted at it, and still
    /// outlasts the second wait, which is an error naming the limit.
    #[test]
    fn a_run_that_will_not_end_is_an_error_after_the_limit() {
        let mut run = Running::Run(
            Run::spawn_with(std::thread::Builder::new(), one_thread_runtime, async {
                // Blocks the thread: an abort cannot get in.
                std::thread::sleep(Duration::from_millis(600));
                RunEnd::Exited(0)
            })
            .expect("started"),
        );
        let why = run
            .reap(Duration::from_millis(100), Duration::from_millis(5))
            .expect_err("still running");
        assert!(why.contains("still runs"), "{why}");
        // It is waited for: the next reap finds the thread gone or errors the same way.
        std::thread::sleep(Duration::from_millis(800));
    }
}
