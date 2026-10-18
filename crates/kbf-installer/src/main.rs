//! `kbf-installer run` and `kbf-installer boot-check` (see the library's docs). Configuration is
//! by flags only; the unit file that starts the helper is root's.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::thread::sleep;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use kbf_host::{HostExec, HostPath, Root};
use kbf_installer::activate;
use kbf_installer::helper::cycle::{Config, Helper};
use kbf_installer::helper::operator::{self, Failure, Operator};
use kbf_installer::helper::restart::Restart;
use kbf_installer::helper::serve::{FileIdentity, Flow, PASS_INTERVAL, Serve};
use kbf_installer::helper::service::HostProcs;
use kbf_installer::helper::{fetcher, leases};
use kbf_installer::layout::{Layout, Platform};

#[derive(Parser)]
#[command(name = "kbf-installer", version)]
struct Cli {
    /// The directory the host's paths are under (for a rehearsal; a host's is `/`).
    #[arg(long, global = true, default_value = "/")]
    root: PathBuf,
    /// Make the change. Without it, the verbs that change a host (`confirm`, `revert`,
    /// `rollback`, `gc`) make every check and say what they would do.
    #[arg(long, global = true)]
    apply: bool,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// Poll the order spool and act on it, until stopped. A pass every 5 seconds.
    Run(RunArgs),
    /// Run before each start of the service: count the start of a release on trial, and
    /// revert one that has used its starts. Exits 0 unless the host's state is unreadable.
    BootCheck(BootArgs),
    /// Keep the swap on trial that the activation (`<epoch>-<order id>`) made, if the new
    /// release is what runs. Exit 5 when no swap is pending.
    Confirm(Named),
    /// Put the host back on the release that ran before the swap on trial.
    Revert(Named),
    /// Put `current` on `previous`, with no window. Exit 5 when there is none or a swap
    /// is on trial (revert it).
    Rollback(Which),
    /// Remove the releases nothing needs: current, previous, staged, the swap on trial,
    /// and any a process executes from stay, and so do the newest `--keep` others.
    Gc(GcArgs),
    /// For a maintainer script: wait out a pass, exit 5 if a swap is on trial.
    Quiesce,
}

/// The service a swap restarts, by its own flag (never from an order).
#[derive(clap::Args)]
struct Which {
    /// The systemd unit to restart after a swap (Linux).
    #[arg(
        long,
        conflicts_with = "launchd_label",
        required_unless_present = "launchd_label"
    )]
    unit: Option<String>,
    /// The launchd label to kickstart after a swap (macOS).
    #[arg(long)]
    launchd_label: Option<String>,
}

impl Which {
    fn restart(self) -> Restart {
        // clap makes exactly one of the two flags present (`required_unless_present`,
        // `conflicts_with`), so the label is there whenever the unit is not.
        match self.unit {
            Some(unit) => Restart::Systemd(unit),
            None => Restart::Launchd(self.launchd_label.unwrap_or_default()),
        }
    }
}

#[derive(clap::Args)]
struct Named {
    /// The activation: `<epoch>-<order id>`, as `status.json` and the state name it.
    #[arg(long)]
    activation: String,
    #[command(flatten)]
    service: Which,
}

#[derive(clap::Args)]
struct GcArgs {
    /// The releases to keep besides the ones that must stay.
    #[arg(long, default_value_t = release_gc_keep())]
    keep: usize,
}

fn release_gc_keep() -> usize {
    kbf_installer::helper::cycle::GC_KEEP
}

#[derive(clap::Args)]
struct RunArgs {
    /// Root's directory: status.json goes here. Root-owned, mode 0755, so the node can read it.
    #[arg(long, default_value = "/var/lib/kbf-installer", value_parser = HostPath::new)]
    update_dir: HostPath,
    /// The order spool: owned by the service user, mode 0700, inside a root-owned parent.
    #[arg(long, default_value = "/var/lib/kbf-installer/orders", value_parser = HostPath::new)]
    orders_dir: HostPath,
    #[command(flatten)]
    service: Which,
    /// The confirm window the boot check gives a release on trial, in seconds.
    #[arg(long, default_value_t = 900, value_parser = clap::value_parser!(u64).range(120..=1800))]
    window_secs: u64,
    /// The starts a release on trial gets before it is reverted (Linux).
    #[arg(long, default_value_t = 3)]
    starts: u32,
    /// The fewest seconds between one activation and the next.
    #[arg(long, default_value_t = 60)]
    min_activation_interval_secs: u64,
    /// Stop after this many passes (a rehearsal or a test); without it, run until stopped.
    #[arg(long)]
    passes: Option<u64>,
}

#[derive(clap::Args)]
struct BootArgs {
    #[arg(long, default_value_t = 900, value_parser = clap::value_parser!(u64).range(120..=1800))]
    window_secs: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn host(root: PathBuf) -> Result<(Layout, HostExec), String> {
    let root = Root::new(root).map_err(|e| e.to_string())?;
    Ok((
        Layout::new(root.clone(), Platform::current()),
        HostExec::new(root),
    ))
}

fn run(root: PathBuf, args: RunArgs) -> Result<(), String> {
    let (layout, exec) = host(root)?;
    let platform = layout.platform();
    let cfg = Config {
        update_dir: args.update_dir,
        orders_dir: args.orders_dir,
        restart: args.service.restart(),
        window_secs: args.window_secs,
        starts: args.starts,
        min_activation_interval_secs: args.min_activation_interval_secs,
    };
    // The release fetcher and the lease source are wired by later changes; until then an
    // activation of a release that is not installed, and every activation, is refused.
    let helper = Helper::new(
        layout,
        exec,
        cfg,
        Box::new(fetcher::NotWired),
        Box::new(leases::NotWired),
        Box::new(HostProcs::new(platform)),
    );
    let me = std::env::current_exe().map_err(|e| format!("cannot find my own file: {e}"))?;
    let watch = FileIdentity::of(&me).map_err(|e| format!("cannot read {}: {e}", me.display()))?;
    let mut serve = Serve::new(helper, Box::new(watch));
    serve.helper().recover().map_err(|e| e.to_string())?;
    let mut left = args.passes;
    while left != Some(0) {
        match serve.turn(now()).map_err(|e| e.to_string())? {
            Flow::Continue => {}
            Flow::Reexec => {
                eprintln!("kbf-installer: my file was replaced; starting the new one");
                let err = Command::new(&me).args(std::env::args_os().skip(1)).exec();
                return Err(format!("cannot start {}: {err}", me.display()));
            }
        }
        serve.helper().drain_log(&mut std::io::stderr());
        left = left.map(|n| n - 1);
        if left != Some(0) {
            sleep(PASS_INTERVAL);
        }
    }
    Ok(())
}

fn boot_check(root: PathBuf, args: &BootArgs) -> Result<(), String> {
    let (layout, mut exec) = host(root)?;
    let mut out = Vec::new();
    let result = activate::boot_check(&mut exec, &layout, now(), args.window_secs, &mut out);
    eprint!("{}", String::from_utf8_lossy(&out));
    match result {
        Ok(decision) => {
            eprintln!("kbf-installer: boot check: {decision:?}");
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// A verb of the operator, run inline under the lock.
fn operate(
    root: PathBuf,
    apply: bool,
    restart: Restart,
    verb: impl FnOnce(&mut Operator<'_, HostExec>) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let (layout, mut exec) = host(root).map_err(Failure::Failed)?;
    let procs = HostProcs::new(layout.platform());
    let mut err = std::io::stderr();
    verb(&mut Operator {
        host: &layout,
        exec: &mut exec,
        restart: &restart,
        procs: &procs,
        out: &mut err,
        apply,
        wait: operator::LOCK_WAIT,
    })
}

fn gc(root: PathBuf, apply: bool, keep: usize) -> Result<(), Failure> {
    let (layout, mut exec) = host(root).map_err(Failure::Failed)?;
    let procs = HostProcs::new(layout.platform());
    operator::gc(&mut exec, &layout, &procs, keep, apply, &mut std::io::stderr())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let (root, apply) = (cli.root, cli.apply);
    let result = match cli.verb {
        Verb::Run(args) => run(root, args).map_err(Failure::Failed),
        Verb::BootCheck(args) => boot_check(root, &args).map_err(Failure::Failed),
        Verb::Confirm(n) => operate(root, apply, n.service.restart(), |o| {
            o.confirm(&n.activation)
        }),
        Verb::Revert(n) => operate(root, apply, n.service.restart(), |o| o.revert(&n.activation)),
        Verb::Rollback(w) => operate(root, apply, w.restart(), |o| o.rollback()),
        Verb::Gc(g) => gc(root, apply, g.keep),
        Verb::Quiesce => host(root)
            .map_err(Failure::Failed)
            .and_then(|(layout, _)| operator::quiesce(&layout, operator::LOCK_WAIT)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("kbf-installer: {failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}
