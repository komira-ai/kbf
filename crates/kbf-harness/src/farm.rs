//! The farm of real `kbf` processes: a frontend, `storage_nodes` storage nodes (each
//! with a daemon in the same process, or storage alone), `daemons` plain daemons (and,
//! when asked, an Envoy in front of the frontend), on loopback addresses of their own,
//! each started from the flags `kbf-deploy` renders into its unit
//! (`kbf_deploy::linux::modes::render`, the `ExecStart=` line, nothing written by hand),
//! under certificates of the rows the farm's certificate table has.
//!
//! A role starts only from its rendered unit line, and the [`Farm`] keeps that line, so
//! [`Farm::restart`] runs the identical command and a test that changes the line
//! (a rolling update) changes it in one place. The deviations from the rendered units
//! are listed in [`Farm::start`].

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kbf_deploy::linux::Mode;
use kbf_deploy::linux::verify_cert::Row;
use serde::Serialize;

use crate::api;
use crate::envoy::{self, BoxPorts};
use crate::node_support::{cgroup_tree, files_holding, id_files, image_path, podman};
use crate::pki::Pki;
use crate::procs::{HANG, Proc, Spec, field};
use crate::units::{Rendered, config, rendered};

/// The binaries a farm runs, by path (a library cannot use `CARGO_BIN_EXE_*`).
#[derive(Clone, Debug)]
pub struct Bins {
    /// `kbf`.
    pub kbf: PathBuf,
    /// The stand-in `podman`, for [`Driver::StandInContainer`].
    pub fake_podman: Option<PathBuf>,
}

/// What runs the actions on the daemons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Driver {
    /// `--driver=native`: each action is a plain process in its lease directory. No
    /// container path is exercised; any argv a build sends runs as sent.
    Native,
    /// `--driver=container` as rendered, over the stand-in `podman` (which runs one
    /// planted argv whatever the lease asks for), a planted cgroup tree and id files.
    StandInContainer,
}

/// A role of the farm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum Role {
    Frontend,
    /// A storage node (with its daemon, in a [`NodeMode::StorageAndDaemon`] farm),
    /// numbered from 1.
    Node(usize),
    /// A plain daemon, numbered from 1.
    Daemon(usize),
    Envoy,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Frontend => f.write_str("frontend"),
            Role::Node(n) => write!(f, "node-{n}"),
            Role::Daemon(n) => f.write_str(&daemon_id(*n)),
            Role::Envoy => f.write_str("envoy"),
        }
    }
}

/// The node id of plain daemon `n` (from 1).
#[must_use]
pub fn daemon_id(n: usize) -> String {
    format!("daemon-{n}")
}

/// What a storage node runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeMode {
    /// `kbf storage`: storage alone. Its actions run on the plain daemons.
    Storage,
    /// `kbf storage-and-daemon`: storage and a daemon in one process. The daemon half
    /// runs the container driver only.
    StorageAndDaemon,
}

/// The ceilings that keep the farm small enough for a hosted runner (about 4 vCPU and
/// 16 GB), as extra flags per kind of process, given to the unit's own flags with
/// [`Rendered::set`] (so they replace a rendered value and are added when there is none).
#[derive(Clone, Debug, Default)]
pub struct Limits {
    pub frontend: Vec<String>,
    pub node: Vec<String>,
    pub daemon: Vec<String>,
}

impl Limits {
    /// No limits: the unit's flags and the binaries' defaults.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The limits of a hosted runner: a small hot tier and a small write buffer on each
    /// storage node (the stores and the write tier are a storage node's: a frontend refuses
    /// these flags), and one CPU kept by each daemon.
    #[must_use]
    pub fn small() -> Self {
        Self {
            frontend: Vec::new(),
            node: vec![
                format!("--hot-store-max-bytes={}", 256 << 20),
                format!("--write-bytes-in-flight={}", 128 << 20),
            ],
            // A daemon keeps 8 CPUs for the farm's processes unless told otherwise, and
            // leases the rest: none on a runner of 4.
            daemon: vec!["--reserve-cpus=1".to_owned()],
        }
    }
}

/// What a farm is made of.
#[derive(Clone, Debug)]
pub struct Topology {
    /// How many storage nodes there are (the farm's replication needs two).
    pub storage_nodes: usize,
    pub node_mode: NodeMode,
    /// How many plain daemons there are.
    pub daemons: usize,
    pub driver: Driver,
    pub limits: Limits,
    /// An Envoy to put in front of the frontend (the `box` topology, the frontend as
    /// instance `a`): the binary's path. `None` runs the frontend alone.
    pub envoy: Option<PathBuf>,
    /// Flags to set on one role after everything else (a mutant's, or a test's).
    pub overrides: Vec<(Role, Vec<String>)>,
}

impl Topology {
    /// The rehearsal's: `storage_nodes` storage-and-daemon nodes and a daemon, the
    /// stand-in container driver, no limits, no Envoy.
    #[must_use]
    pub fn rehearsal(storage_nodes: usize) -> Self {
        Self {
            storage_nodes,
            node_mode: NodeMode::StorageAndDaemon,
            daemons: 1,
            driver: Driver::StandInContainer,
            limits: Limits::none(),
            envoy: None,
            overrides: Vec::new(),
        }
    }

    /// The beta farm's: three storage nodes, two plain daemons running actions as plain
    /// processes (the native driver, which only a plain daemon takes), a hosted runner's
    /// limits, and the Envoy given.
    #[must_use]
    pub fn beta(envoy: Option<PathBuf>) -> Self {
        Self {
            storage_nodes: 3,
            node_mode: NodeMode::Storage,
            daemons: 2,
            driver: Driver::Native,
            limits: Limits::small(),
            envoy,
            overrides: Vec::new(),
        }
    }
}

/// Why a farm did not start or become ready: what was waited for, and every process's
/// state and output.
#[derive(Debug, thiserror::Error)]
#[error("{what}\n{report}")]
pub struct FarmError {
    pub what: String,
    pub report: String,
}

/// One start of one role, for the result document: the stand-in for a boot counter.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProcRecord {
    pub role: String,
    pub pid: u32,
    /// 0 for the first start of the role, 1 for the first restart.
    pub generation: usize,
    /// Seconds since the Unix epoch.
    pub started: u64,
    /// How it ended (`exit 0`, `signal 9`, ...); `None` while it runs.
    pub ended: Option<String>,
}

struct Member {
    role: Role,
    spec: Spec,
    root: PathBuf,
    proc: Proc,
    generation: usize,
    started: u64,
    /// SIGKILLed by the harness: [`Farm::wait`] does not take its exit for a crash.
    killed: bool,
    past: Vec<ProcRecord>,
}

/// The ports the processes are told.
struct Wiring {
    reapi: u16,
    /// The frontend's worker listener.
    worker: u16,
    api: u16,
    /// The worker port the daemons dial: the frontend's own, or Envoy's.
    node_worker: u16,
    /// The storage URL the frontend dials: node 1's.
    storage_url: String,
}

/// The running farm.
pub struct Farm {
    logs: PathBuf,
    members: Vec<Member>,
    /// The frontend's REAPI listener.
    reapi: String,
    /// What a client dials: Envoy's REAPI port when it runs, else the frontend's.
    client_port: u16,
    api: u16,
    ca: Vec<u8>,
    /// The image the actions name: what the stand-in `podman` serves.
    image: String,
    storage_nodes: usize,
    /// The ids the frontend must list as connected, sorted.
    node_ids: Vec<String>,
    /// Envoy's ports, when it runs.
    envoy_ports: Option<BoxPorts>,
    /// How long [`Farm::try_wait`] and [`Farm::stop_all`] wait before giving up.
    hang: Duration,
}

/// The node id of storage-and-daemon node `n` (from 1).
#[must_use]
pub fn node_id(n: usize) -> String {
    format!("node-{n}")
}

/// A loopback address of its own for each process (the whole of `127/8` is loopback on
/// Linux): the frontend's, then each node's, then the plain daemon's.
fn loopback(n: usize) -> String {
    format!("127.0.0.{}", n + 1)
}

/// `n` distinct ports nothing listens on now.
fn ports(n: usize) -> Vec<u16> {
    let held: Vec<TcpListener> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("bind"))
        .collect();
    held.iter()
        .map(|l| l.local_addr().expect("addr").port())
        .collect()
}

/// A base port such that the box topology's six instance ports (`base..base+3` and
/// `base+10..base+13`) are free on loopback now, below the range the kernel hands out
/// to connections.
fn backend_base() -> u16 {
    let first = 12_000 + u16::try_from(std::process::id() % 4000).expect("fits") * 4;
    let free = |base: u16| {
        [0_u16, 1, 2, 10, 11, 12]
            .iter()
            .all(|k| TcpListener::bind(("127.0.0.1", base + k)).is_ok())
    };
    (0..200)
        .map(|i| first + 20 * i)
        .find(|base| free(*base))
        .expect("no free run of ports for the box instances")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The settings of one process: its own address, node id, directories under `root` and
/// storage port, and the farm's wiring. The frontend is one name for every dialler
/// (`KBF_FRONTEND_NAME`), not the process's own (`KBF_HOST_NAME`).
fn settings(
    root: &Path,
    host: &str,
    node_id: &str,
    storage_port: u16,
    worker_port: u16,
    wiring: &Wiring,
    image: &str,
) -> kbf_deploy::linux::LinuxConfig {
    let at = |name: &str| root.join(name).display().to_string();
    config(&[
        ("KBF_HOST_NAME", host.to_owned()),
        ("KBF_FRONTEND_NAME", "localhost".to_owned()),
        ("KBF_LISTEN_ADDRESS", host.to_owned()),
        ("KBF_NODE_ID", node_id.to_owned()),
        // Actions get RAM less the reserved shares: 10 GiB, over the 4 GiB an action asks by default.
        ("KBF_RAM_GIB", "16".to_owned()),
        ("MINIO_RESERVATION_GIB", "1".to_owned()),
        ("KBF_SERVER_RESERVATION_GIB", "1".to_owned()),
        ("KBF_OS_HEADROOM_GIB", "4".to_owned()),
        ("KBF_STORAGE_URL", wiring.storage_url.clone()),
        ("KBF_REAPI_PORT", wiring.reapi.to_string()),
        ("KBF_WORKER_PORT", worker_port.to_string()),
        ("KBF_API_PORT", wiring.api.to_string()),
        ("KBF_STORAGE_PORT", storage_port.to_string()),
        ("KBF_ETC", at("etc")),
        ("KBF_DATA_DIR", at("data")),
        ("KBF_META_DIR", at("meta")),
        ("KBF_HOT_DIR", at("hot")),
        ("KBF_STATE_DIR", at("state")),
        ("KBF_IMAGES", image.to_owned()),
    ])
}

/// The directories a process's unit expects to exist (the installer makes them on a
/// host).
fn make_dirs(root: &Path) -> PathBuf {
    for name in ["etc", "data", "meta", "hot", "state", "cold", "bin"] {
        std::fs::create_dir_all(root.join(name)).expect("mkdir");
    }
    root.join("bin")
}

/// A number of `dir`'s path: farms started in one process (tests run side by side) have
/// directories of their own, so each gets a control socket of its own.
fn path_tag(dir: &Path) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (dir, std::process::id()).hash(&mut hasher);
    hasher.finish()
}

impl Farm {
    /// Renders the units and starts a process from each, then waits for each process's
    /// start line. `dir` holds every process's directories (and must be fresh); `logs`
    /// their output.
    ///
    /// The flags a test box cannot take as rendered, and what each is changed to:
    /// - the nodes' `--cold-store=s3://...` (no MinIO here) is a `file://` directory;
    /// - a `kbf storage` node (which has no daemon half) is given `--frontend` and
    ///   `--advertise-addr`, which its unit does not render (the storage-and-daemon
    ///   unit renders them), so that it registers with the frontend, and with the
    ///   frontend itself even when Envoy runs (a node serves nothing until it has
    ///   registered, the frontend is not ready until a node serves, and Envoy sends
    ///   nothing to a frontend that is not ready: through Envoy no node could register at
    ///   a cold start);
    /// - with [`Driver::StandInContainer`], `--driver=container` stays, over the stand-in
    ///   `podman`: the daemons get it on `PATH`, their cgroup tree (`--cgroup-root`,
    ///   `--self-cgroup`) is planted and `--id-files` stands for `/etc/subuid`;
    /// - with [`Driver::Native`], `--driver=native` replaces it and `KBF_IMAGES` is empty
    ///   (only the container driver takes `--image`);
    /// - the frontend's control socket is in the temporary directory (a `RuntimeDirectory=`
    ///   is systemd's, and a scratch path is too long for a socket's);
    /// - the topology's [`Limits`] and overrides are set on top.
    ///
    /// Every other flag is the unit's.
    ///
    /// # Errors
    /// A process that exits, or does not print its start line within [`HANG`].
    ///
    /// # Panics
    /// When the box cannot be set up (a certificate `verify-cert` refuses, a config
    /// `kbf-deploy` refuses): a defect of the harness.
    pub fn start(logs: &Path, dir: &Path, bins: &Bins, topo: &Topology) -> Result<Self, FarmError> {
        std::fs::create_dir_all(logs).expect("logs");
        let native = topo.driver == Driver::Native;
        // The native driver is the plain daemon's: `kbf storage-and-daemon` runs the container
        // driver only, so nodes with a daemon half go with the container driver and storage
        // nodes with the native one.
        assert_eq!(
            native,
            topo.node_mode == NodeMode::Storage,
            "the native driver needs NodeMode::Storage nodes and the container driver \
             NodeMode::StorageAndDaemon ones"
        );
        let uid = rustix::process::getuid().as_raw();
        let nodes = topo.storage_nodes;
        let p = ports(nodes + 1);
        let (storage, spare) = (&p[..nodes], p[nodes]);
        let box_ports = topo.envoy.is_some().then(|| {
            let p = ports(3);
            BoxPorts {
                base: backend_base(),
                reapi: p[0],
                worker: p[1],
                admin: p[2],
            }
        });
        let front = match &box_ports {
            Some(b) => (b.base, b.base + 1, b.base + 2),
            None => {
                let p = ports(3);
                (p[0], p[1], p[2])
            }
        };
        let wiring = Wiring {
            reapi: front.0,
            worker: front.1,
            api: front.2,
            node_worker: box_ports.as_ref().map_or(front.1, |b| b.worker),
            storage_url: format!("https://{}:{}", loopback(1), storage[0]),
        };
        let pki = Pki::new();
        let steps = ["write-lines", "$UPPER/out/file.txt", "a file"];
        // The nodes that have a daemon half connect to the frontend as nodes.
        let mut node_ids: Vec<String> = (1..=topo.daemons).map(daemon_id).collect();
        if topo.node_mode == NodeMode::StorageAndDaemon {
            node_ids.extend((1..=nodes).map(node_id));
        }
        node_ids.sort();
        let mut farm = Farm {
            logs: logs.to_owned(),
            members: Vec::new(),
            reapi: String::new(),
            client_port: box_ports.as_ref().map_or(front.0, |b| b.reapi),
            api: wiring.api,
            ca: Vec::new(),
            image: String::new(),
            storage_nodes: nodes,
            node_ids,
            envoy_ports: None,
            hang: HANG,
        };
        let overrides = |role: Role, unit: Rendered| {
            let flags: Vec<String> = topo
                .overrides
                .iter()
                .filter(|(r, _)| *r == role)
                .flat_map(|(_, f)| f.iter().cloned())
                .collect();
            unit.set(&flags)
        };
        let fake = || {
            bins.fake_podman
                .as_ref()
                .expect("Bins::fake_podman for the stand-in container driver")
                .display()
                .to_string()
        };
        // The stand-in `podman`'s store in `bin`, and the image it serves, bare.
        let stand_in = |farm: &mut Farm, bin: &Path| {
            if native {
                return String::new();
            }
            let image = podman(bin, &fake(), &steps);
            let bare = image
                .strip_prefix("docker://")
                .expect("docker://")
                .to_owned();
            farm.image = image;
            bare
        };

        // The frontend: its unit as rendered, a state directory made.
        let root = dir.join("frontend");
        let etc = root.join("etc");
        make_dirs(&root);
        pki.write_ca(&etc);
        pki.issue(&etc, "worker", Row::FrontendWorker, &["localhost"]);
        pki.issue_reapi(&etc, &["localhost"]);
        // The unit names `--api-token-file <etc>/api-token`; the harness's API calls only read.
        let token = etc.join("api-token");
        std::fs::write(&token, "rehearsal-api-token-of-32-or-more-visible-chars\n")
            .expect("write the API token");
        std::fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("the API token's mode");
        farm.ca = std::fs::read(etc.join("ca.pem")).expect("ca");
        let cfg = settings(
            &root,
            &loopback(0),
            "frontend-1",
            spare,
            wiring.worker,
            &wiring,
            "",
        );
        let unit = rendered(&cfg, uid, Mode::Frontend);
        let socket = std::env::temp_dir().join(format!("kbf-farm-{:x}.sock", path_tag(dir)));
        let _ = std::fs::remove_file(&socket);
        let control = format!("--handover-socket={}", socket.display());
        let unit = overrides(
            Role::Frontend,
            unit.replace(&[control]).set(&topo.limits.frontend),
        );
        farm.launch(&bins.kbf, Role::Frontend, &root, unit, None);
        let line = farm.start_line(Role::Frontend, "kbf-frontend ")?;
        farm.reapi = field(&line, "reapi=").to_owned();

        // Envoy, before any node: the nodes and daemons dial the frontend through it.
        if let (Some(binary), Some(ports)) = (&topo.envoy, &box_ports) {
            let eroot = dir.join("envoy");
            std::fs::create_dir_all(&eroot).expect("mkdir");
            let spec = envoy::launch(binary, &eroot, &frontend_etc(dir), ports);
            farm.launch_spec(Role::Envoy, &eroot, spec);
            farm.envoy_ports = Some(*ports);
            let admin = ports.admin;
            farm.try_wait("Envoy's admin /ready", |_| {
                (api::get(admin, "/ready")?.0 == 200).then_some(())
            })?;
        }

        // The storage nodes, then the plain daemons.
        for (i, port) in storage.iter().enumerate() {
            let (id, host) = (node_id(i + 1), loopback(i + 1));
            let id = id.as_str();
            let root = dir.join(id);
            let bin = make_dirs(&root);
            let etc = root.join("etc");
            pki.write_ca(&etc);
            pki.issue(&etc, "storage", Row::Storage, &[&host]);
            if !native {
                pki.issue(&etc, "node", Row::Daemon, &[id]);
            }
            let bare = if native {
                String::new()
            } else {
                stand_in(&mut farm, &bin)
            };
            let cfg = settings(&root, &host, id, *port, wiring.node_worker, &wiring, &bare);
            let cold = format!("--cold-store=file://{}", root.join("cold").display());
            let mode = match topo.node_mode {
                NodeMode::Storage => Mode::Storage,
                NodeMode::StorageAndDaemon => Mode::StorageAndDaemon,
            };
            let unit = rendered(&cfg, uid, mode).replace(&[cold]);
            let unit = if native {
                // The `kbf storage` unit renders no registration flags (only the
                // storage-and-daemon unit does), so a storage node of it is a farm of one
                // member that no frontend lists: these two make it join as the other does.
                // It registers with the frontend itself, not through Envoy: a node
                // serves nothing until it has registered, the frontend is not ready
                // until a node serves, and Envoy sends nothing to an unready frontend.
                unit.append(&[
                    format!("--frontend=https://localhost:{}", wiring.worker),
                    format!("--advertise-addr={host}:{port}"),
                ])
            } else {
                Self::driven(unit, topo.driver)
            };
            let unit = overrides(Role::Node(i + 1), unit.set(&topo.limits.node));
            farm.launch(
                &bins.kbf,
                Role::Node(i + 1),
                &root,
                unit,
                (!native).then_some(bin.as_path()),
            );
            farm.start_line(Role::Node(i + 1), "kbf-storage ")?;
        }
        for d in 1..=topo.daemons {
            let (id, host) = (daemon_id(d), loopback(nodes + d));
            let root = dir.join(&id);
            let bin = make_dirs(&root);
            let etc = root.join("etc");
            pki.write_ca(&etc);
            pki.issue(&etc, "node", Row::Daemon, &[&id]);
            let bare = stand_in(&mut farm, &bin);
            let cfg = settings(&root, &host, &id, spare, wiring.node_worker, &wiring, &bare);
            let unit = Self::driven(rendered(&cfg, uid, Mode::Daemon), topo.driver)
                .set(&topo.limits.daemon);
            let unit = overrides(Role::Daemon(d), unit);
            farm.launch(
                &bins.kbf,
                Role::Daemon(d),
                &root,
                unit,
                (!native).then_some(bin.as_path()),
            );
        }

        Ok(farm)
    }

    /// A farm of arbitrary processes under the supervisor (`kill`, `term`, `restart`, the
    /// process table, `wait` and the hang report), with no `kbf` among them: for the
    /// supervisor's own tests, and for a scenario that supervises a helper.
    ///
    /// # Panics
    /// When a process cannot be spawned.
    #[must_use]
    pub fn supervise(logs: &Path, specs: Vec<(Role, Spec)>) -> Self {
        std::fs::create_dir_all(logs).expect("logs");
        let mut farm = Farm {
            logs: logs.to_owned(),
            members: Vec::new(),
            reapi: String::new(),
            client_port: 0,
            api: 0,
            ca: Vec::new(),
            image: String::new(),
            storage_nodes: 0,
            node_ids: Vec::new(),
            envoy_ports: None,
            hang: HANG,
        };
        for (role, spec) in specs {
            let root = spec.cwd.clone();
            farm.launch_spec(role, &root, spec);
        }
        farm
    }

    /// The farm with `hang` as how long it waits before giving up (a test's, shorter than
    /// [`HANG`]).
    #[must_use]
    pub fn with_hang(mut self, hang: Duration) -> Self {
        self.hang = hang;
        self
    }

    /// `unit` for `driver`: the container driver is the unit's own; the native driver
    /// replaces it (and the container-only `--image`, which the example settings leave out).
    fn driven(unit: Rendered, driver: Driver) -> Rendered {
        match driver {
            Driver::StandInContainer => unit,
            Driver::Native => unit.replace(&["--driver=native".to_owned()]),
        }
    }

    /// Starts the process for `unit`; those with a `bin` (the container driver's) get
    /// the stand-in `podman` on `PATH` and the planted cgroup tree and id files.
    fn launch(&mut self, kbf: &Path, role: Role, root: &Path, unit: Rendered, bin: Option<&Path>) {
        let (unit, cwd, mut env) = match bin {
            Some(bin) => {
                let mut flags = cgroup_tree(bin);
                flags.push(format!("--id-files={}", id_files(root).display()));
                let env = vec![("PATH".to_owned(), image_path(bin))];
                // The stand-in finds its state beside the link it was run through.
                (unit.append(&flags), bin, env)
            }
            None => (unit, root, Vec::new()),
        };
        env.extend(unit.env.iter().cloned());
        let spec = Spec {
            name: role.to_string(),
            program: kbf.to_owned(),
            argv: unit.argv,
            env,
            cwd: cwd.to_owned(),
        };
        self.launch_spec(role, root, spec);
    }

    fn launch_spec(&mut self, role: Role, root: &Path, spec: Spec) {
        let proc = Proc::start(&spec, &self.logs, 0);
        self.members.push(Member {
            role,
            spec,
            root: root.to_owned(),
            proc,
            generation: 0,
            started: unix_now(),
            killed: false,
            past: Vec::new(),
        });
    }

    fn index(&self, role: Role) -> usize {
        let at = self.members.iter().position(|m| m.role == role);
        at.unwrap_or_else(|| panic!("the farm has no {role}"))
    }

    fn member(&self, role: Role) -> &Member {
        &self.members[self.index(role)]
    }

    fn member_mut(&mut self, role: Role) -> &mut Member {
        let at = self.index(role);
        &mut self.members[at]
    }

    /// Every role of the farm, in the order they started.
    #[must_use]
    pub fn roles(&self) -> Vec<Role> {
        self.members.iter().map(|m| m.role).collect()
    }

    /// The command line a role runs: the rendered unit's, as it was started.
    #[must_use]
    pub fn spec(&self, role: Role) -> &Spec {
        &self.member(role).spec
    }

    /// Waits for `ready`, failing at once with every process's output if one exits, and
    /// after [`HANG`] with the same. `what` says what is waited for.
    ///
    /// # Errors
    /// A process that is not one the harness killed exits, or `HANG` passes.
    pub fn try_wait<T>(
        &mut self,
        what: &str,
        mut ready: impl FnMut(&Farm) -> Option<T>,
    ) -> Result<T, FarmError> {
        let deadline = Instant::now() + self.hang;
        loop {
            let ended = self.members.iter_mut().find_map(|m| {
                if m.killed {
                    return None;
                }
                m.proc
                    .exited()
                    .map(|s| format!("{} exited ({s}) while waiting for {what}", m.role))
            });
            if let Some(ended) = ended {
                return Err(FarmError {
                    what: ended,
                    report: self.logs(),
                });
            }
            if let Some(found) = ready(self) {
                return Ok(found);
            }
            if Instant::now() >= deadline {
                return Err(FarmError {
                    what: format!("timed out waiting for {what}"),
                    report: self.logs(),
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// [`Farm::try_wait`], panicking with the same text: for a test.
    ///
    /// # Panics
    /// On the error `try_wait` returns.
    pub fn wait<T>(&mut self, what: &str, ready: impl FnMut(&Farm) -> Option<T>) -> T {
        self.try_wait(what, ready).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Waits for `role`'s start line, which starts with `prefix`.
    fn start_line(&mut self, role: Role, prefix: &str) -> Result<String, FarmError> {
        self.try_wait(&format!("{role}'s start line"), |f| {
            f.member(role).proc.line(prefix)
        })
    }

    /// Waits until the frontend is ready and lists every node as connected and every
    /// storage member active.
    ///
    /// # Errors
    /// As [`Farm::try_wait`].
    pub fn wait_ready(&mut self) -> Result<(), FarmError> {
        self.try_wait("the frontend's /readyz", |f| {
            (api::get(f.api, "/readyz")?.0 == 200).then_some(())
        })?;
        self.try_wait("every node connected to the frontend", |f| {
            (api::connected(f.api)? == f.node_ids).then_some(())
        })?;
        self.try_wait("every storage member active", |f| {
            (api::members_active(f.api)? == (f.storage_nodes, true)).then_some(())
        })?;
        // Envoy sends nothing to the frontend until its health checks pass: builds start
        // only when it does.
        if let Some(ports) = self.envoy_ports {
            self.try_wait("Envoy to route to the frontend", |_| {
                let (code, text) = api::get(ports.admin, "/clusters")?;
                (code == 200 && envoy::healthy(&text, &ports)).then_some(())
            })?;
        }
        Ok(())
    }

    /// Whether the frontend still lists every node as connected and every storage member
    /// active (what [`Farm::wait_ready`] waited for): a process that is alive proves
    /// nothing of this, since a node that lost its place in the farm keeps running.
    ///
    /// # Errors
    /// What the frontend lists instead, or that it does not answer.
    pub fn whole(&self) -> Result<(), String> {
        let connected = api::connected(self.api)
            .ok_or_else(|| "the frontend's /v1/nodes does not answer".to_owned())?;
        if connected != self.node_ids {
            return Err(format!(
                "nodes connected: {connected:?}, expected {:?}",
                self.node_ids
            ));
        }
        let members = api::members_active(self.api)
            .ok_or_else(|| "the frontend's /v1/members does not answer".to_owned())?;
        if members != (self.storage_nodes, true) {
            return Err(format!(
                "storage members (count, all active): {members:?}, expected {:?}",
                (self.storage_nodes, true)
            ));
        }
        Ok(())
    }

    /// SIGKILL to `role`, as a host losing power would take it.
    pub fn kill(&mut self, role: Role) {
        let m = self.member_mut(role);
        m.proc.kill();
        m.killed = true;
    }

    /// SIGTERM to `role`; its exit status once it has drained, or `None` if it was still
    /// running after the farm's wait (then it is killed). Either way the role is ended on
    /// purpose, and its end is not a crash.
    pub fn term(&mut self, role: Role) -> Option<std::process::ExitStatus> {
        let hang = self.hang;
        let m = self.member_mut(role);
        let status = m.proc.term_within(hang);
        if status.is_none() {
            m.proc.kill();
        }
        m.killed = true;
        status
    }

    /// Starts `role` again from the identical command line (the rendered unit's, as kept
    /// in the farm), its output in files of the next generation. The role must have
    /// ended ([`Farm::kill`] or [`Farm::term`]).
    ///
    /// # Panics
    /// When the role still runs.
    pub fn restart(&mut self, role: Role) {
        let logs = self.logs.clone();
        let m = self.member_mut(role);
        assert!(m.proc.exited().is_some(), "{role} still runs");
        m.past.push(ProcRecord {
            role: role.to_string(),
            pid: m.proc.pid(),
            generation: m.generation,
            started: m.started,
            ended: m.proc.exited().map(ended_text),
        });
        m.generation += 1;
        m.proc = Proc::start(&m.spec, &logs, m.generation);
        m.started = unix_now();
        m.killed = false;
    }

    /// Every start of every role, past ones first: role, pid, generation, start time and
    /// how it ended. It stands in for a boot counter.
    pub fn process_table(&mut self) -> Vec<ProcRecord> {
        let mut table = Vec::new();
        for m in &mut self.members {
            table.extend(m.past.iter().cloned());
            table.push(ProcRecord {
                role: m.role.to_string(),
                pid: m.proc.pid(),
                generation: m.generation,
                started: m.started,
                ended: m.proc.exited().map(ended_text),
            });
        }
        table
    }

    /// The roles that ended without the harness ending them: a crash.
    pub fn unexpected_exits(&mut self) -> Vec<(Role, String)> {
        self.members
            .iter_mut()
            .filter(|m| !m.killed)
            .filter_map(|m| m.proc.exited().map(|s| (m.role, ended_text(s))))
            .collect()
    }

    /// The process list: each role, its process id and its generation.
    #[must_use]
    pub fn process_list(&self) -> String {
        let mut text = String::from("--- processes ---\n");
        for m in &self.members {
            text.push_str(&format!(
                "{} pid {} generation {}\n",
                m.role,
                m.proc.pid(),
                m.generation
            ));
        }
        text
    }

    /// The process list and every process's output, for a failure message.
    #[must_use]
    pub fn logs(&self) -> String {
        let mut text = self.process_list();
        text.extend(self.members.iter().map(|m| m.proc.log()));
        text
    }

    /// The frontend's operator API port (on loopback).
    #[must_use]
    pub fn api_port(&self) -> u16 {
        self.api
    }

    /// The frontend's REAPI address as it printed it, for a client of the frontend itself.
    #[must_use]
    pub fn reapi(&self) -> &str {
        &self.reapi
    }

    /// The port a build client dials (`localhost:<port>`, under the farm CA): Envoy's
    /// REAPI port when it runs, else the frontend's.
    #[must_use]
    pub fn client_port(&self) -> u16 {
        self.client_port
    }

    /// The farm CA's certificate, PEM.
    #[must_use]
    pub fn ca_pem(&self) -> &[u8] {
        &self.ca
    }

    /// The image the actions name (the stand-in `podman`'s); empty with the native driver.
    #[must_use]
    pub fn image(&self) -> &str {
        &self.image
    }

    /// How many storage-and-daemon nodes there are.
    #[must_use]
    pub fn storage_nodes(&self) -> usize {
        self.storage_nodes
    }

    /// The ids the frontend lists as connected when the farm is whole, sorted.
    #[must_use]
    pub fn node_ids(&self) -> &[String] {
        &self.node_ids
    }

    /// The nodes (numbered from 1) that hold `bytes` in their hot or cold store.
    #[must_use]
    pub fn holders(&self, bytes: &[u8]) -> Vec<usize> {
        (1..=self.storage_nodes)
            .filter(|n| {
                let root = &self.member(Role::Node(*n)).root;
                !files_holding(&[root.join("hot"), root.join("cold")], bytes).is_empty()
            })
            .collect()
    }

    /// SIGTERM to each process still running, last started first (the daemons and nodes,
    /// then Envoy, before the frontend, which waits for open streams); each role and how it
    /// ended, a role that had ended on its own before its turn included (with its status
    /// then). A process still running after [`HANG`] is killed and reported as such; a role
    /// the harness killed is not reported.
    pub fn stop_all(&mut self) -> Vec<(Role, String)> {
        let mut ended = Vec::new();
        let hang = self.hang;
        // A role the harness killed is not stopped (the harness knows how it ended).
        for m in self.members.iter_mut().rev().filter(|m| !m.killed) {
            // One that ended on its own before its turn is reported as it ended: the stop's
            // evidence is every role, and a crash must not be absent from it.
            if let Some(status) = m.proc.exited() {
                m.killed = true;
                ended.push((m.role, ended_text(status)));
                continue;
            }
            let status = m.proc.term_within(hang);
            if status.is_none() {
                m.proc.kill();
            }
            m.killed = true;
            ended.push((
                m.role,
                status.map_or_else(
                    || "still running after SIGTERM: killed".to_owned(),
                    ended_text,
                ),
            ));
        }
        ended
    }

    /// [`Farm::stop_all`], and every process must have exited 0.
    ///
    /// # Panics
    /// When one did not.
    pub fn stop(mut self) {
        let ended = self.stop_all();
        for (role, how) in ended {
            assert_eq!(how, "exit 0", "{role}\n{}", self.logs());
        }
    }
}

/// How a process ended, as the process table says it.
fn ended_text(s: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    s.code().map_or_else(
        || format!("signal {}", s.signal().unwrap_or(0)),
        |c| format!("exit {c}"),
    )
}

/// The frontend's `etc` directory: its certificates are Envoy's.
fn frontend_etc(dir: &Path) -> PathBuf {
    dir.join("frontend").join("etc")
}

#[cfg(test)]
#[path = "farm_tests.rs"]
mod tests;
