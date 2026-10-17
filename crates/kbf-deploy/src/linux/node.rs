//! Rule `node` of [`super::modes`]: the unit of `kbf-deploy linux render-units --mode
//! node`, the one unit the package ships for every non-frontend role
//! (docs/design/install-and-enrollment.md, sections 3.2 and 4.4). The roles come from
//! the frontend at enrollment, not from the unit, so the unit holds nothing a role would
//! change and nothing secret. The rule reads the unit and its drop-ins as systemd applies
//! them (an empty `ExecStartPre=` resets the list) and refuses:
//!
//! - a `User=` that is not `KBF_USER`: the node is unprivileged, and root or another
//!   account would own keys the package gives to `KBF_USER`;
//! - an `ExecStartPre=` list that is not exactly one line,
//!   `<KBF_BIN_DIR>/kbf enroll --config=<KBF_ETC>/kbf.toml`, as written: `enroll` exits 0
//!   at once for a node that holds good certificates, and a unit without it, or with a
//!   `-` in front (systemd then ignores its failure), starts a node that never enrolled;
//! - an `ExecStart=` whose `--config=` is not `<KBF_ETC>/kbf.toml`: the node's one config
//!   file is the package's `/etc/kbf/kbf.toml`;
//! - any flag of either command whose name holds `key` (`--key=`, `--tls-key=`,
//!   `--object-store-keys-file=`): the node makes its own key pair in its state
//!   directory and is sent the cold store's keys over mTLS, so a unit that names a key
//!   file gives the node a credential it must not have. Rule `credential` refuses the
//!   other ways in (`s3.env`, `AWS_*`, systemd credentials).
//!
//! What it cannot see: a key in a file `kbf.toml` names (the config holds no secret by
//! design; the package's `kbf check-args` and `kbf enroll` read it), and a flag spelled
//! without `key` in its name.

use super::config::LinuxConfig;
use crate::unit::{UnitFile, WHITESPACE};

/// The node's config file under `KBF_ETC`.
pub const CONFIG_FILE: &str = "kbf.toml";

/// The `ExecStartPre=` lines the unit runs, as systemd applies them: every assignment of
/// `[Service]` in order, an empty one resetting the list.
fn pre_commands(unit: &UnitFile) -> Vec<String> {
    let mut lines = Vec::new();
    for d in unit
        .directives()
        .iter()
        .filter(|d| d.section == "Service" && d.key == "ExecStartPre")
    {
        let v = d.value.trim_matches(WHITESPACE);
        if v.is_empty() {
            lines.clear();
        } else {
            lines.push(v.to_owned());
        }
    }
    lines
}

/// The command `ExecStartPre=` must be.
#[must_use]
pub fn enroll_command(config: &LinuxConfig) -> String {
    format!(
        "{}/kbf enroll --config={}",
        config.bin_dir,
        config_path(config)
    )
}

fn config_path(config: &LinuxConfig) -> String {
    format!("{}/{CONFIG_FILE}", config.etc)
}

/// Rule `node` on `unit`: one line per way it breaks the node's contract.
pub(super) fn check(config: &LinuxConfig, unit: &UnitFile) -> Vec<String> {
    let mut out = Vec::new();
    let user = unit
        .get_in("Service", "User")
        .map(|u| u.trim_matches(WHITESPACE));
    if user != Some(config.user.as_str()) {
        out.push(format!(
            "runs as {}, not {} (KBF_USER): the node is unprivileged and its keys are \
             {}'s alone",
            user.filter(|u| !u.is_empty()).unwrap_or("root (no User=)"),
            config.user,
            config.user
        ));
    }
    let want = enroll_command(config);
    let pre = pre_commands(unit);
    if pre != [want.clone()] {
        let given = if pre.is_empty() {
            "no ExecStartPre=".to_owned()
        } else {
            format!("ExecStartPre={}", pre.join("; "))
        };
        out.push(format!(
            "has {given}, want exactly 'ExecStartPre={want}': enroll is idempotent, and a \
             unit without it (or with a '-' before it) starts a node that never enrolled"
        ));
    }
    let file = config_path(config);
    if unit.flag("config") != Some(file.as_str()) {
        out.push(format!(
            "ExecStart= does not give --config={file}: the node's one config file"
        ));
    }
    let mut commands = unit.exec_start();
    for line in &pre {
        commands.extend(line.split_whitespace());
    }
    for word in commands {
        let Some(flag) = word.strip_prefix("--") else {
            continue;
        };
        let name = flag.split_once('=').map_or(flag, |(name, _)| name);
        if name.contains("key") {
            out.push(format!(
                "gives the flag --{name}: a node makes its own key pair and is sent the \
                 cold store's keys over mTLS, so no unit names a key file"
            ));
        }
    }
    out
}

#[cfg(test)]
#[path = "node_tests.rs"]
mod tests;
