//! `/etc/kbf/kbf.toml`: the settings of a mode, in a file instead of on the command line.
//!
//! The file is the mode's flags and nothing else. Its keys are the flags' long names
//! (`--storage-listen` is `storage-listen`), grouped in one TOML table per setting group
//! ([`group_of`]), and a value is what the flag takes: a string or a number for a flag
//! with a value, `true` for a switch, an array for a flag that may be given more than
//! once. The file is turned into the flags it stands for ([`file_args`]) and clap parses
//! those with the command line, so a setting has the same name, the same validation and
//! the same default in the file as on the command line, and nothing here knows a mode's
//! flags: the mode's own command says which keys exist.
//!
//! Rules:
//!
//! - a flag given on the command line overrides the file's key for it;
//! - a key that is not a flag of the mode, or sits in another table than the flag's
//!   group, is refused naming the key and the file;
//! - the file must be a regular file that neither group nor other can write, since it
//!   decides what the process serves and where it keeps its keys;
//! - a secret is never a value in the file: the flags that take a secret take the path of
//!   a file that holds it, and so does the key.
//!
//! The path is `--config FILE`, else [`DEFAULT_PATH`] when that file exists.
//! [`template`] writes the whole file for a mode from its flag definitions, which
//! `tests/golden/` pins.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use clap::{Arg, ArgAction, Command};

/// The file `kbf <mode>` reads when `--config` does not name one and this exists.
pub const DEFAULT_PATH: &str = "/etc/kbf/kbf.toml";

/// The group bits and other bits that may not be set on the file: write.
const WRITABLE_BY_OTHERS: u32 = 0o022;

/// The table of the file a flag's key is in, by the flag's long name; `None` for a flag
/// that is in no group (which `config::tests` refuses for every flag of every mode, so a
/// flag cannot be added without its key).
#[must_use]
pub fn group_of(flag: &str) -> Option<&'static str> {
    GROUPS
        .iter()
        .find(|(_, flags)| flags.contains(&flag))
        .map(|(group, _)| *group)
}

/// The flags of each group. A flag is in exactly one.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "listener",
        &[
            "advertise-addr",
            "api-listen",
            "http2-keepalive-interval-secs",
            "http2-keepalive-timeout-secs",
            "listen",
            "reapi-max-streams",
            "releases-dir",
            "shutdown-timeout-secs",
            "storage-listen",
            "tls-reload-interval-secs",
            "worker-listen",
            "worker-max-streams",
        ],
    ),
    (
        "tls",
        &[
            "api-token-file",
            "reapi-auth-policy",
            "reapi-tls-cert",
            "reapi-tls-key",
            "storage-client-ca",
            "storage-tls-cert",
            "storage-tls-key",
            "worker-client-ca",
            "worker-deny-list",
            "worker-tls-cert",
            "worker-tls-key",
        ],
    ),
    (
        "scheduler",
        &[
            "finished-retention-secs",
            "heartbeat-interval-ms",
            "input-tree-cache-bytes",
            "input-tree-cache-entries",
            "input-tree-reads-in-flight",
            "progress-interval-secs",
            "readyz-store-timeout-ms",
            "state-store",
            "unservable-wait-secs",
        ],
    ),
    (
        "history",
        &[
            "history-index-root",
            "history-objects",
            "invocation-memory",
            "invocations-open",
        ],
    ),
    (
        "handover",
        &[
            "envoy-check-interval-ms",
            "envoy-check-timeout-ms",
            "envoy-unhealthy-threshold",
            "handover-follow-interval-ms",
            "handover-peer-name",
            "handover-quiesce-secs",
            "handover-socket",
            "handover-watchdog-secs",
            "instance",
        ],
    ),
    (
        "storage-client",
        &[
            "storage",
            "storage-ca",
            "storage-client-cert",
            "storage-client-key",
            "storage-dial-timeout-ms",
            "storage-ping-interval-ms",
            "storage-ping-timeout-ms",
        ],
    ),
    (
        "stores",
        &[
            "cold-store",
            "cold-store-evict-grace-ms",
            "cold-store-floor-bytes",
            "cold-store-size-bytes",
            "hot-store",
            "hot-store-abandon",
            "hot-store-max-bytes",
            "meta-dir",
            "store-put-attempts",
            "write-bytes-in-flight",
            "write-linger",
            "write-own-segment-bytes",
            "write-puts-in-flight",
        ],
    ),
    (
        "storage-node",
        &[
            "member-dwell-secs",
            "member-ttl-secs",
            "storage-weight",
            "storage-zone",
        ],
    ),
    ("log", &["log-dir", "log-files", "log-max-bytes"]),
    (
        "identity",
        &[
            "ca-cert",
            "cas",
            "cert",
            "frontend",
            "key",
            "label",
            "node-id",
            "server",
            "tls-server-name",
        ],
    ),
    (
        "daemon",
        &[
            "async-threads",
            "blocking-threads",
            "cpu-threads",
            "daemon-user",
            "data-dir",
            "data-max-bytes",
            "driver",
            "image",
            "reconnect-max-ms",
            "reconnect-ms",
            "reserve-cpus",
            "reserve-share",
            "role",
            "update-dir",
        ],
    ),
    (
        "containers",
        &[
            "actions-memory-max-gib",
            "cgroup-parent",
            "cgroup-root",
            "container-nofile",
            "container-nproc",
            "container-pids-limit",
            "container-shm-mib",
            "id-files",
            "lease-placement",
            "lease-swap-kill-mib",
            "lease-swap-kill-percent",
            "lease-swap-poll-ms",
            "numa-dir",
            "oci-runtime",
            "psi-memory-file",
            "self-cgroup",
            "supervisor-memory-min-mib",
        ],
    ),
    (
        "memory",
        &[
            "memory-headroom-mib",
            "memory-ladder-gib",
            "memory-limit-percent",
            "memory-poll-ms",
            "memory-pressure-pause-percent",
        ],
    ),
    (
        "macos",
        &[
            "mac-session-socket",
            "pool-first-uid",
            "pool-users",
            "require-metal-toolchain",
            "xcode-apps",
            "xcode-recheck-secs",
        ],
    ),
    (
        "outputs",
        &[
            "output-max-bytes",
            "output-max-depth",
            "output-max-entries",
            "output-max-stdio-bytes",
            "output-upload-deadline-secs",
            "segment-upload-idle-secs",
            "segment-upload-max-bytes",
            "segment-upload-streams",
            "segment-uploads-in-flight",
        ],
    ),
    (
        "transfer",
        &[
            "transfer-bytes-in-flight",
            "transfer-chunk-bytes",
            "transfer-find-missing-chunk",
            "transfer-in-flight",
            "transfer-memory-blob-bytes",
            "transfer-retries",
            "transfer-stream-window-bytes",
        ],
    ),
];

/// The arguments of `own` that are flags with a long name a file may set: all but help,
/// version, the two flags of this module and the `refused` ones (the flags of
/// `kbf-server` that a mode hides and refuses by name have no key).
fn settings<'a>(own: &'a Command, refused: &BTreeSet<String>) -> BTreeMap<String, &'a Arg> {
    own.get_arguments()
        .filter_map(|a| a.get_long().map(|long| (long.to_owned(), a)))
        .filter(|(long, _)| !["help", "version", "config", "check"].contains(&long.as_str()))
        .filter(|(long, _)| !refused.contains(long))
        .collect()
}

/// Whether `arg` is a switch: it takes no value.
fn is_switch(arg: &Arg) -> bool {
    matches!(
        arg.get_action(),
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count
    )
}

/// Why the file at `path` may not be read, if it may not: not there, not a regular file,
/// or writable by group or other.
///
/// # Errors
/// The reason, naming the file.
pub fn check_file(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("cannot read the config file {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "the config file {} is not a regular file",
            path.display()
        ));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode & WRITABLE_BY_OTHERS != 0 {
        return Err(format!(
            "the config file {} is writable by group or other (mode {mode:04o}): it decides what \
             the process serves, so run `chmod go-w {}`",
            path.display(),
            path.display(),
        ));
    }
    Ok(())
}

/// The flags the file at `path` stands for, for the mode whose own command is `own`:
/// `--key=value` per setting, in the file's order of tables and keys, leaving out each
/// flag in `given` (an id of `own`: it was given on the command line, which wins).
///
/// # Errors
/// Every problem of the file, one per line: it may not be read ([`check_file`]), it is
/// not TOML, a key is not a flag of the mode or is in the wrong table, or a value is not
/// what the flag takes.
pub fn file_args(
    own: &Command,
    refused: &BTreeSet<String>,
    path: &Path,
    given: &BTreeSet<String>,
) -> Result<Vec<OsString>, String> {
    check_file(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the config file {}: {e}", path.display()))?;
    let table: toml::Table = toml::from_str(&text)
        .map_err(|e| format!("the config file {} is not valid TOML: {e}", path.display()))?;
    let known = settings(own, refused);
    let mut args = Vec::new();
    let mut problems = Vec::new();
    for (group, body) in &table {
        let toml::Value::Table(keys) = body else {
            problems.push(match group_of(group) {
                Some(g) if known.contains_key(group.as_str()) => format!(
                    "key `{group}` in {} is outside a table: it belongs under [{g}]",
                    path.display()
                ),
                _ => format!(
                    "unknown key `{group}` in {}: settings are keys of tables, one per group",
                    path.display()
                ),
            });
            continue;
        };
        for (key, value) in keys {
            let Some(arg) = known.get(key.as_str()) else {
                problems.push(format!(
                    "unknown key `{group}.{key}` in {}: kbf has no flag --{key} in this mode",
                    path.display()
                ));
                continue;
            };
            match group_of(key) {
                Some(g) if g == group => {}
                Some(g) => {
                    problems.push(format!(
                        "unknown key `{group}.{key}` in {}: --{key} is a setting of [{g}]",
                        path.display()
                    ));
                    continue;
                }
                None => {
                    problems.push(format!(
                        "unknown key `{group}.{key}` in {}: --{key} is in no group",
                        path.display()
                    ));
                    continue;
                }
            }
            if given.contains(arg.get_id().as_str()) {
                continue;
            }
            match values(arg, key, value) {
                Ok(v) => args.extend(v),
                Err(why) => {
                    problems.push(format!("key `{group}.{key}` in {}: {why}", path.display()))
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(args)
    } else {
        Err(problems.join("\n"))
    }
}

/// The flags one key stands for.
fn values(arg: &Arg, key: &str, value: &toml::Value) -> Result<Vec<OsString>, String> {
    use toml::Value as V;
    let one = |v: &V| -> Result<String, String> {
        match v {
            V::String(s) => Ok(s.clone()),
            V::Integer(i) => Ok(i.to_string()),
            V::Float(f) => Ok(f.to_string()),
            V::Boolean(b) => Ok(b.to_string()),
            V::Datetime(_) | V::Array(_) | V::Table(_) => {
                Err("expects a string or a number".to_owned())
            }
        }
    };
    if is_switch(arg) {
        return match value {
            V::Boolean(true) => Ok(vec![format!("--{key}").into()]),
            V::Boolean(false) => Ok(Vec::new()),
            _ => Err("is a switch: it takes true or false".to_owned()),
        };
    }
    match value {
        V::Array(items) if matches!(arg.get_action(), ArgAction::Append) => items
            .iter()
            .map(|v| one(v).map(|s| format!("--{key}={s}").into()))
            .collect(),
        V::Array(_) => Err("takes one value, not an array".to_owned()),
        v => Ok(vec![format!("--{key}={}", one(v)?).into()]),
    }
}

/// The whole file for the mode whose own command is `own`: every setting in its group's
/// table, with the flag's help; a setting with a default is a live line holding it, one
/// without is a comment showing its shape. A file of the live lines alone is the mode's
/// defaults.
///
/// # Panics
/// On a flag that [`group_of`] does not place (`config::tests` refuses that).
#[must_use]
pub fn template(mode: &str, own: &Command, refused: &BTreeSet<String>) -> String {
    let mut groups: BTreeMap<&'static str, Vec<(String, &Arg)>> = BTreeMap::new();
    for (long, arg) in settings(own, refused) {
        let group = group_of(&long).unwrap_or_else(|| panic!("--{long} is in no group"));
        groups.entry(group).or_default().push((long, arg));
    }
    let mut out = format!(
        "# kbf {mode}: every setting, one table per group; a key is a flag's long name.\n\
         # Generated from the flag definitions (tests/golden). A flag on the command line\n\
         # overrides its key. A secret is the path of the file that holds it.\n"
    );
    for (group, keys) in groups {
        out.push_str(&format!("\n[{group}]\n"));
        for (long, arg) in keys {
            out.push_str(&entry(&long, arg));
        }
    }
    out
}

fn entry(long: &str, arg: &Arg) -> String {
    let mut out = String::new();
    if let Some(help) = arg.get_help() {
        for line in help.to_string().lines() {
            out.push_str(&format!("# {line}\n"));
        }
    }
    if arg.is_hide_set() {
        out.push_str("# (not in --help)\n");
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    let shape = if is_switch(arg) {
        "false".to_owned()
    } else if matches!(arg.get_action(), ArgAction::Append) {
        let items: Vec<String> = defaults.iter().map(|d| literal(d)).collect();
        format!("[{}]", items.join(", "))
    } else {
        match defaults.first() {
            Some(d) => literal(d),
            None => literal(
                &arg.get_value_names()
                    .and_then(|n| n.first().map(ToString::to_string))
                    .unwrap_or_else(|| long.to_uppercase()),
            ),
        }
    };
    let live = is_switch(arg) || !defaults.is_empty();
    if live {
        out.push_str(&format!("{long} = {shape}\n"));
    } else {
        out.push_str(&format!("# {long} = {shape}\n"));
    }
    out
}

/// A TOML literal for a default: a number as itself, anything else a string.
fn literal(value: &str) -> String {
    if value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok_and(f64::is_finite) {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests;
