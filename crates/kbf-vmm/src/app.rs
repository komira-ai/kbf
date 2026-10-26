//! Dispatch: runs the command the line names and prints its JSON.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use crate::cli::{BenchArgs, Cli, Cmd, Common, Gate, ImageCmd, ProbeArgs};
use crate::image::{BaseArgs, SetupArgs};
use crate::probe::{Ctx, Timing};
use crate::report::{Report, Status};
use crate::system::System;
use crate::{bench, e3, gates, image, prime};

fn name_of(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Probe(_) => "probe",
        Cmd::ProbeBench(_) => "probe-bench",
        Cmd::Image(_) => "image",
        Cmd::VmRun { .. } => "vm-run",
    }
}

fn context<'a>(
    sys: &'a dyn System,
    common: &Common,
    timing: Timing,
    keep: bool,
) -> Result<Ctx<'a>, String> {
    std::fs::create_dir_all(&common.work_dir)
        .map_err(|e| format!("create {}: {e}", common.work_dir.display()))?;
    // Absolute, because a helper started as another user need not share the directory.
    let work = std::path::absolute(&common.work_dir).unwrap_or_else(|_| common.work_dir.clone());
    let base = common.base.clone().unwrap_or_else(|| work.join("base"));
    Ok(Ctx {
        sys,
        work,
        base,
        guest_binary: common.guest_binary.clone(),
        keep,
        graphics: !common.headless,
        matrix: !common.no_matrix,
        timing,
    })
}

fn probe(sys: &dyn System, args: &ProbeArgs, timing: Timing) -> Report {
    if args.graphics.is_some() && args.gate != Gate::E3 {
        return Report::error("probe", "--graphics applies to the e3 probe only");
    }
    match context(sys, &args.common, timing, args.keep) {
        Ok(ctx) => match (args.gate, args.graphics) {
            (Gate::E3, Some(mode)) => e3::e3(&ctx, mode),
            _ => gates::run(&ctx, args.gate, args.run_as_uid),
        },
        Err(e) => Report::error("probe", e),
    }
}

/// `image prime`: the base is the work directory, so the share and the frames land inside it.
fn prime_cmd(
    sys: &dyn System,
    base: &Path,
    guest_binary: &Path,
    deadline: Duration,
    timing: Timing,
) -> Report {
    let work = std::path::absolute(base).unwrap_or_else(|_| base.to_owned());
    let ctx = Ctx {
        sys,
        work: work.clone(),
        base: work,
        guest_binary: Some(guest_binary.to_owned()),
        keep: false,
        graphics: true,
        matrix: false,
        timing,
    };
    prime::prime(&ctx, deadline)
}

fn bench_cmd(sys: &dyn System, args: &BenchArgs, timing: Timing) -> Report {
    match context(sys, &args.common, timing, false) {
        Ok(ctx) => bench::run(&ctx, args.runs),
        Err(e) => Report::error("probe-bench", e),
    }
}

/// Runs the command. `sys` is `None` where there is no hypervisor to drive (a platform
/// other than macOS): everything but `--help` and `--version` then refuses with status 2.
pub fn run(
    cli: Cli,
    sys: Option<&dyn System>,
    timing: Timing,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let Some(sys) = sys else {
        let _ = writeln!(
            err,
            "kbf-vmm: `{}` needs macOS with Virtualization.framework (the commands that touch a VM run on macOS only)",
            name_of(&cli.command)
        );
        return ExitCode::from(2);
    };
    let report = match &cli.command {
        Cmd::VmRun { spec } => return vm_run(sys, spec, err),
        Cmd::Probe(args) => probe(sys, args, timing),
        Cmd::ProbeBench(args) => bench_cmd(sys, args, timing),
        Cmd::Image(ImageCmd::Base {
            ipsw,
            sha256,
            out,
            disk_gib,
            cpus,
            memory_mib,
            offline_setup,
            guest_binary,
        }) => image::base(
            sys,
            &BaseArgs {
                ipsw: ipsw.clone(),
                sha256: sha256.clone(),
                out: out.clone(),
                disk_gib: *disk_gib,
                cpus: *cpus,
                memory_mib: *memory_mib,
                offline_setup: *offline_setup,
                guest_binary: guest_binary.clone(),
            },
        ),
        Cmd::Image(ImageCmd::Setup {
            base,
            guest_binary,
            xcode,
            sim_runtimes,
            sim_app,
            allow_external_links,
            guest_user,
        }) => image::setup(
            sys,
            base,
            &SetupArgs {
                guest_binary: guest_binary.clone(),
                xcode: xcode.clone(),
                sim_runtimes: sim_runtimes.clone(),
                sim_app: sim_app.clone(),
                allow_external_links: *allow_external_links,
                guest_user: guest_user.clone(),
            },
        ),
        Cmd::Image(ImageCmd::Prime {
            base,
            guest_binary,
            deadline_secs,
            shutdown_secs,
        }) => prime_cmd(
            sys,
            base,
            guest_binary,
            Duration::from_secs(*deadline_secs),
            Timing {
                shutdown_timeout: Duration::from_secs(*shutdown_secs),
                ..timing
            },
        ),
        Cmd::Image(ImageCmd::Harvest { bundle, out }) => image::harvest(sys, bundle, out),
    };
    let failed = report.status == Status::Error;
    let _ = writeln!(out, "{}", report.on_host(sys.host_facts()).to_line());
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The helper mode: its events go to stdout as the VM runs.
fn vm_run(sys: &dyn System, spec: &Path, err: &mut dyn Write) -> ExitCode {
    let parsed = std::fs::read_to_string(spec)
        .map_err(|e| format!("read {}: {e}", spec.display()))
        .and_then(|t| serde_json::from_str(&t).map_err(|e| format!("{}: {e}", spec.display())));
    match parsed.and_then(|s| sys.run_vm_helper(&s)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "kbf-vmm vm-run: {e}");
            ExitCode::FAILURE
        }
    }
}
