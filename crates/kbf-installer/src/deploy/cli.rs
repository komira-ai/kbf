//! The command line of `kbf-installer deploy`: parse the flags, take the lock, read the
//! token, run the deploy and print its summary as the only line on standard output.

use std::fs;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::api::{Https, Trust};
use super::frontend_addr::derive;
use super::smoke::{CasSmoke, random};
use super::summary::Exit;
use super::summary::Summary;
use super::{
    Check, Clock, Env, NoCheck, NoSmoke, Options, Smoke, SystemClock, deploy, lock, summary, why,
};
use crate::CommitId;

/// Where the host says which operator API to call and where its token is, so a caller
/// that may only run `deploy` names neither.
pub const CONFIG: &str = "/etc/kbf-installer/deploy.json";

pub const USAGE: &str = "usage: kbf-installer deploy --commit <40hex> [--api <127.0.0.1:port>] \
[--api-ca <pem>] [--api-server-name <dns>] [--token-file <path>] [--config <path>] \
[--cas http(s)://127.0.0.1:port] [--cas-ca <pem>] [--cas-server-name <dns>] [--apply] [--json] [--bundle -|DIR] \
[--allow-offline <id>[,<id>...]] [--node-timeout-s N] [--handover-timeout-s N] [--smoke-timeout-s N] \
[--lock <path>] [--platform linux-x86_64|linux-arm64|darwin-arm64]";

/// How long one call to the API may take, a release upload included (each read and write
/// of the connection is held to it).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Where the CAS smoke dials and how it verifies a TLS listener; from the flags, else the
/// host's config. With no address the frontend's own config names it ([`smoke_for`]).
#[derive(Default)]
pub(super) struct CasFlags {
    pub(super) url: Option<String>,
    pub(super) ca: Option<PathBuf>,
    pub(super) server_name: Option<String>,
}

/// Where the API is and how its certificate is verified, from the flags and the host config.
struct Api {
    addr: SocketAddr,
    ca: Option<PathBuf>,
    server_name: Option<String>,
}

pub(super) struct Args {
    pub(super) options: Options,
    api: Api,
    cas: CasFlags,
    token_file: PathBuf,
    bundle: Option<String>,
    lock: PathBuf,
}

/// Flags that point the deploy at a different API, token, config, lock or platform. A caller
/// that got here through `sudo` (a deploy account that may only run `deploy`) must not set
/// them: root would read a file of the caller's choosing and send it to an address of the
/// caller's choosing. Those values come from the root-owned config.
const HOST_FLAGS: [&str; 10] = [
    "--api",
    "--api-ca",
    "--api-server-name",
    "--token-file",
    "--config",
    "--lock",
    "--platform",
    "--cas",
    "--cas-ca",
    "--cas-server-name",
];

pub(super) fn parse(args: &[String], via_sudo: bool, owner: u32) -> Result<Args, String> {
    let mut commit = None;
    let mut api = None;
    let mut api_ca = None;
    let mut api_server_name = None;
    let mut cas = CasFlags::default();
    let mut token_file = None;
    let mut bundle = None;
    let mut apply = false;
    let mut allow = Vec::new();
    let mut lock_path = PathBuf::from(super::LOCK);
    let mut platform = host_platform().to_owned();
    let mut config = PathBuf::from(CONFIG);
    // The nodes' budget each, the whole frontend update (it includes the bake), a smoke.
    let mut secs = [1800u64, 3600, 60];
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
            "--api-ca" => api_ca = Some(PathBuf::from(value("--api-ca")?)),
            "--api-server-name" => api_server_name = Some(value("--api-server-name")?),
            "--cas" => cas.url = Some(value("--cas")?),
            "--cas-ca" => cas.ca = Some(PathBuf::from(value("--cas-ca")?)),
            "--cas-server-name" => cas.server_name = Some(value("--cas-server-name")?),
            "--token-file" => token_file = Some(PathBuf::from(value("--token-file")?)),
            "--bundle" => bundle = Some(value("--bundle")?),
            "--config" => config = PathBuf::from(value("--config")?),
            "--lock" => lock_path = PathBuf::from(value("--lock")?),
            "--platform" => platform = value("--platform")?,
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
    // The host's config fills what the flags leave out. It is not read at all when the flags
    // give everything it could.
    let missing = [
        api.is_none(),
        api_ca.is_none(),
        api_server_name.is_none(),
        token_file.is_none(),
        cas.url.is_none(),
        cas.ca.is_none(),
        cas.server_name.is_none(),
    ];
    let conf = read_config(&config, owner, missing.contains(&true))?;
    let api = api.or(conf.api);
    let api_ca = api_ca.or(conf.api_ca);
    let api_server_name = api_server_name.or(conf.api_server_name);
    let token_file = token_file.or(conf.token_file);
    cas.url = cas.url.or(conf.cas);
    cas.ca = cas.ca.or(conf.cas_ca);
    cas.server_name = cas.server_name.or(conf.cas_server_name);
    let commit = commit.ok_or("--commit is required")?;
    let commit = CommitId::new(&commit).map_err(|e| e.to_string())?;
    let api = api.ok_or("--api is required (or `api` in the config)")?;
    let addr: SocketAddr = api
        .parse()
        .map_err(|_| format!("--api {api:?} is not host:port"))?;
    if !matches!(
        platform.as_str(),
        "linux-x86_64" | "linux-arm64" | "darwin-arm64"
    ) {
        return Err(format!("--platform {platform:?} is not a platform"));
    }
    if let Some(url) = &cas.url {
        CasSmoke::new(url, Duration::from_secs(secs[2])).map_err(|e| format!("--cas {e}"))?;
    }
    // One id for the whole run: every retry of the frontend update carries it.
    let request_id = format!("deploy-{}", hex::encode(random(16)?));
    let mut options = Options::new(commit, &platform, request_id);
    options.apply = apply;
    options.allow_offline = allow;
    options.node_timeout = Duration::from_secs(secs[0]);
    options.handover_timeout = Duration::from_secs(secs[1]);
    options.smoke_timeout = Duration::from_secs(secs[2]);
    Ok(Args {
        options,
        api: Api {
            addr,
            ca: api_ca,
            server_name: api_server_name,
        },
        cas,
        token_file: token_file.ok_or("--token-file is required (or `token_file` in the config)")?,
        bundle,
        lock: lock_path,
    })
}

/// What the host's config file says.
#[derive(Default)]
struct HostConfig {
    /// The proxy's public port, which serves every call of the deploy.
    api: Option<String>,
    /// CA certificates (PEM) the API's certificate is verified against, besides the system's.
    api_ca: Option<PathBuf>,
    /// The name the API's certificate carries.
    api_server_name: Option<String>,
    token_file: Option<PathBuf>,
    /// The local REAPI listener the CAS smoke uses.
    cas: Option<String>,
    cas_ca: Option<PathBuf>,
    cas_server_name: Option<String>,
}

/// `{"api": "127.0.0.1:port", "token_file": "/path", "cas": "http://127.0.0.1:port"}` (and
/// `api_ca`, `api_server_name`, `cas_ca`, `cas_server_name`). Read only when a flag is
/// missing; it must be a regular file owned by root and writable by no one else.
fn read_config(path: &Path, owner: u32, needed: bool) -> Result<HostConfig, String> {
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
    if meta.uid() != owner {
        return Err(format!("config {} must be owned by root", path.display()));
    }
    let text = fs::read_to_string(path).map_err(why("config"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(why("config"))?;
    let get = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    Ok(HostConfig {
        api: get("api"),
        api_ca: get("api_ca").map(PathBuf::from),
        api_server_name: get("api_server_name"),
        token_file: get("token_file").map(PathBuf::from),
        cas: get("cas"),
        cas_ca: get("cas_ca").map(PathBuf::from),
        cas_server_name: get("cas_server_name"),
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
    platform_of(std::env::consts::OS, std::env::consts::ARCH)
}

/// [`host_platform`] for the named OS and CPU (the spellings of `std::env::consts`).
pub(super) fn platform_of(os: &str, arch: &str) -> &'static str {
    match (os, arch) {
        ("macos", _) => "darwin-arm64",
        (_, "aarch64") => "linux-arm64",
        _ => "linux-x86_64",
    }
}

/// The frontend's own config: where its listener's address and certificate are.
pub(super) struct FrontendConfig {
    pub toml: PathBuf,
    pub units: Vec<PathBuf>,
}

impl FrontendConfig {
    pub(super) fn host() -> FrontendConfig {
        let unit = |name: &str| PathBuf::from("/etc/systemd/system").join(name);
        FrontendConfig {
            toml: PathBuf::from("/etc/kbf/kbf.toml"),
            units: [
                "kbf-frontend@.service",
                "kbf-all-in-one.service",
                "kbf-server.service",
            ]
            .map(unit)
            .to_vec(),
        }
    }
}

/// The smoke a deploy ends with: against the REAPI listener the flags or the host config
/// name, else the one the frontend's own config says, else none (which a deploy reports
/// as skipped, exit 5).
pub(super) fn smoke_for(
    cas: &CasFlags,
    timeout: Duration,
    frontend: &FrontendConfig,
) -> Result<Box<dyn Smoke>, String> {
    let (url, derived_name) = match &cas.url {
        Some(url) => (url.clone(), None),
        None => match derive(&frontend.toml, &frontend.units)? {
            Some(d) => (d.url, d.server_name),
            None => return Ok(Box::new(NoSmoke)),
        },
    };
    let ca_pem = cas
        .ca
        .as_ref()
        .map(|p| fs::read(p).map_err(|e| format!("--cas-ca {}: {e}", p.display())))
        .transpose()?;
    let name = cas.server_name.clone().or(derived_name);
    let smoke = CasSmoke::new(&url, timeout)
        .and_then(|s| s.tls(name, ca_pem))
        .map_err(|e| format!("--cas {e}"))?;
    Ok(Box::new(smoke))
}

/// The client of the API at `api`, with the CA file read.
fn api_client(api: &Api, token: String) -> Result<Https, String> {
    let ca_pem = api
        .ca
        .as_ref()
        .map(|p| fs::read(p).map_err(|e| format!("--api-ca {}: {e}", p.display())))
        .transpose()?;
    let trust = Trust {
        server_name: api.server_name.clone(),
        ca_pem,
    };
    Https::new(api.addr, &trust, token, CALL_TIMEOUT).map_err(|e| format!("--api: {e}"))
}

/// The token: a regular file owned by root, mode 0600 or 0400, one token of visible ASCII.
pub fn read_token(path: &Path) -> Result<String, String> {
    let meta = fs::symlink_metadata(path).map_err(why("token file"))?;
    if !meta.file_type().is_file() {
        return Err("the token file is not a regular file".to_owned());
    }
    if !matches!(meta.mode() & 0o7777, 0o600 | 0o400) {
        return Err("the token file must be mode 0600 or 0400".to_owned());
    }
    let text = fs::read_to_string(path).map_err(why("token file"))?;
    let token = text.trim();
    if token.len() < 32 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("the token file does not hold a token".to_owned());
    }
    Ok(token.to_owned())
}

fn is_root() -> bool {
    current_uid() == 0
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
    run(args, stdin, out, err, via_sudo, &Host::current())
}

/// What the deploy needs to know about the machine it runs on.
pub(super) struct Host {
    /// Running as root.
    pub root: bool,
    /// The uid that must own the host's config. Root's, except in the unit tests, whose
    /// files belong to whoever runs them.
    pub config_owner: u32,
    pub frontend: FrontendConfig,
}

impl Host {
    pub(super) fn current() -> Host {
        Host {
            root: is_root(),
            config_owner: if cfg!(test) { current_uid() } else { 0 },
            frontend: FrontendConfig::host(),
        }
    }
}

pub(super) fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// [`main_as`], told too about the machine.
pub(super) fn run(
    args: &[String],
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
    via_sudo: bool,
    host: &Host,
) -> i32 {
    let usage = |err: &mut dyn Write, why: &str| {
        let _ = writeln!(err, "kbf-installer deploy: {why}\n{USAGE}");
        Exit::Usage.code()
    };
    let parsed = match parse(args, via_sudo, host.config_owner) {
        Ok(p) => p,
        Err(why) => return usage(err, &why),
    };
    if parsed.options.apply && !host.root {
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
    let api = match api_client(&parsed.api, token) {
        Ok(a) => a,
        Err(why) => return usage(err, &why),
    };
    // A bundle directory is not read yet; only a stream on standard input is.
    let input: Option<&mut dyn Read> = match parsed.bundle.as_deref() {
        None => None,
        Some("-") => Some(stdin),
        Some(_) => return usage(err, "--bundle takes - (a tar on standard input)"),
    };
    // A plan runs no smoke, so it reads none of the frontend's config either.
    let smoke: Box<dyn Smoke> = if parsed.options.apply {
        match smoke_for(&parsed.cas, parsed.options.smoke_timeout, &host.frontend) {
            Ok(s) => s,
            Err(why) => return usage(err, &why),
        }
    } else {
        Box::new(NoSmoke)
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
