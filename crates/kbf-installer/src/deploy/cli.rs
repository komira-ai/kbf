//! The command line of `kbf-installer deploy`: parse the flags, take the lock, read the
//! token, run the deploy and print its summary as the only line on standard output.

use std::fs;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::api::Http;
use super::summary::Exit;
use super::{Check, Env, NoCheck, NoSmoke, Options, Order, SystemClock, deploy, lock};
use crate::CommitId;

pub const USAGE: &str = "usage: kbf-installer deploy --commit <40hex> --api <127.0.0.1:port> \
--token-file <path> [--apply] [--json] [--bundle -|DIR] [--order nodes-first|frontend-first] \
[--allow-offline <id>[,<id>...]] [--node-timeout-s N] [--handover-timeout-s N] [--smoke-timeout-s N] \
[--lock <path>] [--platform linux-x86_64|linux-arm64|darwin-arm64]";

struct Args {
    options: Options,
    api: SocketAddr,
    token_file: PathBuf,
    bundle: Option<String>,
    lock: PathBuf,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut commit = None;
    let mut api = None;
    let mut token_file = None;
    let mut bundle = None;
    let mut apply = false;
    let mut order = Order::NodesFirst;
    let mut allow = Vec::new();
    let mut lock_path = PathBuf::from(super::LOCK);
    let mut platform = host_platform().to_owned();
    let mut secs = [1800u64, 900, 300];
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match flag.as_str() {
            "--apply" => apply = true,
            // The summary is always JSON; the flag is accepted so a caller can say so.
            "--json" => {}
            "--commit" => commit = Some(value("--commit")?),
            "--api" => api = Some(value("--api")?),
            "--token-file" => token_file = Some(PathBuf::from(value("--token-file")?)),
            "--bundle" => bundle = Some(value("--bundle")?),
            "--lock" => lock_path = PathBuf::from(value("--lock")?),
            "--platform" => platform = value("--platform")?,
            "--order" => {
                order = match value("--order")?.as_str() {
                    "nodes-first" => Order::NodesFirst,
                    "frontend-first" => Order::FrontendFirst,
                    other => {
                        return Err(format!("--order {other:?}: nodes-first or frontend-first"));
                    }
                }
            }
            "--allow-offline" => {
                allow.extend(value("--allow-offline")?.split(',').map(str::to_owned));
            }
            "--node-timeout-s" => {
                secs[0] = number("--node-timeout-s", &value("--node-timeout-s")?)?
            }
            "--handover-timeout-s" => {
                secs[1] = number("--handover-timeout-s", &value("--handover-timeout-s")?)?;
            }
            "--smoke-timeout-s" => {
                secs[2] = number("--smoke-timeout-s", &value("--smoke-timeout-s")?)?;
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    let commit = commit.ok_or("--commit is required")?;
    let commit = CommitId::new(&commit).map_err(|e| e.to_string())?;
    let api = api.ok_or("--api is required")?;
    let api: SocketAddr = api
        .parse()
        .map_err(|_| format!("--api {api:?} is not host:port"))?;
    if !matches!(
        platform.as_str(),
        "linux-x86_64" | "linux-arm64" | "darwin-arm64"
    ) {
        return Err(format!("--platform {platform:?} is not a platform"));
    }
    let mut options = Options::new(commit, &platform);
    options.apply = apply;
    options.order = order;
    options.allow_offline = allow;
    options.node_timeout = Duration::from_secs(secs[0]);
    options.handover_timeout = Duration::from_secs(secs[1]);
    options.smoke_timeout = Duration::from_secs(secs[2]);
    Ok(Args {
        options,
        api,
        token_file: token_file.ok_or("--token-file is required")?,
        bundle,
        lock: lock_path,
    })
}

fn number(name: &str, text: &str) -> Result<u64, String> {
    match text.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!(
            "{name} {text:?} is not a positive number of seconds"
        )),
    }
}

/// The platform this binary runs on, in the store's spelling.
pub fn host_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => "darwin-arm64",
        (_, "aarch64") => "linux-arm64",
        _ => "linux-x86_64",
    }
}

/// The token: a regular file owned by root, mode 0600 or 0400, one token of visible ASCII.
pub fn read_token(path: &Path) -> Result<String, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("token file: {e}"))?;
    if !meta.file_type().is_file() {
        return Err("the token file is not a regular file".to_owned());
    }
    if !matches!(meta.mode() & 0o7777, 0o600 | 0o400) {
        return Err("the token file must be mode 0600 or 0400".to_owned());
    }
    let text = fs::read_to_string(path).map_err(|e| format!("token file: {e}"))?;
    let token = text.trim();
    if token.len() < 32 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("the token file does not hold a token".to_owned());
    }
    Ok(token.to_owned())
}

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Run `kbf-installer deploy <args>`. The summary goes to `out` (one line, the only
/// output), log lines to `err`. Returns the exit code.
pub fn main(
    args: &[String],
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let usage = |err: &mut dyn Write, why: &str| {
        let _ = writeln!(err, "kbf-installer deploy: {why}\n{USAGE}");
        Exit::Usage.code()
    };
    let parsed = match parse(args) {
        Ok(p) => p,
        Err(why) => return usage(err, &why),
    };
    if parsed.options.apply && !is_root() {
        return usage(err, "--apply must run as root");
    }
    let token = match read_token(&parsed.token_file) {
        Ok(t) => t,
        Err(why) => return usage(err, &why),
    };
    let _held = match lock(&parsed.lock) {
        Ok(Some(held)) => held,
        Ok(None) => {
            let _ = writeln!(err, "kbf-installer deploy: another run holds the lock");
            return Exit::Busy.code();
        }
        Err(e) => return usage(err, &format!("the lock: {e}")),
    };
    let api = match Http::new(parsed.api, token, Duration::from_secs(60)) {
        Ok(a) => a,
        Err(e) => return usage(err, &e.to_string()),
    };
    // A bundle directory is not read yet; only a stream on standard input is.
    let input: Option<&mut dyn Read> = match parsed.bundle.as_deref() {
        None => None,
        Some("-") => Some(stdin),
        Some(_) => return usage(err, "--bundle takes - (a tar on standard input)"),
    };
    let (clock, smoke, check) = (SystemClock, NoSmoke, NoCheck);
    let env = Env {
        api: &api,
        clock: &clock,
        smoke: &smoke,
        check: &check as &dyn Check,
    };
    let _ = writeln!(err, "kbf-installer deploy: {}", parsed.options.commit);
    let summary = deploy(&env, &parsed.options, input);
    let _ = writeln!(out, "{}", summary.line());
    summary.exit
}
