//! A frontend host behind the proxy: the `[proxy]` table `setup` writes into `installer.toml`
//! and the flags that fill it.
//!
//! `--mode=frontend --proxy` makes the host one that `kbf-installer proxy` serves: the proxy
//! owns the public port (the socket unit's), starts the frontend children itself on loopback
//! ports counted up from `--proxy-base-port`, and terminates TLS. So `setup` writes the proxy's
//! table, enables the proxy's two units instead of `kbf-frontend.service`, and leaves the
//! TLS files out of `kbf.toml` (the children speak plain h2c behind the proxy):
//!
//! | flag | key of `[proxy]` |
//! |---|---|
//! | `--reapi-tls-cert`, `--reapi-tls-key` | `server_cert_file`, `server_key_file` |
//! | `--worker-client-ca` | `client_ca_file` |
//! | `--proxy-base-port` | `[proxy.upstream] base_port` |
//! | `--buck2-auth`, `--buck2-token-file` | `buck2_auth`, `buck2_token_file` |
//!
//! The operator token is the one `setup` makes (`operator_token_file`). Everything else in the
//! table (the secret file, the state directory, the hold bound, the limits) is the proxy's
//! default, or what an operator wrote: a rerun keeps it. A table that is there is treated as
//! `kbf.toml` is: when the keys `setup` owns already say what the flags say it is kept as it
//! is; when they differ and the rest is the defaults it is rewritten; when they differ and the
//! table holds other settings `setup` refuses, naming the keys, since a rewrite would lose them.
//!
//! The table is read and written as `kbf_proxy::config::ProxyConfig`, so a key of it that
//! `setup` has no flag for is kept by a rerun (`take_owned` copies the owned keys only). A
//! `[[proxy.server_cert]]` list is the operator's: its default entry stands for the single
//! pair the flags give, so a rerun whose flags name that entry's files keeps the list as it
//! is, and flags that differ from it are refused, never written over the list. A
//! `kbf.toml` that still has the `[tls]` keys of a host before the proxy is refused
//! ([`leftover_tls_keys`]): the children would serve TLS behind a proxy that speaks h2c.
//!
//! No message carries a value, only the key or flag and the rule.

use clap::ValueEnum;
use kbf_proxy::auth::Buck2Auth;
use kbf_proxy::config::{MAX_BASE_PORT, SLOT_STRIDE};
// The one name of the proxy's table the rest of `setup` may use (the layering test lets only
// this file name `kbf_proxy`).
pub(super) use kbf_proxy::config::ProxyConfig;

use super::SetupError;
use super::kbf_settings::{KbfFlags, Setting, check_path};
use super::paths;
use super::validate::mode_name;
use crate::cli::Mode;
use crate::installer_toml::toml_string;
use crate::layout::Platform;

/// The first port of slot A when `--proxy-base-port` is not given.
pub const DEFAULT_BASE_PORT: u16 = 18_000;

/// The ports of a default host that the children's range must not cover: the proxy socket
/// unit's public port, the operator API's `kbf.toml` default, and the storage node's port that
/// the documentation and the packaged examples use (a storage node can share the host).
const RESERVED_PORTS: [u16; 3] = [8980, 8982, 8990];

/// The ports a slot uses above its base (REAPI, worker, operator, admin, enrollment).
const SLOT_SPAN: u16 = 4;

/// How the proxy authenticates Buck2 and REAPI callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AuthFlag {
    None,
    Bearer,
    Mtls,
}

impl AuthFlag {
    fn auth(self) -> Buck2Auth {
        match self {
            AuthFlag::None => Buck2Auth::None,
            AuthFlag::Bearer => Buck2Auth::Bearer,
            AuthFlag::Mtls => Buck2Auth::Mtls,
        }
    }
}

/// The flags as the command line gave them.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyFlags {
    /// Set this frontend host up behind `kbf-installer proxy`: write the `[proxy]` table of
    /// installer.toml and enable kbf-proxy.socket and kbf-proxy.service instead of
    /// kbf-frontend.service. Only for `--mode=frontend`. The proxy takes --reapi-tls-cert,
    /// --reapi-tls-key and --worker-client-ca; the listener flags are not taken.
    #[arg(long)]
    pub proxy: bool,
    /// The first loopback port of the frontend children the proxy starts (1024 to 65521);
    /// slot A uses it and the four after, slot B ten above. Default 18000. With --proxy.
    #[arg(long)]
    pub proxy_base_port: Option<u16>,
    /// How REAPI and Buck2 callers authenticate to the proxy; default none. With --proxy.
    #[arg(long, value_enum)]
    pub buck2_auth: Option<AuthFlag>,
    /// The Buck2 API key file, an absolute path, for --buck2-auth bearer.
    #[arg(long)]
    pub buck2_token_file: Option<String>,
}

/// The proxy host's settings, valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyPlan {
    pub base_port: u16,
    pub buck2_auth: Buck2Auth,
    pub buck2_token_file: Option<String>,
    pub server_cert: String,
    pub server_key: String,
    pub client_ca: String,
}

impl ProxyFlags {
    /// Check the flags for `mode`. `kbf` is the settings the `kbf.toml` flags gave: the ones
    /// the proxy owns are moved out of it into the plan. Each problem is pushed to `problems`.
    pub fn validate(
        &self,
        platform: Platform,
        mode: Mode,
        flags: &KbfFlags,
        kbf: &mut Vec<Setting>,
        problems: &mut Vec<String>,
    ) -> Option<ProxyPlan> {
        let before = problems.len();
        if !self.proxy {
            for (given, flag) in [
                (self.proxy_base_port.is_some(), "--proxy-base-port"),
                (self.buck2_auth.is_some(), "--buck2-auth"),
                (self.buck2_token_file.is_some(), "--buck2-token-file"),
            ] {
                if given {
                    problems.push(format!("{flag} is for --proxy"));
                }
            }
            return None;
        }
        if platform != Platform::Linux {
            problems.push("--proxy is for Linux: the proxy runs as systemd units".to_owned());
            return None;
        }
        if mode != Mode::Frontend {
            problems.push(format!(
                "--proxy is for --mode=frontend: the proxy starts `kbf --mode=frontend` children, which a {} host cannot sit behind",
                mode_name(mode)
            ));
            return None;
        }
        let (owned, rest): (Vec<Setting>, Vec<Setting>) = kbf
            .drain(..)
            .partition(|s| s.table == "tls" && TLS_KEYS.contains(&s.key));
        *kbf = rest;
        // A flag that is not given is named here; one that is given and bad was named by the
        // check of its value already.
        for flag in TLS_KEYS {
            if flags.get(flag).is_none() {
                problems.push(format!("--{flag}: required with --proxy"));
            }
        }
        let file = |key: &str| owned.iter().find(|s| s.key == key).map(|s| s.value.clone());
        let (cert, key, ca) = (
            file("reapi-tls-cert"),
            file("reapi-tls-key"),
            file("worker-client-ca"),
        );

        let base_port = self.proxy_base_port.unwrap_or(DEFAULT_BASE_PORT);
        if !(1024..=MAX_BASE_PORT).contains(&base_port) {
            problems.push(format!(
                "--proxy-base-port: must be 1024 to {MAX_BASE_PORT} so both slots fit"
            ));
        } else if let Some(port) = reserved_in_range(base_port) {
            problems.push(format!(
                "--proxy-base-port: the children's ports would cover {port}, which the public socket or the operator API uses by default"
            ));
        }
        let auth = self.buck2_auth.map_or(Buck2Auth::None, AuthFlag::auth);
        match (&self.buck2_token_file, auth) {
            (Some(file), Buck2Auth::Bearer) => {
                if let Err(rule) = check_path(file) {
                    problems.push(format!("--buck2-token-file: {rule}"));
                }
            }
            (Some(_), _) => {
                problems.push("--buck2-token-file is for --buck2-auth bearer".to_owned())
            }
            (None, _) => {}
        }
        let (Some(server_cert), Some(server_key), Some(client_ca)) = (cert, key, ca) else {
            return None;
        };
        if problems.len() > before {
            return None;
        }
        let plan = ProxyPlan {
            base_port,
            buck2_auth: auth,
            buck2_token_file: self.buck2_token_file.clone(),
            server_cert,
            server_key,
            client_ca,
        };
        // The proxy's own rules (bearer needs its file, the limits fit the budget), by the
        // function it starts with, so setup cannot write a table the proxy would refuse.
        if let Err(e) = plan.table() {
            problems.push(format!("--proxy: {e}"));
            return None;
        }
        Some(plan)
    }
}

/// The `kbf.toml` keys (and flags) whose files the proxy reads, not the children.
const TLS_KEYS: [&str; 3] = ["reapi-tls-cert", "reapi-tls-key", "worker-client-ca"];

/// The `[tls]` keys of `kbf.toml` that a `--proxy` host's children must not have: the proxy
/// reads the files (`TLS_KEYS`), and the worker pair belongs to a listener the children do not
/// have. A child that finds one of them serves TLS where the proxy speaks plain h2c to it.
const CHILD_TLS_KEYS: [&str; 5] = [
    "reapi-tls-cert",
    "reapi-tls-key",
    "worker-client-ca",
    "worker-tls-cert",
    "worker-tls-key",
];

/// The keys of `[tls]` in the `kbf.toml` text that `--proxy` makes wrong, in the order of
/// [`CHILD_TLS_KEYS`]; a text that does not parse has none here (the `kbf.toml` check refuses
/// it by itself).
///
/// `setup` refuses a host that has any rather than noting them: `kbf.toml` is the file the
/// operator's other settings are in, so `setup` cannot remove the keys, and a note is read
/// after the proxy's units are enabled, when the first start would fail on the TLS mismatch.
pub fn leftover_tls_keys(kbf_toml: &str) -> Vec<&'static str> {
    let Ok(table) = kbf_toml.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(tls) = table.get("tls").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    CHILD_TLS_KEYS
        .into_iter()
        .filter(|key| tls.contains_key(*key))
        .collect()
}

/// A reserved port inside either slot's range, if any.
fn reserved_in_range(base: u16) -> Option<u16> {
    RESERVED_PORTS.into_iter().find(|port| {
        [base, base + SLOT_STRIDE]
            .into_iter()
            .any(|start| (start..=start + SLOT_SPAN).contains(port))
    })
}

impl ProxyPlan {
    /// The settings that name files, for `check_files` (the `kbf.toml` flag each came from).
    pub fn files(&self) -> Vec<Setting> {
        let tls = |key, value: &String| Setting {
            table: "tls",
            key,
            value: value.clone(),
        };
        vec![
            tls("reapi-tls-cert", &self.server_cert),
            tls("reapi-tls-key", &self.server_key),
            tls("worker-client-ca", &self.client_ca),
        ]
    }

    /// The table these flags say, with the proxy's defaults for every key they do not set.
    /// It goes through `ProxyConfig::from_toml_str`, which makes the proxy's own checks.
    pub fn table(&self) -> Result<ProxyConfig, kbf_proxy::config::ConfigError> {
        let mut text = format!(
            "operator_token_file = {}\nserver_cert_file = {}\nserver_key_file = {}\nclient_ca_file = {}\nbuck2_auth = \"{}\"\n",
            toml_string(paths::api_token(Platform::Linux)),
            toml_string(&self.server_cert),
            toml_string(&self.server_key),
            toml_string(&self.client_ca),
            self.buck2_auth.name(),
        );
        if let Some(file) = &self.buck2_token_file {
            text.push_str(&format!("buck2_token_file = {}\n", toml_string(file)));
        }
        text.push_str(&format!("[upstream]\nbase_port = {}\n", self.base_port));
        ProxyConfig::from_toml_str(&text)
    }

    /// The table `installer.toml` holds after this run, given the one that is there.
    pub fn merge(&self, existing: Option<ProxyConfig>) -> Result<ProxyConfig, SetupError> {
        let wanted = self
            .table()
            .map_err(|e| SetupError::Invalid(vec![format!("--proxy: {e}")]))?;
        let Some(there) = existing else {
            return Ok(wanted);
        };
        let differ = differing(&there, &wanted);
        if differ.is_empty() {
            return Ok(there);
        }
        let mut moved = there;
        take_owned(&wanted, &mut moved);
        if moved == wanted {
            return Ok(wanted);
        }
        Err(SetupError::NeedsOperator(format!(
            "{} holds a [proxy] table with other settings and these keys differ from the flags ({}): edit them by hand",
            paths::installer_toml(Platform::Linux),
            differ.join("; ")
        )))
    }
}

/// The keys `setup` owns in which the two tables differ.
fn differing(a: &ProxyConfig, b: &ProxyConfig) -> Vec<&'static str> {
    [
        (
            a.operator_token_file != b.operator_token_file,
            "operator_token_file",
        ),
        (
            default_cert(a).map(|c| c.0) != default_cert(b).map(|c| c.0),
            "server_cert_file",
        ),
        (
            default_cert(a).map(|c| c.1) != default_cert(b).map(|c| c.1),
            "server_key_file",
        ),
        (a.client_ca_file != b.client_ca_file, "client_ca_file"),
        (a.buck2_auth != b.buck2_auth, "buck2_auth"),
        (a.buck2_token_file != b.buck2_token_file, "buck2_token_file"),
        (
            a.upstream.base_port != b.upstream.base_port,
            "upstream.base_port",
        ),
    ]
    .into_iter()
    .filter_map(|(differs, key)| differs.then_some(key))
    .collect()
}

/// The certificate and key `setup` owns: the single pair, or, in a table with a
/// `[[server_cert]]` list, the entry that is the default (the only one, or the one marked).
/// The flags `--reapi-tls-cert` and `--reapi-tls-key` are compared with this pair.
fn default_cert(c: &ProxyConfig) -> Option<(&std::path::Path, &std::path::Path)> {
    if let (Some(cert), Some(key)) = (&c.server_cert_file, &c.server_key_file) {
        return Some((cert, key));
    }
    let list = &c.server_cert;
    let only = list.first().filter(|_| list.len() == 1);
    list.iter()
        .find(|e| e.default)
        .or(only)
        .map(|e| (e.cert_file.as_path(), e.key_file.as_path()))
}

/// Copy the keys `setup` owns from `from` into `to`. The `[[server_cert]]` list is the
/// operator's and is not copied: a table that has one still differs from the wanted one
/// afterwards, so [`ProxyPlan::merge`] refuses it.
fn take_owned(from: &ProxyConfig, to: &mut ProxyConfig) {
    to.operator_token_file = from.operator_token_file.clone();
    to.server_cert_file = from.server_cert_file.clone();
    to.server_key_file = from.server_key_file.clone();
    to.client_ca_file = from.client_ca_file.clone();
    to.buck2_auth = from.buck2_auth;
    to.buck2_token_file = from.buck2_token_file.clone();
    to.upstream = from.upstream;
}

/// Catches the proxy's port layout and the deploy's copy of it drifting apart: `deploy` finds
/// the operator listener of each slot as `base + stride * slot + offset` without naming the
/// proxy, so a change to either side would send the gates to the wrong port. Here (one of the
/// files that may name `kbf_proxy`) is where both can be read.
///
/// Mutants planted: `SLOT_STRIDE` 10 to 11 in `kbf_proxy::config`; the operator offset 2 to 3
/// in `kbf_proxy::route`; each turned this test red.
#[cfg(test)]
mod layout_tests {
    use super::{MAX_BASE_PORT, SLOT_STRIDE};
    use crate::observe;
    use kbf_proxy::route::Target;

    #[test]
    fn the_deploys_port_layout_is_the_proxys() {
        assert_eq!(observe::SLOT_STRIDE, SLOT_STRIDE);
        assert_eq!(observe::OPERATOR_OFFSET, Target::Operator.offset());
        assert_eq!(observe::MAX_BASE_PORT, MAX_BASE_PORT);
    }
}
