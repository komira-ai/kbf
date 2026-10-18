//! What the binary does with a parsed command line: the verbs that exist run, the ones a later
//! pull request builds say so (`not implemented (P<n>)`, exit 1) after the checks every verb
//! shares (root, `--json`, `--apply`).
//!
//! `run` and `boot-check` are the helper's, folded in unchanged; `P5` replaces them.

use std::io::Write;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_host::{HostExec, Root};

use crate::activate;
use crate::cli::{BootArgs, Cli, RunArgs, Verb};
use crate::exit::Exit;
use crate::helper::cycle::{Config, Helper};
use crate::helper::restart::Restart;
use crate::layout::{Layout, Platform};
use crate::release;
use crate::run::SystemRunner;
use crate::settings::Settings;
use crate::stage::{Context, Limits, stage_from_inbox};

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

fn run(root: PathBuf, args: &RunArgs) -> Result<(), String> {
    let (layout, exec) = host(root)?;
    // clap makes exactly one of the two flags present (`required_unless_present`,
    // `conflicts_with`), so the label is there whenever the unit is not.
    let restart = match &args.unit {
        Some(unit) => Restart::Systemd(unit.clone()),
        None => Restart::Launchd(args.launchd_label.clone().unwrap_or_default()),
    };
    let cfg = Config {
        update_dir: args.update_dir.clone(),
        restart,
        window_secs: args.window_secs,
        starts: args.starts,
    };
    let mut helper = Helper::new(layout.clone(), exec, cfg);
    let trust = HostExec::new(layout.root().clone());
    let mut stager = move |exec: &mut HostExec, entry: &std::path::Path| {
        let settings = Settings::load(&trust, &layout)?;
        let mut ctx = Context {
            host: &layout,
            settings: &settings,
            exec,
            trust: &trust,
            runner: &SystemRunner,
            owner: release::ROOT,
            limits: Limits::DEFAULT,
        };
        stage_from_inbox(&mut ctx, entry)
    };
    helper.recover().map_err(|e| e.to_string())?;
    let mut left = args.passes;
    while left != Some(0) {
        helper
            .cycle(now(), &mut stager)
            .map_err(|e| e.to_string())?;
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

/// The directory the host's paths are under: `/`, except in a build with the sandbox flag.
fn root_of(cli: &Cli) -> PathBuf {
    #[cfg(feature = "test-sandbox")]
    return cli.root.clone();
    #[cfg(not(feature = "test-sandbox"))]
    {
        let _ = cli;
        PathBuf::from("/")
    }
}

/// Carry out `cli` as the user `euid`; what is printed for the person or `--json` goes to
/// `out`, what went wrong to `err`.
pub fn dispatch(cli: &Cli, euid: u32, out: &mut dyn Write, err: &mut dyn Write) -> Exit {
    let name = cli.verb.name();
    let mut say = |code: Exit, text: String, json: serde_json::Value| {
        // A closed pipe is the reader's business; the exit code still tells.
        let _ = if cli.json {
            writeln!(out, "{json}")
        } else if code == Exit::Ok {
            writeln!(out, "{text}")
        } else {
            writeln!(err, "{text}")
        };
        code
    };
    // `run` and `boot-check` keep the helper's own checks (they work on a scratch root as an
    // ordinary user) until P5 replaces them; every other verb is gated here.
    let gated = cli.verb.built_by().is_some();
    if gated && cli.verb.needs_root() && euid != 0 {
        return say(
            Exit::Usage,
            format!("kbf-installer: {name} needs root: run it with sudo (effective uid is {euid})"),
            serde_json::json!({"verb": name, "error": "needs_root", "euid": euid}),
        );
    }
    let result = match &cli.verb {
        Verb::Run(args) => run(root_of(cli), args),
        Verb::BootCheck(args) => boot_check(root_of(cli), args),
        other => {
            let phase = other.built_by().unwrap_or("?");
            return say(
                Exit::Failed,
                format!("kbf-installer: {name}: not implemented ({phase})"),
                serde_json::json!({
                    "verb": name,
                    "apply": cli.apply,
                    "error": "not_implemented",
                    "phase": phase,
                }),
            );
        }
    };
    match result {
        Ok(()) => Exit::Ok,
        Err(why) => say(
            Exit::Failed,
            format!("kbf-installer: {why}"),
            serde_json::json!({"verb": name, "error": "failed", "message": why}),
        ),
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
