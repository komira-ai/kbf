//! The host's deploy config, `/etc/kbf-installer/deploy.json`, written by `setup` on a
//! `frontend` or `all-in-one` host from named flags (`--deploy-stage` and friends), so
//! `kbf-installer deploy` needs none of them on its command line. The file is the one type
//! `deploy` reads ([`HostConfig`]): what this module writes is what that reader gets back.
//!
//! - Nothing is written unless `--deploy-stage` is given; every other `--deploy-*` flag needs it.
//!   The node modes take none of them and write nothing.
//! - The API address defaults to the proxy's public port (`[proxy] listen` of `installer.toml`,
//!   an unspecified address read as loopback; the packaged port when there is none), and the
//!   token to the `[proxy] operator_token_file` of the same file, so a host that has told
//!   `setup` where the proxy is gives only the stage.
//! - The token and the CA are referenced by path and never copied or read: they are the
//!   operator's. The one exception is the token `setup` makes on this run for a host that serves
//!   the API (the packaged `operator_token_file`): it may be absent, because it is there when
//!   `deploy` runs. Every other file must exist, be a regular file (a link is not followed), and have a
//!   safe owner and mode; the refusal says the one command that fixes it. A flag that holds
//!   PEM text or a token is refused as not a path.
//! - Setup owns five keys of the file (`api`, `api_ca`, `api_server_name`, `token_file`,
//!   `stage`) and keeps every other key it holds; the file is root's, mode 0644, written
//!   atomically by the same step every file of `setup` is.
//! - The sudoers rule that lets the deploy account run `deploy` is the CEO's and is not written
//!   from code: `docs/design/gamma-deploy.md`, "The sudoers rule".

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use kbf_host::{Entry, Exec, HostPath, Kind, Perms, Planner};

use super::kbf_settings::check_path;
use super::os::View;
use super::paths;
use super::plan::{Ctx, Outcome};
use super::proxy::ProxyConfig;
use super::{SetupError, validate::mode_name};
use crate::cli::Mode;
use crate::deploy::host_config::{CONFIG, HostConfig};
use crate::deploy::offer::Stage;

/// The proxy's port when `installer.toml` names none: the packaged `kbf-proxy.socket`.
const PACKAGED_API: &str = "127.0.0.1:8980";
/// The directory of the file, root's.
const CONFIG_DIR: &str = "/etc/kbf-installer";

/// The flags as the command line gave them.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct DeployFlags {
    /// The stage this host is, gamma or prod: setup writes /etc/kbf-installer/deploy.json so
    /// `kbf-installer deploy` binds an offer to it. frontend, all-in-one.
    #[arg(long)]
    pub deploy_stage: Option<String>,
    /// The operator API deploy calls, as a loopback IP:PORT. Default: the proxy's public port
    /// (installer.toml `[proxy] listen`, else 127.0.0.1:8980).
    #[arg(long)]
    pub deploy_api: Option<String>,
    /// A PEM file of the CA the API's certificate is verified against (absolute path; it must
    /// exist). Default: the system's roots only.
    #[arg(long)]
    pub deploy_api_ca: Option<String>,
    /// The DNS name the API's certificate carries. Default: the API address's IP.
    #[arg(long)]
    pub deploy_server_name: Option<String>,
    /// The operator token's file (absolute path, mode 0600 or 0400, owned by root; it must
    /// exist, except the token setup makes itself, /etc/kbf/api-token on Linux). Default: installer.toml `[proxy]
    /// operator_token_file`.
    #[arg(long)]
    pub deploy_token_file: Option<String>,
}

/// The flags, valid, with what needs the host still open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploySettings {
    pub stage: Stage,
    pub api: Option<SocketAddr>,
    pub api_ca: Option<String>,
    pub server_name: Option<String>,
    pub token_file: Option<String>,
}

impl DeployFlags {
    fn given(&self) -> [(&'static str, bool); 5] {
        [
            ("--deploy-stage", self.deploy_stage.is_some()),
            ("--deploy-api", self.deploy_api.is_some()),
            ("--deploy-api-ca", self.deploy_api_ca.is_some()),
            ("--deploy-server-name", self.deploy_server_name.is_some()),
            ("--deploy-token-file", self.deploy_token_file.is_some()),
        ]
    }

    /// Check the flags for `mode` with no host: the settings, or a line per problem pushed to
    /// `problems`. No line carries a value.
    pub fn validate(&self, mode: Mode, problems: &mut Vec<String>) -> Option<DeploySettings> {
        let given: Vec<&str> = self
            .given()
            .into_iter()
            .filter_map(|(flag, set)| set.then_some(flag))
            .collect();
        if given.is_empty() {
            return None;
        }
        if !matches!(mode, Mode::Frontend | Mode::AllInOne) {
            for flag in given {
                problems.push(format!(
                    "{flag} is for the modes that serve the API (frontend, all-in-one); --mode={} does not take it",
                    mode_name(mode)
                ));
            }
            return None;
        }
        let before = problems.len();
        let stage = match &self.deploy_stage {
            Some(s) => Stage::parse(s)
                .map_err(|_| problems.push("--deploy-stage: must be gamma or prod".to_owned()))
                .ok(),
            None => {
                problems
                    .push("--deploy-stage: required with the other --deploy-* flags".to_owned());
                None
            }
        };
        let api = self.deploy_api.as_deref().and_then(|a| {
            api_addr(a)
                .map_err(|rule| problems.push(format!("--deploy-api: {rule}")))
                .ok()
        });
        for (flag, value) in [
            ("--deploy-api-ca", &self.deploy_api_ca),
            ("--deploy-token-file", &self.deploy_token_file),
        ] {
            if let Some(Err(rule)) = value.as_deref().map(check_path) {
                problems.push(format!("{flag}: {rule}"));
            }
        }
        if let Some(name) = &self.deploy_server_name
            && !dns_name(name)
        {
            problems.push(
                "--deploy-server-name: must be a DNS name of letters, digits, - and . (no IP, no port)"
                    .to_owned(),
            );
        }
        match stage {
            Some(stage) if problems.len() == before => Some(DeploySettings {
                stage,
                api,
                api_ca: self.deploy_api_ca.clone(),
                server_name: self.deploy_server_name.clone(),
                token_file: self.deploy_token_file.clone(),
            }),
            _ => None,
        }
    }
}

/// A loopback `IP:PORT`: a deploy drives the proxy of its own host.
fn api_addr(text: &str) -> Result<SocketAddr, &'static str> {
    let addr: SocketAddr = text
        .parse()
        .map_err(|_| "must be a loopback IP:PORT, as 127.0.0.1:8980")?;
    if addr.port() == 0 || !addr.ip().to_canonical().is_loopback() {
        return Err(
            "must be a loopback IP:PORT with a port from 1 to 65535: a deploy drives the proxy of its own host",
        );
    }
    Ok(addr)
}

fn dns_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && name.parse::<IpAddr>().is_err()
}

/// The address `deploy` should dial for a proxy listening at `listen`: the same port on
/// loopback when the listener is unspecified or loopback, and none for any other address.
fn dial(listen: SocketAddr) -> Option<SocketAddr> {
    let ip = listen.ip().to_canonical();
    let host = match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip if ip.is_loopback() => ip,
        _ => return None,
    };
    Some(SocketAddr::new(host, listen.port()))
}

/// What the file will say, once the files it names are checked on the host. `None` when no
/// `--deploy-*` flag was given. Reads the host, changes nothing.
pub fn resolve(
    exec: &dyn Exec,
    want: Option<&DeploySettings>,
    proxy: Option<&ProxyConfig>,
    ctx: &Ctx,
) -> Result<Option<HostConfig>, SetupError> {
    let Some(want) = want else {
        return Ok(None);
    };
    let mut problems = Vec::new();
    let api = match (want.api, proxy.and_then(|p| p.listen)) {
        (Some(addr), _) => Some(addr),
        (None, None) => Some(PACKAGED_API.parse().expect("a socket address")),
        (None, Some(listen)) => {
            let addr = dial(listen);
            if addr.is_none() {
                problems.push(
                    "--deploy-api: the proxy listens on an address that is not loopback, and a deploy drives its own host: give a loopback --deploy-api"
                        .to_owned(),
                );
            }
            addr
        }
    };
    let token_file = want
        .token_file
        .clone()
        .or_else(|| proxy.map(|p| p.operator_token_file.to_string_lossy().into_owned()));
    if token_file.is_none() {
        problems.push(
            "--deploy-token-file: required (installer.toml has no [proxy] operator_token_file to default to)"
                .to_owned(),
        );
    }
    let (Some(api), Some(token_file)) = (api, token_file) else {
        return Err(SetupError::Invalid(problems));
    };
    let service = ctx.service.as_ref().map(|u| u.uid);
    let mut refused = Vec::new();
    refused.extend(check_file(
        exec,
        "--deploy-token-file",
        &token_file,
        true,
        ctx,
        service,
    ));
    if let Some(ca) = &want.api_ca {
        refused.extend(check_file(exec, "--deploy-api-ca", ca, false, ctx, service));
    }
    if !refused.is_empty() {
        return Err(SetupError::NeedsOperator(refused.join("\n")));
    }
    Ok(Some(HostConfig {
        api: Some(api.to_string()),
        api_ca: want.api_ca.clone().map(Into::into),
        api_server_name: want.server_name.clone(),
        token_file: Some(token_file.into()),
        stage: Some(want.stage.word().to_owned()),
        // The gates read the frontend proxy's children, which the `[proxy.upstream]` table
        // places; with no table here the operator gives `--observe-base-port` to `deploy`.
        observe_base_port: proxy.map(|p| p.upstream.base_port.to_string()),
        ..HostConfig::default()
    }))
}

/// Why the file `path` (named by `flag`) cannot be what `deploy` needs, as one line that ends
/// with the command that fixes it; `None` when it can. The file is looked at (a link is not
/// followed) and never read. A token is mode 0600 or 0400 (what `deploy` demands); a CA is
/// not writable by its group or others. Either belongs to root or to the service user.
fn check_file(
    exec: &dyn Exec,
    flag: &str,
    path: &str,
    token: bool,
    ctx: &Ctx,
    service: Option<u32>,
) -> Option<String> {
    let what = if token {
        "the operator token"
    } else {
        "the CA"
    };
    let meta = match HostPath::new(path).map(|p| exec.stat(&p)) {
        Ok(Ok(Some(meta))) => meta,
        // The token setup itself makes on this run (plan::operator_token, for a host that serves
        // the API): absent now, it is there when `deploy` runs, so a first run may name it.
        Ok(Ok(None)) if token && path == paths::api_token(ctx.platform) => return None,
        Ok(Ok(None)) => {
            return Some(format!(
                "{flag}: {path} does not exist; {what} is the operator's and setup never copies it: place it, then run setup again (install -o root -g root -m {} SOURCE {path})",
                if token { "0600" } else { "0644" }
            ));
        }
        _ => {
            return Some(format!(
                "{flag}: {path} cannot be looked at: check the path and its directory, then run setup again"
            ));
        }
    };
    if meta.kind != Kind::File {
        return Some(format!(
            "{flag}: {path} is not a regular file (a symbolic link is not followed): replace it with the file itself, then run setup again"
        ));
    }
    if meta.owner.uid != ctx.root.uid && Some(meta.owner.uid) != service {
        return Some(format!(
            "{flag}: {path} is owned by neither root nor the service user: chown root:root {path}"
        ));
    }
    let bits = meta.perms.bits() & 0o7777;
    if token && !matches!(bits, 0o600 | 0o400) {
        return Some(format!(
            "{flag}: {path} must be mode 0600 or 0400, as deploy requires: chmod 600 {path}"
        ));
    }
    if !token && bits & 0o022 != 0 {
        return Some(format!(
            "{flag}: {path} must not be writable by its group or others: chmod go-w {path}"
        ));
    }
    None
}

/// The plan for the file: the directory, then the file with `want`'s five keys and whatever
/// else the file holds now. A file that is not a JSON object of the config's shape is the
/// operator's to fix. Reads the host, changes nothing.
pub fn plan(exec: &dyn Exec, ctx: &Ctx, want: &HostConfig) -> Result<Outcome, SetupError> {
    let path = HostPath::new(CONFIG)?;
    let current = match exec.inspect(&path)? {
        None => None,
        Some(Entry::File { bytes, .. }) => {
            let text = String::from_utf8_lossy(&bytes);
            Some(HostConfig::parse(&text).map_err(|why| {
                SetupError::NeedsOperator(format!(
                    "{CONFIG} {why}: fix it or remove it, then run setup again"
                ))
            })?)
        }
        Some(_) => {
            return Err(SetupError::NeedsOperator(format!(
                "{CONFIG} is not a regular file: replace it or remove it, then run setup again"
            )));
        }
    };
    let existed = current.is_some();
    let config = HostConfig {
        api: want.api.clone(),
        api_ca: want.api_ca.clone(),
        api_server_name: want.api_server_name.clone(),
        token_file: want.token_file.clone(),
        stage: want.stage.clone(),
        // Setup's own derivation wins; an operator's value stays when it derived none.
        observe_base_port: want
            .observe_base_port
            .clone()
            .or_else(|| current.as_ref().and_then(|c| c.observe_base_port.clone())),
        ..current.unwrap_or_default()
    };
    let view = View(exec);
    let mut p = Planner::new(&view);
    let dir = HostPath::new(CONFIG_DIR)?;
    p.dir(dir, Perms::new(0o755), Some(ctx.root))?;
    let (perms, bytes) = (Perms::new(0o644), config.render().into_bytes());
    let changed = p.file(path, perms, Some(ctx.root), bytes)?;
    let mut notes = Vec::new();
    if changed {
        notes.push(format!(
            "{CONFIG} {}: it names the token and the CA by path only; the sudoers rule that lets the deploy account run `deploy` is the CEO's to install (docs/design/gamma-deploy.md, \"The sudoers rule\")",
            if existed { "changed" } else { "written" }
        ));
    }
    Ok(Outcome {
        plan: p.finish(),
        notes,
    })
}
