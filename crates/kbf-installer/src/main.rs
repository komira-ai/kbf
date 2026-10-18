//! `kbf-installer run` and `kbf-installer boot-check` (see the library's docs). Configuration is
//! by flags only; the unit file that starts the helper is root's.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use kbf_host::{HostExec, HostPath, Root};
use kbf_installer::activate;
use kbf_installer::helper::cycle::{Config, Helper};
use kbf_installer::helper::restart::Restart;
use kbf_installer::helper::{fetcher, leases};
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::migrate;

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
    Run(RunArgs),
    /// Run before each start of the service: count the start of a release on trial, and
    /// revert one that has used its starts. Exits 0 unless the host's state is unreadable.
    BootCheck(BootArgs),
    /// Adopt the old layout (/usr/local/kbf, the deploy state) into the installer's tree,
    /// or put it back (`--rollback`). A dry run without --apply. `setup` calls the same
    /// code once it exists; the test-farm hosts that run the dev installer use this verb.
    Migrate(MigrateArgs),
}

#[derive(clap::Args)]
struct MigrateArgs {
    /// Do it; without this, print what would be done.
    #[arg(long)]
    apply: bool,
    /// Undo a migration instead (refused once the installer has moved the host on).
    #[arg(long)]
    rollback: bool,
    /// A unit that runs kbf from /usr/local/kbf/current, to repoint (repeat for several;
    /// `.service`, or `.plist` for launchd).
    #[arg(long = "unit", value_parser = migrate::Unit::new)]
    units: Vec<migrate::Unit>,
    /// The old helper's unit: stopped and disabled, so the installer is the only manager.
    #[arg(long, value_parser = migrate::Unit::new)]
    helper_unit: Option<migrate::Unit>,
    /// The installer's own unit, which a rollback stops and disables first.
    #[arg(long, value_parser = migrate::Unit::new)]
    installer_unit: Option<migrate::Unit>,
    /// Where the host gets its releases: decides where the generation floor is seeded from.
    #[arg(long, value_enum, default_value_t = ReleaseSource::Url)]
    release_source: ReleaseSource,
    /// The lowest generation an order may carry (the floor, when there is no frontend to ask).
    #[arg(long)]
    min_generation: Option<u64>,
    /// The unprivileged user the old binaries' `--version --json` runs as.
    #[arg(long, default_value = "kbf-fetch")]
    fetch_user: String,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ReleaseSource {
    Url,
    Frontend,
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

/// The user that must own every file the migration reads and writes. On a host every file
/// belongs to root. Under `--root` (a rehearsal, a test) the tree is the caller's own, so
/// the owner to expect is the caller: nobody gains anything by it, since root and a
/// stranger alike only reach what they already could.
fn owner_uid(rehearsal: bool, euid: u32) -> u32 {
    if rehearsal { euid } else { 0 }
}

/// The user the version probe runs as: the exit code and the reason it failed with when the
/// host cannot say (1) or has no such user (2).
fn probe_user<E: kbf_host::Exec>(exec: &E, name: &str) -> Result<kbf_host::RunUser, (u8, String)> {
    let found = kbf_host::user::lookup(exec, name).map_err(|e| (1, e.to_string()))?;
    found.ok_or_else(|| (2, format!("no user {name:?} to run the version probe as")))
}

/// `kbf-installer migrate`: the exit code and the reason it failed with.
fn migrate_verb(root: PathBuf, args: MigrateArgs) -> Result<(), (u8, String)> {
    let rehearsal = root != Path::new("/");
    let (layout, mut exec) = host(root).map_err(|e| (1, e))?;
    let user = probe_user(&exec, &args.fetch_user)?;
    let m = migrate::Migration {
        roots: migrate::Roots::new(layout.platform()),
        layout,
        run_as: kbf_installer::run::RunAs {
            uid: user.uid,
            gid: user.gid,
        },
        owner_uid: owner_uid(rehearsal, rustix::process::geteuid().as_raw()),
        units: args.units,
        helper_unit: args.helper_unit,
        installer_unit: args.installer_unit,
        seed: migrate::Seed {
            source: match args.release_source {
                ReleaseSource::Url => migrate::Source::Url,
                ReleaseSource::Frontend => migrate::Source::Frontend,
            },
            min_generation: args.min_generation,
        },
    };
    let mode = if args.apply {
        kbf_host::Mode::Apply
    } else {
        kbf_host::Mode::DryRun
    };
    let report = if args.rollback {
        migrate::rollback(&m, &mut exec, mode)
    } else {
        // The verify pipeline and the frontend client are later changes (contract P8, P7):
        // until then every release is adopted unverified, and the report says so.
        let hooks = migrate::Hooks {
            runner: &kbf_installer::run::SystemRunner,
            verifier: &migrate::NotWired,
            frontend: &migrate::NoFrontend,
        };
        migrate::migrate(&m, &mut exec, &hooks, mode)
    }
    .map_err(|e| (e.exit_code(), e.to_string()))?;
    print!("{report}");
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.verb {
        Verb::Migrate(args) => {
            return match migrate_verb(cli.root, args) {
                Ok(()) => ExitCode::SUCCESS,
                Err((code, why)) => {
                    eprintln!("kbf-installer: {why}");
                    ExitCode::from(code)
                }
            };
        }
        Verb::Run(args) => run(cli.root, args),
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

#[cfg(test)]
mod tests {
    use kbf_host::testkit::{FakeExec, reply};

    use super::*;

    /// Catches the owner a migration trusts being root's only by accident: on a host it is
    /// uid 0 whoever runs the verb; under `--root` it is the caller, who owns the rehearsal.
    #[test]
    fn the_owner_is_root_on_a_host_and_the_caller_in_a_rehearsal() {
        assert_eq!(owner_uid(false, 1000), 0);
        assert_eq!(owner_uid(true, 1000), 1000);
    }

    /// Catches the probe user's lookup failing quietly: a host that cannot answer is exit 1
    /// with the reason, an unknown user exit 2 naming the user, a known one its ids.
    #[test]
    fn the_probe_user_is_looked_up_with_the_exit_codes_of_the_contract() {
        let mut exec = FakeExec::new(0);
        exec.answer = Box::new(|cmd| {
            let who = cmd.args.last().map(String::as_str);
            Ok(match (cmd.program.as_str().ends_with("getent"), who) {
                (true, Some("kbf-fetch")) => {
                    reply(0, "kbf-fetch:x:991:991::/nonexistent:/sbin/nologin\n", "")
                }
                (true, Some("broken")) => reply(1, "", "getent: no such service"),
                (true, _) => reply(2, "", ""),
                (false, _) => reply(0, "991\n", ""),
            })
        });
        assert_eq!(probe_user(&exec, "kbf-fetch").expect("found").uid, 991);
        let (code, why) = probe_user(&exec, "nobody-here").expect_err("unknown");
        assert_eq!(code, 2);
        assert!(why.contains("nobody-here"), "{why}");
        let (code, why) = probe_user(&exec, "broken").expect_err("broken");
        assert_eq!(code, 1);
        assert!(why.contains("getent"), "{why}");
    }
}
