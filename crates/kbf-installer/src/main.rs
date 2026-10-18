//! `kbf-installer run` and `kbf-installer boot-check` (see the library's docs). Configuration is
//! by flags only; the unit file that starts the helper is root's.

use std::path::PathBuf;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use kbf_host::{HostExec, HostPath, HttpUrl, Root};
use kbf_installer::activate;
use kbf_installer::frontend::probe::RunnerProbe;
use kbf_installer::frontend::{self, Frontend, FrontendConfig};
use kbf_installer::helper::cycle::{Config, Helper};
use kbf_installer::helper::restart::Restart;
use kbf_installer::helper::{fetcher, leases};
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::run::SystemRunner;

#[derive(Parser)]
#[command(name = "kbf-installer", version)]
struct Cli {
    /// The directory the host's paths are under (for a rehearsal; a host's is `/`).
    #[arg(long, global = true, default_value = "/")]
    root: PathBuf,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// Poll the order spool and act on it, until stopped.
    Run(Box<RunArgs>),
    /// Run before each start of the service: count the start of a release on trial, and
    /// revert one that has used its starts. Exits 0 unless the host's state is unreadable.
    BootCheck(BootArgs),
}

#[derive(clap::Args)]
struct RunArgs {
    /// Root's directory: status.json goes here. Root-owned, mode 0755, so the node can read it.
    #[arg(long, default_value = "/var/lib/kbf-installer", value_parser = HostPath::new)]
    update_dir: HostPath,
    /// The order spool: owned by the service user, mode 0700, inside a root-owned parent.
    #[arg(long, default_value = "/var/lib/kbf-installer/orders", value_parser = HostPath::new)]
    orders_dir: HostPath,
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
    /// Seconds between passes.
    #[arg(long, default_value_t = 3)]
    interval_secs: u64,
    /// The confirm window the boot check gives a release on trial (120 to 1800).
    #[arg(long, default_value_t = 900)]
    window_secs: u64,
    /// The starts a release on trial gets before it is reverted (Linux).
    #[arg(long, default_value_t = 3)]
    starts: u32,
    /// Instance a's `/readyz` on loopback. With instance b's, this host updates its frontend
    /// (`frontend_update` records); without them such a record is refused.
    #[arg(long, value_parser = frontend::probe::loopback, requires = "frontend_readyz_b")]
    frontend_readyz_a: Option<HttpUrl>,
    /// Instance b's `/readyz`.
    #[arg(long, value_parser = frontend::probe::loopback, requires = "frontend_readyz_a")]
    frontend_readyz_b: Option<HttpUrl>,
    /// The confirm window a frontend update gets (120 to 1800).
    #[arg(long, default_value_t = 900)]
    frontend_window_secs: u64,
    /// The passes in a row the new frontend instance must be ready before it is kept.
    #[arg(long, default_value_t = 3)]
    frontend_looks: u32,
    /// The longest undoing a frontend update waits for the old instance to stop and come back
    /// before an operator is asked.
    #[arg(long, default_value_t = 300)]
    frontend_revert_bound_secs: u64,
    /// Stop after this many passes (a rehearsal or a test); without it, run until stopped.
    #[arg(long)]
    passes: Option<u64>,
}

#[derive(clap::Args)]
struct BootArgs {
    #[arg(long, default_value_t = 900)]
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
    // clap makes exactly one of the two flags present (`required_unless_present`,
    // `conflicts_with`), so the label is there whenever the unit is not.
    let restart = match args.unit {
        Some(unit) => Restart::Systemd(unit),
        None => Restart::Launchd(args.launchd_label.unwrap_or_default()),
    };
    let cfg = Config {
        update_dir: args.update_dir,
        orders_dir: args.orders_dir,
        restart,
        window_secs: args.window_secs,
        starts: args.starts,
    };
    // The release fetcher and the lease source are wired by later changes; until then an
    // activation of a release that is not installed, and every activation, is refused.
    let mut helper = Helper::new(
        layout,
        exec,
        cfg,
        Box::new(fetcher::NotWired),
        Box::new(leases::NotWired),
    );
    if let (Some(a), Some(b)) = (args.frontend_readyz_a, args.frontend_readyz_b) {
        let cfg = FrontendConfig {
            readyz_a: a,
            readyz_b: b,
            window_s: args.frontend_window_secs,
            looks: args.frontend_looks,
            revert_bound_s: args.frontend_revert_bound_secs,
        };
        let probe = RunnerProbe {
            runner: SystemRunner,
            platform: Platform::current(),
        };
        helper = helper.with_frontend(Frontend::new(cfg, Box::new(probe)));
    }
    helper.recover().map_err(|e| e.to_string())?;
    let mut left = args.passes;
    while left != Some(0) {
        helper.cycle(now()).map_err(|e| e.to_string())?;
        helper.drain_log(&mut std::io::stderr());
        left = left.map(|n| n - 1);
        sleep(Duration::from_secs(args.interval_secs));
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.verb {
        Verb::Run(args) => run(cli.root, *args),
        Verb::BootCheck(args) => boot_check(cli.root, &args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("kbf-installer: {why}");
            ExitCode::FAILURE
        }
    }
}
