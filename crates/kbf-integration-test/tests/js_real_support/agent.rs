//! The pinned `js-agent`, started as root on this machine over a node made for one test, with
//! the stand-in programs of `kbf_integration_test::js_stand_in` in place of `crun`, `ip`, `nft`
//! and `nsenter`.
//!
//! **Why root, and what it touches.** The released agent runs jobs with `--runtime crun` only
//! as root, and checks the host before it registers: a job user with subordinate ids,
//! `newuidmap` and `newgidmap`, a cgroup version 2 subtree delegated to it, a state root and a
//! scratch root owned by root under directories only root can write, and (where the kernel
//! restricts unprivileged user namespaces) an AppArmor profile that names its `crun`. So a
//! [`Node`] is two directories: one under `CARGO_TARGET_TMPDIR` (the stand-in links, their
//! directory, the state root, the join token) and one under `/run` (the scratch root, the image
//! store beside it, and the run-files root, which must be on a tmpfs); both are removed when
//! the node drops. The agent is started by `systemd-run --scope -p Delegate=yes` in a scope of
//! its own, which it delegates to itself, and the scope's processes are killed when the agent
//! drops. The profile, when the kernel needs one, names the stand-in `crun` only, and is
//! unloaded when the node drops. The job user is the user who ran `sudo` (`SUDO_USER`,
//! `SUDO_UID`, `SUDO_GID`, which sudo sets).
//!
//! **No network is touched.** The agent is started with egress `allow-list` (the executor's
//! jobs ask for it), whose wiring runs the `ip`, `nft` and `nsenter` it is given: the
//! stand-ins, which record and change nothing. The one host setting it writes is IPv4
//! forwarding, which it turns on (hosted runners have it on already). The container's first
//! process, as `crun state` names it, is a process the node keeps in a network namespace of its
//! own (`unshare --net`), so the wiring has a namespace that is not the host's.
//!
//! **The image.** The job's image is put in the agent's store before it starts (a rootfs
//! directory and a config), so nothing is pulled.
//!
//! **Signals, not clocks.** Each container connects to the node's listener when it starts and
//! stays connected while it runs ([`Node::next`]); the agent's registration is a line it
//! prints. A test waits on those and on job states, never on time.

use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_testkit::dir::TestDir;
use tokio::io::AsyncBufReadExt as _;
use tokio::net::UnixListener;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use super::{JOIN, RealJobServer, program};

/// The digest of the one image the node's store holds.
pub const IMAGE_HEX: &str = "4b6266a7e1a5cbd7e1f5c3f8f1c2e3d4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0";

/// The image every job of the node names: the one its store holds.
pub fn image() -> String {
    format!("registry.test/kbf-task@sha256:{IMAGE_HEX}")
}

/// The user jobs run as: the one who ran `sudo`.
#[derive(Clone, Debug)]
pub struct JobUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

impl JobUser {
    /// The user sudo names, refused when the test does not run as root under sudo.
    pub fn of_sudo() -> JobUser {
        let root = rustix::process::geteuid().is_root();
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        let (name, uid, gid) = (var("SUDO_USER"), var("SUDO_UID"), var("SUDO_GID"));
        match (root, uid.parse(), gid.parse()) {
            (true, Ok(uid @ 1..), Ok(gid @ 1..)) if !name.is_empty() => JobUser { name, uid, gid },
            _ => panic!(
                "the pinned js-agent runs jobs only as root, as the user who ran sudo: run this \
                 test with `cargo xtask sudo-test -p kbf-integration-test --test \
                 executor_real_service_integration` (root {root}, SUDO_USER {name:?}, SUDO_UID \
                 {uid:?}, SUDO_GID {gid:?})"
            ),
        }
    }
}

/// A child process that is killed and reaped when this drops.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What a container told the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The container `name` started with `args` (the image's entrypoint and the job's
    /// arguments, after the agent's init).
    Started { name: String, args: Vec<String> },
    /// The container `name` ended.
    Ended { name: String },
}

impl Event {
    /// The container's name.
    pub fn name(&self) -> &str {
        match self {
            Event::Started { name, .. } | Event::Ended { name } => name,
        }
    }
}

fn mkdir(path: &Path, mode: u32) -> PathBuf {
    std::fs::create_dir_all(path).unwrap_or_else(|e| panic!("mkdir {}: {e}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
    path.to_owned()
}

fn chown(path: &Path, user: &JobUser) {
    std::os::unix::fs::chown(path, Some(user.uid), Some(user.gid))
        .unwrap_or_else(|e| panic!("chown {}: {e}", path.display()));
}

/// Runs `program` with `args` and panics, with what it said, unless it succeeds.
fn must(program: &str, args: &[&std::ffi::OsStr]) {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The files and processes one agent runs over.
pub struct Node {
    /// The agent's host id, the node's name.
    pub host: String,
    pub user: JobUser,
    /// The stand-in links (`crun`, `ip`, `nft`, `nsenter`).
    pub bin: PathBuf,
    /// The stand-ins' directory (`js_stand_in::Place`).
    pub stand_in: PathBuf,
    pub state_root: PathBuf,
    pub scratch_root: PathBuf,
    pub run_files: PathBuf,
    pub join: PathBuf,
    events: AsyncMutex<mpsc::UnboundedReceiver<Event>>,
    accepting: tokio::task::JoinHandle<()>,
    /// The AppArmor profile loaded for the stand-in `crun`, unloaded at drop.
    profile: Option<PathBuf>,
    _holder: Reaped,
    // Dropped after the processes: the directories go last.
    _run: TestDir,
    _test: TestDir,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.accepting.abort();
        if let Some(profile) = &self.profile {
            let _ = Command::new("/usr/sbin/apparmor_parser")
                .arg("-R")
                .arg(profile)
                .output();
        }
    }
}

impl Node {
    /// A node named after `name`, for jobs run as the user who ran sudo.
    pub async fn new(name: &str) -> Node {
        let user = JobUser::of_sudo();
        let test = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let run = TestDir::under("/run", name);
        let host = run
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.to_owned());
        mkdir(test.path(), 0o755);
        mkdir(run.path(), 0o755);

        let bin = mkdir(&test.path().join("bin"), 0o755);
        let stand_in_exe = Path::new(env!("CARGO_BIN_EXE_kbf-js-stand-in"));
        for name in ["crun", "ip", "nft", "nsenter"] {
            // A link, not a copy: the test never holds the program open for writing, so it
            // cannot be busy when the agent runs it.
            std::fs::hard_link(stand_in_exe, bin.join(name))
                .unwrap_or_else(|e| panic!("link {name}: {e}"));
        }
        let stand_in = mkdir(&bin.join("stand-in"), 0o755);
        mkdir(&stand_in.join("plan"), 0o755);
        for dir in [&stand_in, &stand_in.join("plan")] {
            chown(dir, &user);
        }
        let calls = stand_in.join("calls");
        std::fs::write(&calls, "").expect("the calls file");
        std::fs::set_permissions(&calls, std::fs::Permissions::from_mode(0o666)).expect("chmod");

        let state_root = mkdir(&test.path().join("state"), 0o700);
        let join = test.path().join("join");
        std::fs::write(&join, JOIN).expect("the join token");
        std::fs::set_permissions(&join, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        let scratch = mkdir(&run.path().join("scratch"), 0o755);
        let scratch_root = mkdir(&scratch.join("runs"), 0o711);
        let run_files = mkdir(&run.path().join("files"), 0o711);
        let store = scratch.join("oci");
        mkdir(
            &store.join("oci-images").join(IMAGE_HEX).join("rootfs"),
            0o755,
        );
        let configs = mkdir(&store.join("oci-configs"), 0o755);
        let config = r#"{"config":{"Entrypoint":["/bin/task"],"Env":[]},"rootfs":{"diff_ids":[]}}"#;
        std::fs::write(configs.join(format!("{IMAGE_HEX}.json")), config).expect("the config");

        let profile = Node::allow_crun(&bin.join("crun"), test.path());
        let holder = Node::hold_a_network_namespace(&stand_in);
        let (accepting, events) = Node::listen(&stand_in, run.path());
        Node {
            host,
            user,
            bin,
            stand_in,
            state_root,
            scratch_root,
            run_files,
            join,
            events: AsyncMutex::new(events),
            accepting,
            profile,
            _holder: holder,
            _run: run,
            _test: test,
        }
    }

    /// Where the kernel restricts unprivileged user namespaces, loads a profile that names the
    /// stand-in `crun` (the agent refuses a `crun` no profile covers); the file, to unload.
    fn allow_crun(crun: &Path, dir: &Path) -> Option<PathBuf> {
        let switch = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
        let restricted = std::fs::read_to_string(switch).unwrap_or_default();
        if restricted.trim() != "1" {
            return None;
        }
        let crun = std::fs::canonicalize(crun).expect("the stand-in crun");
        let file = dir.join("crun.apparmor");
        let text = format!(
            "abi <abi/4.0>,\nprofile {path} {path} flags=(unconfined) {{\n  userns,\n}}\n",
            path = crun.display()
        );
        std::fs::write(&file, text).expect("the profile");
        must(
            "/usr/sbin/apparmor_parser",
            &["-r".as_ref(), file.as_os_str()],
        );
        Some(file)
    }

    /// A process in a network namespace of its own, whose id `crun state` names; returns once
    /// it is in that namespace.
    fn hold_a_network_namespace(stand_in: &Path) -> Reaped {
        let child = Command::new("/usr/bin/unshare")
            .args(["--net", "--", "/usr/bin/sleep", "infinity"])
            .stdin(Stdio::null())
            .spawn()
            .expect("unshare --net");
        let pid = child.id();
        let holder = Reaped(child);
        let own = std::fs::read_link("/proc/self/ns/net").expect("this namespace");
        // Until unshare has made the namespace (it does before it runs the program).
        while std::fs::read_link(format!("/proc/{pid}/ns/net"))
            .ok()
            .as_ref()
            == Some(&own)
        {
            std::thread::yield_now();
        }
        std::fs::write(stand_in.join("netns-pid"), pid.to_string()).expect("netns-pid");
        holder
    }

    /// The listener the containers connect to, in `short` (a socket's path is short) with a
    /// link to it in `stand_in`, and the events it hears.
    fn listen(
        stand_in: &Path,
        short: &Path,
    ) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedReceiver<Event>) {
        let path = short.join("events.sock");
        let listener = UnixListener::bind(&path).expect("events.sock");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        std::os::unix::fs::symlink(&path, stand_in.join("events.sock")).expect("the link");
        let (send, events) = mpsc::unbounded_channel();
        let accepting = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let send = send.clone();
                tokio::spawn(async move {
                    let mut stream = tokio::io::BufReader::new(stream);
                    let mut line = String::new();
                    let _ = stream.read_line(&mut line).await;
                    let mut words = line.trim_end_matches('\n').split('\t');
                    let _start = words.next();
                    let name = words.next().unwrap_or_default().to_owned();
                    let args = words.map(str::to_owned).collect();
                    let _ = send.send(Event::Started {
                        name: name.clone(),
                        args,
                    });
                    // Until the container's end closes the connection.
                    let _ = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
                    let _ = send.send(Event::Ended { name });
                });
            }
        });
        (accepting, events)
    }

    /// The next event of any container.
    pub async fn next(&self) -> Event {
        self.events
            .lock()
            .await
            .recv()
            .await
            .expect("the listener runs while the node lives")
    }

    /// The next event of any container, if one has come.
    pub fn try_next(&self) -> Option<Event> {
        self.events.try_lock().ok()?.try_recv().ok()
    }

    /// The action whose digest has hash `hash` runs as `plan` says (`exit <n>` or `hold`).
    pub fn plan(&self, hash: &str, plan: &str) {
        let file = self.stand_in.join("plan").join(hash);
        std::fs::write(&file, plan).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    }

    /// Every call the stand-ins recorded.
    pub fn calls(&self) -> String {
        std::fs::read_to_string(self.stand_in.join("calls")).unwrap_or_default()
    }
}

/// A `js-agent` of the pinned release, registered with a server, until it drops.
pub struct RealAgent {
    child: Option<Reaped>,
    log: Arc<Mutex<String>>,
    _dir: TestDir,
}

impl Drop for RealAgent {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        // The scope the agent delegated to itself: its supervisors outlive it.
        let cgroup = std::fs::read_to_string(format!("/proc/{}/cgroup", child.0.id()));
        drop(child);
        let scope = cgroup.unwrap_or_default();
        let scope = scope
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .map(|path| path.trim_end_matches("/supervisor").to_owned());
        if let Some(scope) = scope.filter(|scope| scope.len() > 1) {
            let kill = Path::new("/sys/fs/cgroup")
                .join(scope.trim_start_matches('/'))
                .join("cgroup.kill");
            let _ = std::fs::write(kill, "1");
        }
    }
}

impl RealAgent {
    /// Starts the pinned agent over `node`, joined to `server`'s pool, and returns once it has
    /// registered.
    pub async fn start(node: &Node, server: &RealJobServer) -> RealAgent {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("{}-agent", node.host));
        let binary = program("js-agent", &dir);
        let path = |p: &Path| p.display().to_string();
        let server_url = format!("http://{}", server.lease_addr());
        let unit = format!("{}-agent", node.host);
        let mut args: Vec<String> = [
            "--scope",
            "--quiet",
            "--collect",
            "--unit",
            &unit,
            "-p",
            "Delegate=yes",
            "--",
        ]
        .map(str::to_owned)
        .into();
        args.push(path(&binary));
        let flags = [
            ("--server", server_url),
            ("--pool", "kbf-tests".to_owned()),
            ("--token-file", path(&node.join)),
            ("--host-id", node.host.clone()),
            ("--runtime", "crun".to_owned()),
            ("--job-user", node.user.name.clone()),
            ("--crun-bin", path(&node.bin.join("crun"))),
            ("--state-root", path(&node.state_root)),
            ("--scratch-root", path(&node.scratch_root)),
            ("--run-files-root", path(&node.run_files)),
            ("--stop-grace", "0".to_owned()),
            ("--egress", "allow-list".to_owned()),
            // A documentation address: an allow-list needs one entry, and no job reaches it.
            ("--egress-allow", "192.0.2.1:443".to_owned()),
            ("--egress-ip", path(&node.bin.join("ip"))),
            ("--egress-nft", path(&node.bin.join("nft"))),
            ("--egress-nsenter", path(&node.bin.join("nsenter"))),
        ];
        for (flag, value) in flags {
            args.push(flag.to_owned());
            args.push(value);
        }
        let child = Command::new("/usr/bin/systemd-run")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("systemd-run {}: {e}", binary.display()));
        let mut child = Reaped(child);
        let stderr = child.0.stderr.take().expect("piped stderr");
        let log = Arc::new(Mutex::new(String::new()));
        let kept = Arc::clone(&log);
        let (registered, said) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let mut registered = Some(registered);
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                kept.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push_str(&format!("{line}\n"));
                if line.contains("registered, epoch") {
                    registered.take().map(|tell| tell.send(()));
                }
            }
        });
        if said.await.is_err() {
            let status = child.0.wait();
            let log = log.lock().unwrap_or_else(PoisonError::into_inner);
            panic!("js-agent did not register ({status:?}); its log:\n{log}");
        }
        RealAgent {
            child: Some(child),
            log,
            _dir: dir,
        }
    }

    /// What the agent has logged so far.
    pub fn log(&self) -> String {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}
