//! `kbf-node`: the worker daemon as a library. The `kbf-daemon` binary is [`dispatch_helpers`],
//! [`Cli`] parsed from the command line, and [`start`]; another binary that runs the
//! daemon beside the server parses [`Cli`] the same way (it is a `clap::Args` too) and calls
//! the same functions. See the `kbf-daemon` library for what the daemon does.
//!
//! Configuration is by flags only (`kbf-daemon --help`). `--driver` picks how leases
//! run: `fake` (nothing runs; for bring-up), `container` (rootless containers made by `crun`; Linux) or
//! `native` (plain processes; for Macs), and for `whole_machine` leases on a Mac
//! `native-whole-machine` (each lease as its own user in its GUI session, through
//! `kbf-mac-session`; see `whole_machine`). Given more than once, the daemon has one
//! runtime per driver (the node's own, native or container, set up first) and runs
//! each Start on the one that serves its lease kind; it refuses to start when two
//! serve the same kind. The two real drivers read and write blobs over mutual TLS
//! through the server's worker listener named by `--cas` (normally the same address
//! as `--server`), with the daemon's own certificate, and make lease directories
//! under `<--data-dir>/leases`. A Mac in komira's pool runs, for example:
//!
//! ```text
//! kbf-daemon --driver native --server https://kbf-server:8981 --cas https://kbf-server:8981 \
//!   --ca-cert ca.pem --cert node.pem --key node.key --node-id mac-studio-1 \
//!   --data-dir /var/kbf
//! ```
//!
//! Exits non-zero when the node cannot be detected or the configuration cannot be
//! loaded; otherwise runs until SIGTERM or SIGINT, then drains: it takes no new lease,
//! lets the running ones finish and report, waits for their acknowledgements and exits
//! zero, however long the leases take: a drain never ends because time ran out. A second
//! signal is the operator's hard stop; leases still running then are abandoned (their
//! processes killed, their directories removed) and the scheduler places them again.
//! `--drain-deadline-secs`, if given, only makes the daemon warn, once, with the number of
//! leases still running, that many seconds after the first signal; it keeps waiting.
//!
//! Everything the daemon keeps on disk is under one directory, `--data-dir` (default
//! `/var/lib/kbf`; `data_dir`): `leases/`, and under the container driver and the
//! native driver on macOS the input cache it links (container) or clones (native) each
//! lease's input files from, and the shared input trees. Their budgets
//! come from the filesystem, not from flags; `--data-max-bytes` only lowers them, and
//! `--no-input-cache` switches the cache and the trees off.
//!
//! With `--pack-read` (off by default; `pack_read`) the data plane of `kbf daemon` and of the
//! daemon half of `kbf storage-and-daemon` with a frontend reads blobs from the packs of the
//! farm's object store first, finding them through the frontend's pack index; a blob it does
//! not locate is read where it always was, and outputs are stored where they were.
//! With `--pack-write` as well (off by default; `pack_read`) it writes its outputs into packs
//! while the frontend takes pack commits.
//!
//! With `--tierstore-endpoint` (off by default; `tierstore`) `kbf daemon` reads the same packs
//! through a tierstore on loopback first, and every blob it does not serve where it always was.
//!
//! The log goes to stderr, in color only when stderr is a terminal: under launchd or
//! systemd it is a file or a journal, where escape codes are noise (issue #170). The
//! daemon does not rotate it; the service manager that owns stderr does. systemd's
//! journal rotates by its own size limits; launchd's `StandardErrorPath` file is
//! emptied by `kbf-mac-provision` at each restart and upgrade (see
//! `docs/design/mac-node-provisioning.md`, section 5.7).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

pub use kbf_daemon::LocalMember;
use kbf_daemon::{
    Args, Capacity, DAEMON_VERSION, Daemon, DaemonConfig, DriverWatch, FakeRuntime, FarmCas,
    Liveness, MapHolder, NodeReport, Runtime, Runtimes,
};
use kbf_driver_native::slots::Pool;
use kbf_driver_native::{MemoryPolicy, NativeConfig, NativeRuntime, xcode, xcode_watch};
use kbf_outputs::OutputLimits;
use kbf_threads::disk::Statvfs;
use kbf_transfer::Cas;
use tokio::signal::unix::{Signal, SignalKind, signal};

pub use data_dir::DEFAULT_DATA_DIR;
pub use handle::{NodeHandle, Signals};
pub use needs::{ContainerNeeds, HostNeeds};
pub use run::run;

/// The `kbf-daemon` command line.
#[derive(Clone, Debug, clap::Parser)]
#[command(name = "kbf-daemon", version = DAEMON_VERSION, about = "The kbf worker daemon.")]
pub struct Cli {
    #[command(flatten)]
    daemon: Args,
    /// How leases run; once per runtime. The daemon runs each lease on the driver that
    /// serves its kind and refuses to start when two serve the same kind. The node's own
    /// driver (native or container) is set up first, whatever the order given.
    #[arg(long = "driver", value_enum, required = true)]
    drivers: Vec<Driver>,
    /// The server's worker listener, an `https://` URL (normally `--server`'s), that
    /// the container and native drivers read and write blobs through, over mutual TLS
    /// with this daemon's `--ca-cert`, `--cert`, `--key` and `--tls-server-name`.
    /// Plain `http://` is refused: the worker listener admits each blob call by the
    /// daemon's certificate. Every driver's blobs go through one transfer engine, over
    /// one connection, within the `--transfer-*` limits.
    #[arg(long)]
    cas: Option<String>,
    /// The retired `--update-dir` and `--orders-dir`: accepted, hidden, ignored.
    #[command(flatten)]
    retired_update: retired::RetiredUpdateFlags,
    #[command(flatten)]
    data: data_dir::DataArgs,
    #[command(flatten)]
    outputs: OutputLimits,
    /// `--reserve-cpus`: the logical CPUs the farm's processes keep; the leases get the
    /// rest of the CPUs the daemon may use, and the daemon's async, blocking and CPU
    /// pools are sized from it (`--async-threads`, `--blocking-threads` and
    /// `--cpu-threads` override them). See `kbf_threads`.
    #[command(flatten)]
    threads: kbf_threads::ThreadArgs,
    /// How long one lease's whole output upload (its output files, stdout and stderr)
    /// may take, in seconds, before the lease fails with a reason naming the upload and
    /// the time it ran (container and native drivers). Without it, a store that stops
    /// answering holds the lease, and its slot, with no error.
    #[arg(
        long,
        default_value_t = kbf_daemon::UPLOAD_DEADLINE.as_secs(),
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    output_upload_deadline_secs: u64,
    #[command(flatten)]
    transfer: transfer::TransferArgs,
    #[command(flatten)]
    pack_read: pack_read::PackReadArgs,
    #[command(flatten)]
    tierstore: tierstore::TierstoreArgs,
    /// The cgroup lease cgroups are made under, from the cgroup root (container). Left
    /// out, the daemon sets up its own cgroup, which its systemd unit must delegate
    /// (`Delegate=yes`): it moves itself into a `supervisor` leaf there and makes
    /// `actions` beside it, with cpu, memory and pids enabled. Given, it must already
    /// enable cpu, memory and pids for its children.
    #[arg(long)]
    cgroup_parent: Option<String>,
    /// Written to the actions cgroup's `memory.max` at start (container): the most
    /// memory all leases together may use, in GiB, so the rest stays for what else
    /// runs on the node. The node reports the lowest `memory.max` above its leases, or
    /// its memory when that is less, and the CPUs of their cpuset.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    actions_memory_max_gib: Option<u64>,
    #[command(flatten)]
    actions_cpu: actions_cpu::ActionsCpuArgs,
    /// Written to the daemon's `supervisor` leaf's `memory.min` at start, in MiB
    /// (container, without `--cgroup-parent`): memory the kernel does not reclaim from
    /// the daemon while it uses no more, so builds short of memory never take it. The
    /// kernel protects no more than the unit's and its slice's `MemoryMin=` allow; the
    /// daemon warns when one is lower. 0 protects nothing.
    #[arg(long, default_value_t = 256)]
    supervisor_memory_min_mib: u64,
    /// Where cgroup v2 is mounted (container). For tests of the cgroup setup only.
    #[arg(long, hide = true, default_value = "/sys/fs/cgroup")]
    cgroup_root: PathBuf,
    /// The file naming the daemon's own cgroup (container). For tests of the cgroup
    /// setup only.
    #[arg(long, hide = true, default_value = "/proc/self/cgroup")]
    self_cgroup: PathBuf,
    /// Each container's `pids.max`: its tasks, threads included (container).
    #[arg(long, default_value_t = 8192)]
    container_pids_limit: u64,
    /// The size of each container's `/dev/shm`, in MiB (container).
    #[arg(long, default_value_t = 64)]
    container_shm_mib: u64,
    /// Each container's open-file limit, soft and hard (container). Rootless, it cannot
    /// exceed the daemon's own hard limit.
    #[arg(long, default_value_t = 65_536)]
    container_nofile: u64,
    /// The process limit, soft and hard, of each container's user (container). Every
    /// container's root is the same host id, so it bounds all the node's actions
    /// together.
    #[arg(long, default_value_t = 32_768)]
    container_nproc: u64,
    /// A lease at its own memory cap is killed, and reported out of memory, once it
    /// has pushed more than this many MiB into swap at its cap... (container)
    #[arg(long, default_value_t = 512)]
    lease_swap_kill_mib: u64,
    /// ...or this percentage of its booked memory, whichever is more. The kernel caps
    /// each lease's RAM at its booking x 1.5 + 512 MiB and lets it swap without limit;
    /// a lease is killed by this rule only while its own cap keeps being hit (its
    /// `memory.events` `max` grew since the last sample) and its swap rose since the
    /// last sample, and only swap that rose so counts, never swap that host pressure
    /// moved out of it.
    #[arg(long, default_value_t = 25)]
    lease_swap_kill_percent: u64,
    /// The daemon takes no new lease while the node's memory stall time (the `some`
    /// line's `avg10` in `/proc/pressure/memory`, as a percentage) is above this and
    /// it runs a lease: it says so to the server, which places nothing on it until the
    /// stall falls, and the leases it runs go on. With no lease it serves whatever the
    /// stall, so it is never left idle for good. Off where the kernel reports no
    /// pressure stall information (macOS, a kernel built without it).
    #[arg(long, default_value_t = 10.0, value_parser = percent)]
    memory_pressure_pause_percent: f64,
    /// On the first SIGTERM or SIGINT the daemon drains: it takes no new lease, lets
    /// the running ones finish and report, waits for their acknowledgements, then exits
    /// 0, however long that takes. A second signal stops it at once and abandons the
    /// leases still running. Left out, the drain has no deadline. Given, this many
    /// seconds after the first signal the daemon logs a warning with the number of
    /// leases still running and keeps waiting; the value has no upper bound, and 0
    /// warns at once. It never abandons a lease. Used by `kbf daemon`, `kbf
    /// storage-and-daemon` and `kbf all-in-one`, whose daemon half drains once its server
    /// half has no execution open, while the server half goes on serving it; a second
    /// signal stops both.
    #[arg(long, value_name = "SECONDS")]
    drain_deadline_secs: Option<u64>,
    /// Where the kernel reports memory pressure stall information. For tests only.
    #[arg(long, hide = true, default_value = kbf_daemon::pressure::MEMORY_PSI)]
    psi_memory_file: PathBuf,
    /// How often each lease's `max` events and swap are sampled, in milliseconds
    /// (container).
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    lease_swap_poll_ms: u64,
    /// A lease's processes are killed past this percentage of its booked memory...
    #[arg(long, default_value_t = MemoryPolicy::DEFAULT.percent)]
    memory_limit_percent: u64,
    /// ...plus this many MiB (native). A lease that booked nothing has no limit.
    #[arg(long, default_value_t = MemoryPolicy::DEFAULT.headroom_bytes >> 20)]
    memory_headroom_mib: u64,
    /// How often a lease's memory is measured, in milliseconds (native).
    #[arg(long, default_value_t = 250)]
    memory_poll_ms: u64,
    /// How long a lease's processes are given to end after SIGTERM before they are
    /// killed, in milliseconds; 0 kills them at once (native). A workload that cleans up
    /// when asked to end, such as Python's multiprocessing resource tracker with the
    /// shared memory it made, does so in this time; one that ignores SIGTERM is named in
    /// the lease's stderr. A slot's clean uses the same value (at most its own time
    /// limit). More than 60000 is taken as 60000. The wait comes after the action's
    /// timeout and its memory watch have ended it (neither runs during it), and on top
    /// of the time given to the kill that follows. A lease past its memory limit is
    /// killed at once. Applies on every platform.
    #[arg(long, default_value_t = 2000)]
    native_term_grace_ms: u64,
    /// The directory searched for `Xcode*.app` (native). Each Xcode whose own
    /// `xcodebuild` answers `-version`, `-license check` and `-checkFirstLaunchStatus`,
    /// and for which `xcrun --find clang` does (each within a minute, under the
    /// actions' sandbox), is ready: it is reported as an `xcode` entry, and an action
    /// that names its build runs with it as `DEVELOPER_DIR`. One that is not is listed
    /// in the node's status with why and the command that fixes it, and logged at WARN.
    /// The daemon says Hello without waiting for its first survey, which runs in the
    /// background: until it ends, each Xcode is listed as not surveyed yet and none is
    /// reported.
    #[arg(long, default_value = xcode::APPLICATIONS)]
    xcode_apps: PathBuf,
    /// How often, in seconds, the Xcodes are asked again (native), so one fixed while
    /// the daemon runs becomes ready without a restart.
    #[arg(long, default_value_t = xcode_watch::EVERY.as_secs())]
    xcode_recheck_secs: u64,
    /// This node's actions use Metal (native): an Xcode whose Metal toolchain is not
    /// installed (`xcodebuild -showComponent MetalToolchain`) is not ready.
    #[arg(long)]
    require_metal_toolchain: bool,
    /// A container image this node is pinned to, as an action names it:
    /// `docker://<repo>@sha256:<digest>`, one architecture's manifest digest
    /// (container; repeatable). At start the daemon makes sure each is in the node's
    /// image store, as the prepare step of every lease does: it fetches the image from
    /// its registry by that digest, checks every byte against a digest, and unpacks it
    /// once. It exits with status 2 if one cannot be fetched, fails a check, or names an
    /// image index. The node's status lists them (`GET /v1/nodes`,
    /// `software.container_images`). No work is placed by image yet: an action may still
    /// name another image.
    #[arg(long = "image", value_name = "IMAGE")]
    images: Vec<String>,
    /// Where each lease runs among the node's CPUs (container, Linux): `none`, the
    /// default, lets a lease use any CPU the daemon may; `numa-node` puts each lease on
    /// one NUMA node, its CPUs (`cpuset.cpus`) and memory (`cpuset.mems`), the node with
    /// the least CPU booked by running leases (the lowest id on a tie). `--reserve-cpus`
    /// is a quota on all leases together, so it still applies; it removes no CPU from a
    /// node. Needs the `cpuset` controller delegated to the daemon (the start fails and
    /// says how when it is not); a host with fewer than two NUMA nodes that hold CPUs
    /// runs as `none`, which the start logs. Refused with a driver other than
    /// `container` (macOS has no such driver).
    #[arg(long, default_value = "none", value_parser = ["none", "numa-node"], value_name = "none|numa-node")]
    lease_placement: String,
    /// Where the kernel lists NUMA nodes (container). For tests of the placement only.
    #[arg(long, hide = true, default_value = "/sys/devices/system/node")]
    numa_dir: PathBuf,
    /// The OCI runtime that runs each action's container (container): `crun`, run
    /// directly. Each action gets a fresh rootless sandbox. It needs the daemon user's
    /// subordinate ids in one range, `newuidmap` and `newgidmap` (package `uidmap`) on
    /// `PATH`, and every directory down to `--data-dir` searchable by others.
    #[arg(long, default_value = "/usr/bin/crun")]
    oci_runtime: PathBuf,
    /// A directory holding `passwd`, `subuid` and `subgid` that the container
    /// driver's startup check reads instead of `/etc`'s. For tests of that check only.
    #[arg(long, hide = true)]
    id_files: Option<PathBuf>,
    /// The socket `kbf-mac-session` serves (native-whole-machine).
    #[arg(long, default_value = "/var/run/kbf-mac-session/socket")]
    mac_session_socket: PathBuf,
    /// This daemon's own user, which keeps read and remove access to what a
    /// whole-machine lease's user writes in its lease directory (native-whole-machine).
    #[arg(long, default_value = "_kbf")]
    daemon_user: String,
    /// How many lease users the pool has (native, macOS, a daemon run as root): every
    /// action runs as one of `kbf-pool-0` .. `kbf-pool-<N-1>`, one lease per user at a
    /// time, as kbf-mac-provision makes them (`pool_users`). Required of a root daemon,
    /// which runs no action as root; refused for any other.
    #[arg(long, requires = "pool_first_uid", value_parser = clap::value_parser!(u32).range(1..=100))]
    pool_users: Option<u32>,
    /// The uid of `kbf-pool-0`, and the group id of all the pool users (`pool_first_uid`
    /// in kbf-mac-provision's profile); at least 600.
    #[arg(long, requires = "pool_users")]
    pool_first_uid: Option<u32>,
}

/// The modes whose daemon reads blobs in-process ([`start_with`]).
const IN_PROCESS_MODES: &str = "`kbf all-in-one` or `kbf storage-and-daemon` without `--frontend`";

/// The drivers this binary can run leases through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Driver {
    /// Runs nothing; every action succeeds with an empty result. For bring-up only.
    Fake,
    /// Each action in a fresh rootless container made by `crun` (Linux).
    Container,
    /// Each action as plain processes on the node, for Macs.
    Native,
    /// Each whole-machine lease as its own user in its GUI session, through
    /// `kbf-mac-session` (macOS).
    NativeWholeMachine,
}

/// What stops the daemon from starting.
pub type Error = Box<dyn std::error::Error>;

/// The daemon's exit status when the host refuses the start ([`exit_code`]).
const REFUSED: u8 = 2;

/// An `--image` this node cannot run: missing from the image store, an index, or not
/// an image by digest. The daemon exits with [`REFUSED`].
#[derive(Debug)]
pub struct ImageUnusable(String);

impl std::fmt::Display for ImageUnusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "--image: {}", self.0)
    }
}

impl std::error::Error for ImageUnusable {}

/// The exit status for an error that stopped the daemon from starting.
#[must_use]
pub fn exit_code(e: &Error) -> ExitCode {
    if refused(e) {
        ExitCode::from(REFUSED)
    } else {
        ExitCode::FAILURE
    }
}

/// A start the host refuses that no driver's error type names: the daemon's own cgroup
/// cannot be read.
#[derive(Debug)]
struct HostRefused(String);

impl std::fmt::Display for HostRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostRefused {}

/// Whether `e` is a start the host refuses, which a restart would not change: an image
/// that cannot be used ([`ImageUnusable`]), a daemon user without subordinate ids, a
/// cgroup that is not delegated, or a container runtime that is missing. The units carry
/// `RestartPreventExitStatus=2`, so systemd leaves such a daemon down for its operator
/// instead of restarting it every `RestartSec` forever.
fn refused(e: &Error) -> bool {
    // The container driver is Linux's: macOS has none of these errors.
    #[cfg(target_os = "linux")]
    if e.is::<kbf_driver_container::SubidError>()
        || e.is::<kbf_driver_container::DelegateError>()
        || e.is::<kbf_driver_container::OciError>()
    {
        return true;
    }
    e.is::<ImageUnusable>() || e.is::<HostRefused>()
}

/// Runs the helper this process was started as, if it was: each run's keeper, the
/// lease-user pool's `as-user` step or the container driver's `__chown-dirs`, all of
/// which are this binary re-run with a first argument the daemon's flags never take. A
/// binary calls it first thing, before parsing [`Cli`], and exits with the code when
/// it returns one.
#[must_use]
pub fn dispatch_helpers() -> Option<ExitCode> {
    // Each run's keeper (`kbf-keeper __keeper ...`) is this binary, dispatched before
    // the flags are parsed, so a keeper never reads the daemon's command line.
    if let Some(code) = kbf_daemon::keeper::dispatch() {
        return Some(code);
    }
    // So is the `as-user` step an action of the lease-user pool starts through
    // (`launchctl asuser <uid> kbf __as-user ...`).
    #[cfg(target_os = "macos")]
    if let Some(code) = kbf_driver_native::slots::dispatch() {
        return Some(code);
    }
    // So is the container driver's `__chown-dirs` helper, which it runs in a helper
    // container.
    #[cfg(target_os = "linux")]
    if let Some(code) = kbf_driver_container::chown_dirs::dispatch() {
        return Some(ExitCode::from(code));
    }
    None
}

/// The thread sizes the flags and this machine's CPUs give; fails on a reserve the
/// machine cannot give.
/// What the node offers the scheduler: `detected` lowered to what the leases may use.
/// The report sent to the scheduler and the placer's whole-host figure both come from
/// here, so a lease the scheduler books as the whole node is the one the placer leaves
/// unpinned.
fn offered(detected: NodeReport, capacity: Capacity) -> NodeReport {
    detected.within(capacity)
}

fn sizes(cli: &Cli) -> Result<kbf_threads::Sizes, Error> {
    Ok(sizes_on(cli, kbf_threads::Topology::detect())?)
}

/// [`sizes`] on a given machine shape: the reserve, the pools from it, then the
/// overrides.
fn sizes_on(
    cli: &Cli,
    topology: kbf_threads::Topology,
) -> Result<kbf_threads::Sizes, kbf_threads::ReserveError> {
    cli.threads.resolve(topology)
}

/// Builds the runtimes `--driver` names and the daemon over them; returns an error
/// that stopped the daemon from starting.
///
/// # Errors
/// The flags name no driver or a combination that cannot run, a driver's start-up
/// checks fail ([`ImageUnusable`] among them, for which [`exit_code`] answers 2), or
/// the node cannot be detected.
pub fn start(cli: &Cli) -> Result<NodeHandle, Error> {
    // With `--tierstore-endpoint`, the same plane with its reads going through it first.
    if let Some(settings) = cli.tierstore.settings()? {
        return tierstore::start(cli, settings);
    }
    // With `--pack-read`, the same plane with its reads going to the packs first.
    if let Some(settings) = cli.pack_read.settings() {
        return pack_read::start(cli, settings);
    }
    // The map the frontend pushes lands in `maps`: the daemon fills it, the data plane
    // routes by it.
    let maps = Arc::new(MapHolder::new());
    let held = Arc::clone(&maps);
    let node = start_over(cli, Some(Arc::clone(&maps)), move |cli, liveness| {
        farm(cli, &held, liveness).map(|(plane, _)| plane)
    })?;
    Ok(node.checking_reach(maps, None))
}

/// [`start`], for a daemon in the process of the storage node `local` (`kbf
/// storage-and-daemon` with a frontend): the data plane routes every blob by the map the
/// frontend sends, the blobs of `local` in-process and the others over TLS to their
/// owners ([`kbf_daemon::member_cas`]). There is no `--cas` leg: with no map held a call
/// fails UNAVAILABLE and writes nothing. With `--pack-read` reads ask the packs first
/// (`pack_read`), the frontend over a connection to the first `--server`.
///
/// # Errors
/// As [`start`].
pub fn start_member(cli: &Cli, local: LocalMember) -> Result<NodeHandle, Error> {
    cli.tierstore.refuse("`kbf storage-and-daemon`")?;
    if let Some(settings) = cli.pack_read.settings() {
        return pack_read::start_member(cli, local, settings);
    }
    let maps = Arc::new(MapHolder::new());
    let held = Arc::clone(&maps);
    let own = local.addr().to_owned();
    let node = start_over(cli, Some(Arc::clone(&maps)), move |cli, liveness| {
        member(cli, &held, &local, liveness)
    })?;
    Ok(node.checking_reach(maps, Some(own)))
}

/// [`start`], with every runtime reading and writing blobs through the data plane
/// `plane` makes, rather than through the transfer engine over `--cas`: for a binary
/// that runs the daemon in the server's own process. `plane` is called once, in the
/// async runtime's context, when the first driver that moves blobs is set up (never,
/// with `--driver fake` alone).
///
/// # Errors
/// As [`start`], or what `plane` returns.
pub fn start_with<C: Cas>(
    cli: &Cli,
    plane: impl Fn(&Cli) -> Result<Arc<C>, Error> + 'static,
) -> Result<NodeHandle, Error> {
    cli.pack_read.refuse_in_process()?;
    cli.tierstore.refuse(IN_PROCESS_MODES)?;
    start_over(cli, None, move |cli, _| plane(cli))
}

/// [`start_with`], and the daemon keeping the storage map its frontend pushes in `maps`
/// (`None`: the daemon ignores the map, as a server's own process does).
fn start_over<C: Cas>(
    cli: &Cli,
    maps: Option<Arc<MapHolder>>,
    plane: impl Fn(&Cli, Liveness) -> Result<Arc<C>, Error> + 'static,
) -> Result<NodeHandle, Error> {
    // Refused here, before any driver is set up, though only the plane over a storage
    // map reads the values; the plane gets the checked values, not the flags again.
    cli.retired_update.warn();
    let liveness = cli.daemon.storage_liveness()?;
    let sizes = sizes(cli)?;
    sizes.log();
    let tokio = sizes.runtime()?;
    let daemon = {
        // The CAS channel is made lazily, which needs the runtime's context.
        let _context = tokio.enter();
        let xcrun = Path::new(xcode::XCRUN);
        let (daemon, _) = node_over(
            cli,
            || native_config(cli),
            xcrun,
            maps,
            Box::new(move |cli| plane(cli, liveness)),
        )?;
        backstop(cli, daemon)
    };
    Ok(NodeHandle {
        tokio,
        daemon,
        drain_deadline: cli.drain_deadline_secs.map(Duration::from_secs),
        reach: None,
    })
}

/// The data plane of a daemon whose server is elsewhere: the process's transfer engine
/// ([`transfer::engine`]) over `--cas`, used until the frontend sends a storage map and
/// not after (`kbf_daemon::FarmCas`). With a map every blob goes to the storage node that
/// owns it, verified against its own name from the map (the daemon's CA and certificate,
/// never `--tls-server-name`, which names the frontend). A frontend that sends no map
/// leaves the daemon on `--cas`, as before the map existed. Each change of side is
/// logged once, by the plane.
///
/// Answers the plane with the liveness its storage nodes hold, read back from them: the
/// plane keeps them as `dyn Nodes`, which has none to read, and a test must see that the
/// flags arrived (`start` drops it).
fn farm(
    cli: &Cli,
    maps: &Arc<MapHolder>,
    liveness: Liveness,
) -> Result<(Arc<FarmCas>, Liveness), Error> {
    farm_over(cli, maps, liveness, Arc::new(transfer::engine(cli)?))
}

/// [`farm`] over the engine `server`, which a test gives an in-process channel.
fn farm_over(
    cli: &Cli,
    maps: &Arc<MapHolder>,
    liveness: Liveness,
    server: Arc<kbf_transfer::Transfers>,
) -> Result<(Arc<FarmCas>, Liveness), Error> {
    let nodes = kbf_daemon::storage_nodes(&DaemonConfig::from_args(&cli.daemon).tls, liveness)?;
    tracing::info!(
        "data plane: through --cas until the frontend sends a storage map, then to the \
         storage nodes it names"
    );
    let held = nodes.liveness();
    let plane = FarmCas::with_hold(
        server,
        Arc::clone(maps),
        nodes,
        &cli.transfer.config(),
        kbf_daemon::DEFAULT_HOLD,
    );
    Ok((Arc::new(plane), held))
}

/// The data plane of [`start_member`]: the daemon's CA and certificate for the members
/// that are not `local`, each verified against its own name from the map.
fn member(
    cli: &Cli,
    maps: &Arc<MapHolder>,
    local: &LocalMember,
    liveness: Liveness,
) -> Result<Arc<kbf_transfer::RoutedCas>, Error> {
    let nodes = kbf_daemon::storage_nodes(&DaemonConfig::from_args(&cli.daemon).tls, liveness)?;
    Ok(kbf_daemon::member_cas_with_hold(
        Arc::clone(maps),
        local.clone(),
        nodes,
        &cli.transfer.config(),
        kbf_daemon::DEFAULT_HOLD,
    ))
}

/// The engine over `--cas` alone, for tests of the node's set-up.
#[cfg(test)]
fn remote(cli: &Cli) -> Result<Arc<kbf_transfer::Transfers>, Error> {
    Ok(Arc::new(transfer::engine(cli)?))
}

/// `daemon` with the memory backstop `--memory-pressure-pause-percent` sets, if the
/// node reports memory pressure ([`memory_stall`]). Needs a tokio runtime's context.
fn backstop(cli: &Cli, daemon: Daemon) -> Daemon {
    match memory_stall(cli) {
        Some(stall) => {
            tracing::info!(
                above_percent = cli.memory_pressure_pause_percent,
                "memory backstop: on"
            );
            daemon.with_memory_backstop(stall, cli.memory_pressure_pause_percent)
        }
        None => daemon,
    }
}

/// A percentage above 0 and at most 100, for `--memory-pressure-pause-percent`.
fn percent(text: &str) -> Result<f64, String> {
    let value: f64 = text
        .parse()
        .map_err(|e| format!("{text:?} is not a number: {e}"))?;
    if value > 0.0 && value <= 100.0 {
        Ok(value)
    } else {
        Err(format!("{text} is not above 0 and at most 100"))
    }
}

/// The node's memory stall time, sampled every [`kbf_daemon::pressure::SAMPLE_EVERY`]
/// from the file `--psi-memory-file` names; `None` (the backstop off, logged) when the
/// file cannot be read now, as on a kernel without pressure stall information. Needs a
/// tokio runtime's context.
fn memory_stall(cli: &Cli) -> Option<tokio::sync::watch::Receiver<Option<f64>>> {
    let stall = kbf_daemon::pressure::watch(
        cli.psi_memory_file.clone(),
        kbf_daemon::pressure::SAMPLE_EVERY,
    );
    if stall.borrow().is_none() {
        tracing::info!(
            file = %cli.psi_memory_file.display(),
            "memory backstop: off: the file cannot be read"
        );
        return None;
    }
    Some(stall)
}

/// The drivers `--driver` names, each once: the node's own (native or container)
/// first, then the others in the order given.
fn drivers(cli: &Cli) -> Vec<Driver> {
    let own = |d: &Driver| matches!(d, Driver::Native | Driver::Container);
    let ordered = cli.drivers.iter().filter(|d| own(d));
    let mut drivers: Vec<Driver> = Vec::new();
    for driver in ordered.chain(cli.drivers.iter().filter(|d| !own(d))) {
        if !drivers.contains(driver) {
            drivers.push(*driver);
        }
    }
    drivers
}

/// What the drivers set up so far give the daemon, whose runtimes move blobs through
/// the data plane `C`.
struct Node<C> {
    /// One runtime per driver; `None` before the first.
    runtimes: Option<Runtimes>,
    /// What leases may use (the container driver's `actions/` cgroup).
    capacity: Capacity,
    /// The native driver's Xcode reports.
    reports: Option<Reports>,
    /// The whole-machine runtime's phase.
    phase: Option<tokio::sync::watch::Receiver<kbf_types::NodePhase>>,
    /// The data plane every runtime shares, made by the first that needs it.
    plane: Plane<C>,
}

/// A [`Node`]'s data plane: how to make it until a runtime asks for it, then it.
enum Plane<C> {
    Unmade(MakePlane<C>),
    Made(Arc<C>),
}

/// What makes a daemon's data plane from its flags.
type MakePlane<C> = Box<dyn Fn(&Cli) -> Result<Arc<C>, Error>>;

impl<C: Cas> Node<C> {
    /// No runtime yet, its data plane made by `plane` when a runtime first asks.
    fn new(plane: MakePlane<C>) -> Self {
        Self {
            runtimes: None,
            capacity: Capacity::default(),
            reports: None,
            phase: None,
            plane: Plane::Unmade(plane),
        }
    }

    /// The process's data plane, made on the first call; every later call returns the
    /// same one.
    fn plane(&mut self, cli: &Cli) -> Result<Arc<C>, Error> {
        let made = match &self.plane {
            Plane::Made(made) => return Ok(Arc::clone(made)),
            Plane::Unmade(make) => make(cli)?,
        };
        self.plane = Plane::Made(Arc::clone(&made));
        Ok(made)
    }

    /// Adds `runtime`. Fails when it serves a lease kind a runtime added before serves.
    fn add<R: Runtime>(&mut self, runtime: Arc<R>) -> Result<(), Error> {
        self.runtimes = Some(match self.runtimes.take() {
            None => Runtimes::new(runtime),
            Some(set) => set.and(runtime)?,
        });
        Ok(())
    }
}

/// The daemon with a runtime for each of [`drivers`], set up in that order, every one
/// moving blobs through the one data plane `plane` makes; the native driver with the
/// configuration `native` makes and the `xcrun` it surveys with and
/// warms (a test names its own user folders and sandbox, and an `xcrun` of its own).
/// Also returns what the native driver's Xcode watch sends the daemon, for a test to
/// follow. Its node report is the detected entries and a `drivers` entry
/// per runtime (which `Daemon::new` adds), its `cpus` and `mem_gib` no more than the
/// container driver allows; the native driver adds its own with
/// `Daemon::with_driver_report`. Its status lists every `--image`, which only the
/// container driver runs: without it, `--image` fails the start. With a real driver, once
/// every runtime is built, it claims the lease directory and names the claim it found
/// as its predecessor (`instance`). With `maps`, the daemon keeps the storage map its
/// frontend pushes there.
fn node_over<C: Cas>(
    cli: &Cli,
    native: impl Fn() -> Result<NativeConfig, Error>,
    xcrun: &Path,
    maps: Option<Arc<MapHolder>>,
    plane: MakePlane<C>,
) -> Result<(Daemon, Option<Reports>), Error> {
    if !cli.images.is_empty() && !cli.drivers.contains(&Driver::Container) {
        return Err("--image names container images: only --driver container runs them".into());
    }
    let plan = data_dir(cli)?;
    if cli.lease_placement != "none" && !cli.drivers.contains(&Driver::Container) {
        return Err(format!(
            "--lease-placement={} places container leases on NUMA nodes: only --driver container \
             runs them (Linux only; macOS has no NUMA nodes or cpusets)",
            cli.lease_placement
        )
        .into());
    }
    cli.actions_cpu.refuse(&cli.drivers)?;
    let mut node = Node::new(plane);
    for driver in drivers(cli) {
        match driver {
            Driver::Fake => node.add(Arc::new(FakeRuntime::new(Duration::ZERO)))?,
            Driver::Native => {
                #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
                let mut config = native()?;
                // Its leases' inputs are clones of the data directory's cache and trees
                // on macOS; elsewhere the plan makes neither.
                #[cfg(target_os = "macos")]
                if let Some(plan) = plan.as_ref().filter(|plan| plan.cached) {
                    config.cache = data_dir::native_cache(plan);
                    config.trees = data_dir::native_trees(plan, config.cache.as_ref());
                }
                let cas = node.plane(cli)?;
                let (runtime, reports) = native_runtime(cli, config, xcrun, cas)?;
                node.add(runtime)?;
                node.reports = Some(reports);
            }
            Driver::Container => container::add(cli, &mut node, &plan)?,
            Driver::NativeWholeMachine => whole_machine::add(cli, &mut node)?,
        }
    }
    let runtimes = node.runtimes.ok_or("no --driver given")?;
    // Whatever the driver, leases get the CPUs the daemon may use but the reserve (the
    // container driver's `actions/cpu.max` says the same to the kernel).
    let leases = sizes(cli)?.lease_cpus() as u64;
    node.capacity.cpus = Some(node.capacity.cpus.map_or(leases, |cpus| cpus.min(leases)));
    // `Daemon::new` adds every runtime's driver.
    let report = offered(NodeReport::detect(&[])?, node.capacity);
    // A node without the whole-machine runtime is always serving: a phase that never
    // changes.
    let phase = node
        .phase
        .unwrap_or_else(|| tokio::sync::watch::channel(kbf_types::NodePhase::Serving).1);
    let daemon = Daemon::new(DaemonConfig::from_args(&cli.daemon), runtimes, report)?
        .with_container_images(cli.images.iter().cloned())
        .with_phase(phase);
    // One line, not a match: this function has an instance per data plane, and an arm
    // each instance never takes would be a line the coverage ratchet counts as missed.
    let daemon = maps.into_iter().fold(daemon, Daemon::with_map);
    // Every runtime is built, so every sweep has run: claim the scratch directory.
    let daemon = match &plan {
        Some(plan) => instance::named(daemon, &plan.leases()),
        None => daemon,
    };
    Ok(match node.reports {
        Some(reports) => (daemon.with_driver_report(reports.clone()), Some(reports)),
        None => (daemon, None),
    })
}

/// [`node_over`] with the transfer engine over `--cas` as the data plane.
#[cfg(test)]
fn node(
    cli: &Cli,
    native: impl Fn() -> Result<NativeConfig, Error>,
    xcrun: &Path,
) -> Result<(Daemon, Option<Reports>), Error> {
    node_over(cli, native, xcrun, None, Box::new(remote))
}

/// [`node`] with the native driver alone, its configuration `config`.
#[cfg(test)]
fn native_with(cli: &Cli, config: NativeConfig, xcrun: &Path) -> Result<(Daemon, Reports), Error> {
    let (daemon, reports) = node(cli, || Ok(config.clone()), xcrun)?;
    Ok((daemon, reports.ok_or("no native driver")?))
}

/// The native driver with `config`, the `xcrun` it surveys with and warms, and the
/// process's data plane `cas`. It
/// surveys the Xcodes in `--xcode-apps` in the background, at once and again every
/// `--xcode-recheck-secs`, and returns each changed survey for the daemon
/// (`Daemon::with_driver_report`): without it the node reports no Xcode, ready or
/// not. It does not wait for the first survey: until that ends, the daemon reports
/// every Xcode found as not surveyed yet, and none as ready.
fn native_runtime<C: Cas>(
    cli: &Cli,
    mut config: NativeConfig,
    xcrun: &Path,
    cas: Arc<C>,
) -> Result<(Arc<NativeRuntime<C>>, Reports), Error> {
    // The simulators of the leases that ask for one are made with the same `xcrun`.
    config.xcrun = xcrun.to_owned();
    let runtime = NativeRuntime::new(config, cas)?;
    let runtime = Arc::new(runtime);
    let watched = Arc::clone(&runtime);
    let (mut probe, every) = xcode_watch_args(cli);
    probe.xcrun = xcrun.to_owned();
    // Every question runs as an action does: xcrun reads and fills its cache, which
    // leases can write, only under the sandbox.
    probe.sandbox = Some(runtime.sandbox(kbf_driver_native::SURVEY_DIR));
    let warm = xcrun.to_owned();
    let warmed = std::sync::Once::new();
    let apply = move |xcodes: &[xcode::Xcode]| {
        let report = watched.apply_xcodes(xcodes);
        // Once the first survey is applied, in the background and sandboxed as an
        // action: xcrun fills its cache for the node's own Xcode and the ready ones
        // while the node serves; each later survey warms the Xcodes it makes ready.
        // Not before: the warm-up's lookups would make the survey's miss.
        if xcodes.iter().all(|x| x.state != xcode::State::NotSurveyed) {
            warmed.call_once(|| {
                let _ = watched.warm_xcrun(&warm, xcode::ANSWER_WITHIN);
            });
        }
        report
    };
    let (_driver, reports, xcodes) = watch_parts(&runtime)?;
    // Returns at once: the survey runs on the watch's thread.
    let _ = xcode_watch::watch(
        xcodes,
        cli.xcode_apps.clone(),
        probe,
        every,
        Box::new(apply),
    );
    Ok((runtime, reports))
}

/// The native driver's driver watch, with a part per source so none overwrites another's
/// entries: the slots' (the number of slots that can run a lease reaches the daemon
/// whenever it changes, whether or not a survey does) wired to `runtime` before the
/// Xcodes', whose part is returned for the survey.
fn watch_parts<C: Cas>(
    runtime: &NativeRuntime<C>,
) -> Result<(DriverWatch, Reports, kbf_daemon::DriverPart), Error> {
    let (driver, reports) = DriverWatch::new();
    let slots = driver
        .part(kbf_driver_native::slots::SOURCE)
        .ok_or("the slots part of the driver watch is taken")?;
    runtime.report_slots_through(slots);
    let xcodes = driver
        .part(xcode_watch::SOURCE)
        .ok_or("the Xcode part of the driver watch is taken")?;
    Ok((driver, reports, xcodes))
}

/// What the native driver's surveys send the daemon, merged.
type Reports = tokio::sync::watch::Receiver<kbf_daemon::DriverReport>;

/// The data directory, resolved and made, for a daemon that runs a driver that keeps
/// lease directories (every one but `fake`) or is given `--data-dir`; `None` for the
/// fake driver alone without it, which touches no directory. Logs every directory and
/// budget it resolved.
fn data_dir(cli: &Cli) -> Result<Option<data_dir::Plan>, Error> {
    let real = |d: &Driver| *d != Driver::Fake;
    if !cli.drivers.iter().any(real) && cli.data.data_dir.is_none() {
        return Ok(None);
    }
    // The drivers that link or clone leases' inputs from the cache.
    let cached = cli.drivers.contains(&Driver::Container)
        || (cfg!(target_os = "macos") && cli.drivers.contains(&Driver::Native));
    let plan = data_dir::Plan::new(&cli.data, cached, &Statvfs)?.with_open_leases(leases_open(cli));
    plan.create()?;
    for line in plan.describe() {
        tracing::info!("{line}");
    }
    Ok(Some(plan))
}

/// Whether `<--data-dir>/leases` must be searchable by others (0711, not the daemon's
/// 0700): the container ids of the container driver are not the daemon's, and neither
/// are the pool users of the native driver (`--pool-users`), each of which must reach
/// its own lease directory through it. A native daemon run as its own user keeps 0700.
fn leases_open(cli: &Cli) -> bool {
    cli.drivers.contains(&Driver::Container)
        || (cli.pool_users.is_some() && cli.drivers.contains(&Driver::Native))
}

/// The lease directory root, `<--data-dir>/leases`, which the real drivers require.
fn scratch(cli: &Cli) -> Result<PathBuf, Error> {
    let dir = cli.data.leases();
    if !dir.is_absolute() {
        return Err(format!("--data-dir {} must be absolute", cli.data.dir().display()).into());
    }
    Ok(dir)
}

/// `--output-upload-deadline-secs`.
fn upload_deadline(cli: &Cli) -> Duration {
    Duration::from_secs(cli.output_upload_deadline_secs)
}

fn native_config(cli: &Cli) -> Result<NativeConfig, Error> {
    let mut config = NativeConfig::new(scratch(cli)?);
    config.outputs = cli.outputs;
    config.upload_deadline = upload_deadline(cli);
    config.memory = MemoryPolicy {
        percent: cli.memory_limit_percent,
        headroom_bytes: cli.memory_headroom_mib.saturating_mul(1 << 20),
    };
    config.poll = Duration::from_millis(cli.memory_poll_ms.max(1));
    config.term_grace =
        Duration::from_millis(cli.native_term_grace_ms).min(kbf_driver_native::grace::MAX_GRACE);
    config.pool = (cli.pool_users.zip(cli.pool_first_uid))
        .map(|(count, first)| Pool::provisioned(count, first))
        .transpose()?;
    Ok(config)
}

/// How the native driver asks its Xcodes (the system's tools, and the Metal toolchain
/// if required), and how often it asks again (at least every second).
fn xcode_watch_args(cli: &Cli) -> (xcode::Probe, Duration) {
    let probe = xcode::Probe::system(cli.require_metal_toolchain);
    (probe, Duration::from_secs(cli.xcode_recheck_secs.max(1)))
}

#[cfg(target_os = "linux")]
#[path = "container_linux.rs"]
mod container;

#[cfg(not(target_os = "linux"))]
#[path = "container_other.rs"]
mod container;

mod actions_cpu;
mod data_dir;
mod handle;
mod instance;
mod needs;
mod pack_read;
mod retired;
mod run;
mod tierstore;
mod transfer;
mod whole_machine;

#[cfg(test)]
mod deploy_doc_tests;
#[cfg(test)]
mod exit_tests;
#[cfg(test)]
mod instance_tests;
#[cfg(test)]
mod startup_tests;

#[cfg(test)]
mod signal_tests;
#[cfg(test)]
mod tests;
