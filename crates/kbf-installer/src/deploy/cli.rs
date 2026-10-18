//! The command line of `kbf-installer deploy`: parse the flags, take the lock, read the
//! token, run the deploy and print its summary as the only line on standard output.

use std::fs;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::api::Http;
use super::smoke::CasSmoke;
use super::summary::Exit;
use super::summary::Summary;
use super::{
    Check, Clock, Env, NoCheck, NoSmoke, Options, Order, Smoke, SystemClock, deploy, lock, summary,
};
use crate::CommitId;

/// Where the host says which operator API to call and where its token is, so a caller
/// that may only run `deploy` names neither.
pub const CONFIG: &str = "/etc/kbf-installer/deploy.json";

pub const USAGE: &str = "usage: kbf-installer deploy --commit <40hex> [--api <127.0.0.1:port>] \
[--token-file <path>] [--config <path>] [--cas http://127.0.0.1:port] [--apply] [--json] [--bundle -|DIR] [--order nodes-first|frontend-first] \
[--allow-offline <id>[,<id>...]] [--node-timeout-s N] [--handover-timeout-s N] [--smoke-timeout-s N] \
[--lock <path>] [--platform linux-x86_64|linux-arm64|darwin-arm64]";

struct Args {
    options: Options,
    api: SocketAddr,
    cas: Option<String>,
    token_file: PathBuf,
    bundle: Option<String>,
    lock: PathBuf,
}

/// Flags that point the deploy at a different API, token, config, lock or platform. A caller
/// that got here through `sudo` (a deploy account that may only run `deploy`) must not set
/// them: root would read a file of the caller's choosing and send it to an address of the
/// caller's choosing. Those values come from the root-owned config.
const HOST_FLAGS: [&str; 6] = [
    "--api",
    "--token-file",
    "--config",
    "--lock",
    "--platform",
    "--cas",
];

fn parse(args: &[String], via_sudo: bool) -> Result<Args, String> {
    let mut commit = None;
    let mut api = None;
    let mut cas = None;
    let mut token_file = None;
    let mut bundle = None;
    let mut apply = false;
    let mut order = Order::NodesFirst;
    let mut allow = Vec::new();
    let mut lock_path = PathBuf::from(super::LOCK);
    let mut platform = host_platform().to_owned();
    let mut config = PathBuf::from(CONFIG);
    let mut secs = [1800u64, 900, 60];
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        if via_sudo && HOST_FLAGS.contains(&flag.as_str()) {
            return Err(format!(
                "{flag} is not accepted through sudo; the host's config sets it"
            ));
        }
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match flag.as_str() {
            "--apply" => apply = true,
            // The summary is always JSON; the flag is accepted so a caller can say so.
            "--json" => {}
            "--commit" => commit = Some(value("--commit")?),
            "--api" => api = Some(value("--api")?),
            "--cas" => cas = Some(value("--cas")?),
            "--token-file" => token_file = Some(PathBuf::from(value("--token-file")?)),
            "--bundle" => bundle = Some(value("--bundle")?),
            "--config" => config = PathBuf::from(value("--config")?),
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
    // The host's config fills what the flags leave out. A missing config file is fine when
    // both flags are given.
    let conf = read_config(
        &config,
        api.is_none() || token_file.is_none() || cas.is_none(),
    )?;
    let api = api.or(conf.api);
    let token_file = token_file.or(conf.token_file);
    let cas = cas.or(conf.cas);
    let commit = commit.ok_or("--commit is required")?;
    let commit = CommitId::new(&commit).map_err(|e| e.to_string())?;
    let api = api.ok_or("--api is required (or `api` in the config)")?;
    let api: SocketAddr = api
        .parse()
        .map_err(|_| format!("--api {api:?} is not host:port"))?;
    if !matches!(
        platform.as_str(),
        "linux-x86_64" | "linux-arm64" | "darwin-arm64"
    ) {
        return Err(format!("--platform {platform:?} is not a platform"));
    }
    if let Some(url) = &cas {
        CasSmoke::new(url, Duration::from_secs(secs[2])).map_err(|e| format!("--cas {e}"))?;
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
        cas,
        token_file: token_file.ok_or("--token-file is required (or `token_file` in the config)")?,
        bundle,
        lock: lock_path,
    })
}

/// What the host's config file says.
#[derive(Default)]
struct HostConfig {
    api: Option<String>,
    token_file: Option<PathBuf>,
    /// The local REAPI listener the CAS smoke uses.
    cas: Option<String>,
}

/// `{"api": "127.0.0.1:port", "token_file": "/path", "cas": "http://127.0.0.1:port"}`. Read
/// only when a flag is missing; it must be a regular file owned by root and writable by no
/// one else.
fn read_config(path: &Path, needed: bool) -> Result<HostConfig, String> {
    if !needed {
        return Ok(HostConfig::default());
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HostConfig::default()),
        Err(e) => return Err(format!("config {}: {e}", path.display())),
    };
    if !meta.file_type().is_file() || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "config {} must be a regular file no one else may write",
            path.display()
        ));
    }
    #[cfg(not(test))]
    if meta.uid() != 0 {
        return Err(format!("config {} must be owned by root", path.display()));
    }
    let text = fs::read_to_string(path).map_err(|e| format!("config: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("config: {e}"))?;
    let get = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    Ok(HostConfig {
        api: get("api"),
        token_file: get("token_file").map(PathBuf::from),
        cas: get("cas"),
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

/// The smoke a deploy ends with: against the local REAPI listener when the host names one,
/// else none (which a deploy reports as skipped, exit 5).
pub(super) fn smoke_for(cas: Option<&str>, timeout: Duration) -> Result<Box<dyn Smoke>, String> {
    match cas {
        None => Ok(Box::new(NoSmoke)),
        Some(url) => CasSmoke::new(url, timeout)
            .map(|s| Box::new(s) as Box<dyn Smoke>)
            .map_err(|e| format!("--cas {e}")),
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
    // sudo sets SUDO_UID to the caller's uid and a caller cannot unset it for the command.
    let via_sudo = std::env::var_os("SUDO_UID").is_some_and(|u| u != "0");
    main_as(args, stdin, out, err, via_sudo)
}

/// [`main`], told whether the caller came through `sudo`.
pub fn main_as(
    args: &[String],
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
    via_sudo: bool,
) -> i32 {
    let usage = |err: &mut dyn Write, why: &str| {
        let _ = writeln!(err, "kbf-installer deploy: {why}\n{USAGE}");
        Exit::Usage.code()
    };
    let parsed = match parse(args, via_sudo) {
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
            // The one line on standard output is there for every end, this one too.
            let mut sum = Summary::new(&parsed.options.commit, SystemClock.now());
            sum.ended = summary::utc(SystemClock.now());
            sum.end(Exit::Busy, "lock_held", "another run holds the lock");
            let _ = writeln!(out, "{}", sum.line());
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
    let smoke = match smoke_for(parsed.cas.as_deref(), parsed.options.smoke_timeout) {
        Ok(s) => s,
        Err(why) => return usage(err, &why),
    };
    let (clock, check) = (SystemClock, NoCheck);
    let env = Env {
        api: &api,
        clock: &clock,
        smoke: &*smoke,
        check: &check as &dyn Check,
    };
    let _ = writeln!(err, "kbf-installer deploy: {}", parsed.options.commit);
    let summary = deploy(&env, &parsed.options, input);
    let _ = writeln!(out, "{}", summary.line());
    summary.exit
}
