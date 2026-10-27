//! The command line.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// The VM helper for macOS VM leases: feasibility probes and the base-image builder.
///
/// Every command that touches the hypervisor runs on macOS only. It prints one JSON
/// object on stdout.
#[derive(Debug, Parser)]
#[command(name = "kbf-vmm", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Run one feasibility gate and print its result as one JSON object.
    Probe(ProbeArgs),
    /// Time clone, boot and teardown, cold and warm, and print median and max as JSON.
    ProbeBench(BenchArgs),
    /// Build a base VM bundle.
    #[command(subcommand)]
    Image(ImageCmd),
    /// Run one VM from a spec file; events are JSON lines on stdout; stdin closing stops
    /// the VM. The probes start this as a child process.
    #[command(hide = true)]
    VmRun {
        #[arg(long)]
        spec: PathBuf,
    },
}

/// The gates `probe` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Gate {
    /// Clone a base bundle: same machine identifier twice at once; a new identifier.
    G1,
    /// Which channel carries bytes from a guest process to the host.
    G2,
    /// Whether mounting a custom virtiofs tag in the guest needs root.
    G3,
    /// Start a VM from a launch-daemon child, nobody logged in, as a throwaway uid.
    #[value(alias = "p5")]
    G5,
    /// Why a booted guest never reaches its own userspace: two guests at once, every
    /// helper event timed, CPU and disk writes sampled, the clones kept for `image
    /// harvest` (give `--guest-binary` and `--keep`).
    E1,
    /// The iOS simulator inside a guest: first launch of Xcode, a runtime added, a device
    /// created, booted and photographed, one result frame per step (give `--guest-binary`;
    /// the base needs `image setup --xcode --sim-runtime`).
    E3,
    /// Whether a root offline write into a guest volume works.
    P0,
    /// The third-VM limit error, and the slot after SIGKILL of a helper.
    P2,
    /// Whether an ad hoc hardened-runtime signature with the entitlement runs a VM.
    P6,
}

/// What every probe needs.
#[derive(Debug, Args)]
pub struct Common {
    /// Where clones, shared folders and logs go. Must be on the volume of the base
    /// bundle: `clonefile` does not cross volumes.
    #[arg(long)]
    pub work_dir: PathBuf,
    /// The prepared base bundle (default: `<work-dir>/base`).
    #[arg(long)]
    pub base: Option<PathBuf>,
    /// A `kbf-guest` built for macOS, copied into the folder shared with each guest.
    #[arg(long)]
    pub guest_binary: Option<PathBuf>,
    /// Start the probe VMs without a graphics device. Without this flag they have one,
    /// because a headless guest crash-loops its window server; use this to compare.
    /// (E1's first arm is headless either way.)
    #[arg(long)]
    pub headless: bool,
    /// Do not attach the share matrix to E1's VMs (about ten extra virtiofs devices, each
    /// with a host directory that differs in one factor; see `docs/spikes/macos-vm-probe.md`).
    #[arg(long)]
    pub no_matrix: bool,
}

#[derive(Debug, Args)]
pub struct ProbeArgs {
    #[arg(value_enum, ignore_case = true)]
    pub gate: Gate,
    #[command(flatten)]
    pub common: Common,
    /// The user id G5 starts the helper as (default 59001); needs root.
    #[arg(long)]
    pub run_as_uid: Option<u32>,
    /// Leave the clones and their shares in the work directory when the probe ends, so
    /// `image harvest` can read the guest's disk.
    #[arg(long)]
    pub keep: bool,
    /// E3 only: which guests to run, one after the other: `on` (a graphics device, the
    /// default, unless `--headless`), `off` (none) or `both` (`off` first, then `on`).
    #[arg(long, value_enum)]
    pub graphics: Option<Graphics>,
}

/// The guests `probe e3` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Graphics {
    On,
    Off,
    /// Without graphics first, then with.
    Both,
}

/// A user name `--guest-user` accepts.
fn parse_guest_user(name: &str) -> Result<String, String> {
    kbf_guest::mkuser::check_name(name).map(|()| name.to_owned())
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    #[command(flatten)]
    pub common: Common,
    /// Runs per measure, cold and warm each.
    #[arg(long, default_value_t = 20)]
    pub runs: u32,
}

#[derive(Debug, Subcommand)]
pub enum ImageCmd {
    /// Install macOS from a restore image into a new base bundle.
    Base {
        /// The restore image: an https URL or a path to an .ipsw.
        #[arg(long)]
        ipsw: String,
        /// The SHA-256 the restore image must have (64 hex digits). A mismatch is refused.
        #[arg(long)]
        sha256: String,
        /// The new bundle directory; it must not exist.
        #[arg(long)]
        out: PathBuf,
        /// The guest disk's size. Xcode, its simulator runtimes and a simulator need far
        /// more than the default; `image setup --xcode` refuses a disk that is too small.
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(32..=4096))]
        disk_gib: u64,
        /// The guest's CPU count (default: what the restore image needs, at least 2). A
        /// value below the restore image's minimum is refused. The base records it.
        #[arg(long, value_parser = clap::value_parser!(u32).range(2..=64))]
        cpus: Option<u32>,
        /// The guest's memory in MiB (default: what the restore image needs, at least
        /// 4096). A value below the restore image's minimum is refused. The base records it.
        #[arg(long, value_parser = clap::value_parser!(u64).range(4096..=524_288))]
        memory_mib: Option<u64>,
        /// As root, write the Setup Assistant marker and the probe launch daemons into
        /// the installed image (probe P0's question).
        #[arg(long)]
        offline_setup: bool,
        /// The `kbf-guest` for macOS that `--offline-setup` copies onto the guest's disk
        /// (the mount daemon runs it); required with `--offline-setup`.
        #[arg(long)]
        guest_binary: Option<PathBuf>,
    },
    /// As root, write the probe launch daemons and the Setup Assistant marker into an
    /// existing base bundle (the `--offline-setup` step, for a bundle built without it).
    Setup {
        /// The base bundle directory.
        #[arg(long)]
        base: PathBuf,
        /// The `kbf-guest` for macOS to copy onto the guest's disk; the mount daemon runs it.
        #[arg(long)]
        guest_binary: Option<PathBuf>,
        /// An Xcode.app to clone into the guest's /Applications/Xcode.app (a copy-on-write
        /// clone where the filesystems allow, else a copy; owner root:wheel). The guest
        /// disk needs the room: the free space is checked first. A symbolic link in the
        /// app is copied as a link when its target stays inside the app, else refused.
        #[arg(long)]
        xcode: Option<PathBuf>,
        /// A simulator runtime to stage under /private/var/kbf-sim/runtimes: a disk image
        /// (`<name>.dmg`, a file) or an exported bundle (`<name>.exportedBundle`, a directory,
        /// what `xcodebuild -downloadPlatform iOS -exportPath` writes). Repeat the flag for
        /// several (at most 8, with different names); the directory then holds exactly these
        /// (a setup that names none empties it). The report records each image's name, size
        /// and SHA-256, and each bundle's files with a SHA-256 for each.
        #[arg(long = "sim-runtime", value_name = "DMG_OR_BUNDLE")]
        sim_runtimes: Vec<PathBuf>,
        /// An app bundle (`<name>.app`, built for the iOS simulator, with an Info.plist) to
        /// stage under /private/var/kbf-sim/apps for `probe e3` to install and launch; copied
        /// by the same rules as --xcode. The directory then holds exactly this app.
        #[arg(long)]
        sim_app: Option<PathBuf>,
        /// The UI tests to stage under /private/var/kbf-sim/tests: the `Build/Products`
        /// directory of `xcodebuild build-for-testing -project <p> -scheme <s> -sdk
        /// iphonesimulator -destination 'generic/platform=iOS Simulator' -derivedDataPath
        /// <dd> CODE_SIGNING_ALLOWED=NO` (`<dd>/Build/Products`), which holds exactly one
        /// `*.xctestrun` and the built apps. Copied by the same rules as --sim-app (links
        /// that leave the tree as for --allow-external-links), each file hashed in the report.
        /// `probe e3` then runs the tests after launching the app. The directory then holds
        /// exactly these tests (a setup that names none removes the earlier ones).
        #[arg(long, value_name = "PRODUCTS_DIR")]
        sim_tests: Option<PathBuf>,
        /// Copy symbolic links inside Xcode.app (or the app) whose target leaves the app (an absolute
        /// path elsewhere, or more `..` than the link is deep) as links, and list them in
        /// the report. Without this flag such a link is refused.
        #[arg(long)]
        allow_external_links: bool,
        /// The one non-admin user the guest is to have, with auto-login, so that the
        /// simulator's user steps have a session (a short name: lowercase letters, digits,
        /// `-`, `_`). Stages a launch daemon that creates it in the first boot, which
        /// `image prime` runs; until then the simulator probe waits for it. Needs the
        /// `kbf-guest` the guest disk holds (from this or an earlier setup with
        /// `--guest-binary`).
        #[arg(long, value_name = "NAME", value_parser = parse_guest_user)]
        guest_user: Option<String>,
    },
    /// Boot the base bundle itself, as the console user and with graphics, so that the
    /// guest creates its user (`image setup --guest-user`) and powers itself off; the
    /// result frames go to `<base>/prime-results`. Afterwards every clone of the base
    /// boots into that user's session. Run again on a base whose user is ready and whose simulator
    /// root setup is done, it stops the guest at once; with part of the setup still to do (a
    /// runtime added since), it does that part and the guest powers off.
    Prime {
        /// The base bundle directory (set up with `--guest-user`).
        #[arg(long)]
        base: PathBuf,
        /// A `kbf-guest` built for macOS, copied into the folder shared with the guest.
        #[arg(long)]
        guest_binary: PathBuf,
        /// How long to wait for the guest to power off before it is stopped and the run is
        /// reported as a timeout.
        #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..=86_400))]
        deadline_secs: u64,
        /// How long a guest that is still up at the deadline (or already prepared) has to
        /// shut down after being asked to, before the host stops it by force. A forced stop
        /// can leave the base's disk unflushed: copy a base that cannot be rebuilt before
        /// priming it.
        #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=3600))]
        shutdown_secs: u64,
    },
    /// As root, copy a fixed allowlist of diagnostics (the probe daemons' logs, the local
    /// marker, the unified log's data files, crash reports) out of a stopped guest's disk
    /// into a new directory. The disk is attached read-only; nothing else is read.
    Harvest {
        /// The bundle (a clone kept by `probe e1 --keep`).
        #[arg(long)]
        bundle: PathBuf,
        /// The new directory to copy into; it must not exist.
        #[arg(long)]
        out: PathBuf,
    },
}
