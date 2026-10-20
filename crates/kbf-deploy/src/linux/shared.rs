//! What the per-mode verbs (`--mode`) share: the check result types, the slice and the
//! values every unit template takes, and the host reads and constants `install-units`
//! plans with.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use super::config::{LOOPBACK_MINIO, LinuxConfig};
use super::host::{CRUN, HostError, NEWGIDMAP, NEWUIDMAP, SYSTEMCTL, inspect, stat, tool, under};
use crate::template::{self, TemplateError, Value, Values};
use crate::unit::UnitError;
use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::step::{Cmd, Owner};
use kbf_host::user::RunUser;

/// The slice file, under the output directory.
pub const SLICE: &str = "kbf.slice";
/// Where systemd reads units.
pub(super) const UNITS: &str = "/etc/systemd/system";
const LINGER: &str = "/var/lib/systemd/linger";
/// What `render-units` writes when `--kbf-uid` is not given: the kbf user's uid (the
/// daemon's `XDG_RUNTIME_DIR`) as a placeholder, which `check-units --installable`
/// refuses.
pub const UID_PLACEHOLDER: &str = "<kbf uid>";
/// How long kbf-server has to answer `/readyz` after a start.
pub const READY: Duration = Duration::from_secs(60);
/// Who owns each binary, `.previous` copy and unit install-units writes: root, user and
/// group. The in-place check compares it too, so a right file another user owns (and so
/// could rewrite what systemd runs as root) is replaced.
pub(super) const ROOT: Owner = Owner { uid: 0, gid: 0 };

/// One broken rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub rule: &'static str,
    pub message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FAIL {}: {}", self.rule, self.message)
    }
}

/// Why the units could not be checked at all.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("{name}: {source}")]
    Unit { name: String, source: UnitError },
}

/// What the check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub failures: Vec<Failure>,
    /// Lines about a rule not checked, such as `SKIP traverse: ...`; not failures.
    pub notes: Vec<String>,
    /// The summary line: `UNITS OK: ...` or `UNITS BROKEN: ...`.
    pub summary: String,
}

/// The first `<lower case words>` placeholder in `text`, such as `<kbf uid>`.
pub(super) fn angle_placeholder(text: &str) -> Option<&str> {
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let words = after.split_once('>').map(|(w, _)| w).filter(|w| {
            w.starts_with(|c: char| c.is_ascii_lowercase())
                && w.chars().all(|c| c.is_ascii_lowercase() || c == ' ')
        });
        if let Some(w) = words {
            return Some(&rest[open..open + w.len() + 2]);
        }
        rest = after;
    }
    None
}

/// `kbf.slice`, rendered from the template compiled in from `templates/linux/kbf.slice`;
/// `kbf_uid` is the kbf user's uid, or `None` to leave [`UID_PLACEHOLDER`].
pub fn render_slice(c: &LinuxConfig, kbf_uid: Option<u32>) -> Result<String, TemplateError> {
    template::render(
        SLICE,
        include_str!("../../templates/linux/kbf.slice"),
        &values(c, kbf_uid),
    )
}

pub(super) fn values(c: &LinuxConfig, kbf_uid: Option<u32>) -> Values {
    let slice_mib =
        (c.minio_reservation_gib + c.server_reservation_gib) * 1024 + c.daemon_memory_min_mib;
    // kbf-daemon --image takes docker://<repo>@sha256:<digest>; KBF_IMAGES holds the
    // bare form.
    let images = c
        .images
        .iter()
        .map(|i| format!("    --image=docker://{i} \\"))
        .collect();
    // The cold store's `s3://` path: the bucket, then the prefix when there is one.
    let s3_location = format!(
        "{}{}{}",
        c.s3_bucket,
        if c.s3_prefix.is_empty() { "" } else { "/" },
        c.s3_prefix
    );
    // The daemon's tuning flags, each on a line of its own in the continued command line.
    let tuning = c
        .reserve_cpus
        .map(|n| format!("    --reserve-cpus={n} \\"))
        .into_iter()
        .chain(
            c.lease_placement
                .map(|p| format!("    --lease-placement={} \\", p.as_str())),
        )
        .collect();
    BTreeMap::from([
        ("SLICE_MEMORY_MIN", Value::Text(format!("{slice_mib}M"))),
        (
            "MINIO_MEMORY_MIN",
            Value::Text(format!("{}G", c.minio_reservation_gib)),
        ),
        (
            "SERVER_MEMORY_MIN",
            Value::Text(format!("{}G", c.server_reservation_gib)),
        ),
        (
            "DAEMON_MEMORY_MIN",
            Value::Text(format!("{}M", c.daemon_memory_min_mib)),
        ),
        (
            "ACTIONS_MEMORY_MAX_GIB",
            Value::Text(c.actions_gib.to_string()),
        ),
        (
            "KBF_DAEMON_MEMORY_MIN_MIB",
            Value::Text(c.daemon_memory_min_mib.to_string()),
        ),
        (
            "KBF_UID",
            Value::Text(kbf_uid.map_or(UID_PLACEHOLDER.to_owned(), |u| u.to_string())),
        ),
        ("MINIO_UNIT", Value::Text(c.minio_unit.clone())),
        (
            "MINIO_S3_ENDPOINT",
            Value::Text(
                c.minio_s3_endpoint
                    .clone()
                    .unwrap_or_else(|| LOOPBACK_MINIO.to_owned()),
            ),
        ),
        ("KBF_S3_BUCKET", Value::Text(c.s3_bucket.clone())),
        ("KBF_S3_LOCATION", Value::Text(s3_location)),
        ("KBF_FRONTEND_NAME", Value::Text(c.frontend_name.clone())),
        (
            "KBF_STORAGE_URL",
            Value::Text(c.storage_url.clone().unwrap_or_default()),
        ),
        ("KBF_REAPI_CERT", Value::Text(c.reapi_cert.to_string())),
        ("KBF_REAPI_KEY", Value::Text(c.reapi_key.to_string())),
        (
            "KBF_API_TOKEN_FILE",
            Value::Text(c.api_token_file.to_string()),
        ),
        ("KBF_STATE_STORE", Value::Text(c.state_store.clone())),
        ("DAEMON_TUNING_ARGS", Value::Lines(tuning)),
        ("KBF_LISTEN_HOST", Value::Text(c.listen_host())),
        ("KBF_HOST_NAME", Value::Text(c.host_name.clone())),
        ("KBF_NODE_ID", Value::Text(c.node_id.clone())),
        ("KBF_REAPI_PORT", Value::Text(c.reapi_port.to_string())),
        ("KBF_WORKER_PORT", Value::Text(c.worker_port.to_string())),
        ("KBF_API_PORT", Value::Text(c.api_port.to_string())),
        ("KBF_STORAGE_PORT", Value::Text(c.storage_port.to_string())),
        ("KBF_DATA_DIR", Value::Text(c.data_dir.to_string())),
        ("KBF_META_DIR", Value::Text(c.meta_dir.to_string())),
        ("KBF_HOT_DIR", Value::Text(c.hot_dir.to_string())),
        ("KBF_USER", Value::Text(c.user.clone())),
        ("KBF_SERVER_USER", Value::Text(c.server_user.clone())),
        ("KBF_ETC", Value::Text(c.etc.to_string())),
        ("KBF_BIN_DIR", Value::Text(c.bin_dir.to_string())),
        ("DAEMON_IMAGE_ARGS", Value::Lines(images)),
    ])
}

/// `kbf` can run rootless containers with `crun`: it has subuid and subgid ranges (which
/// `newuidmap` and `newgidmap` map), the runtime and those two tools are installed, and
/// it lingers (the unit's `XDG_RUNTIME_DIR`).
///
/// Whether the two tools are setuid or hold file capabilities is the daemon's start-up
/// check: a capability is not something this kit reads.
pub(super) fn rootless<E: Exec>(exec: &E, kbf: &RunUser) -> Result<(), HostError> {
    // A range line is `<user>:<first id>:<count>`, the user named or given by uid
    // (subuid(5); newuidmap takes either).
    let uid = kbf.uid.to_string();
    for file in ["/etc/subuid", "/etc/subgid"] {
        let file = HostPath::new(file)?;
        let ranged = text_of(exec, &file)?.is_some_and(|t| {
            t.lines().any(|l| {
                l.split_once(':')
                    .is_some_and(|(who, _)| who == kbf.name || who == uid)
            })
        });
        if !ranged {
            return Err(HostError::NoSubIds {
                user: kbf.name.clone(),
                file,
            });
        }
    }
    for (path, package) in [(CRUN, "crun"), (NEWUIDMAP, "uidmap"), (NEWGIDMAP, "uidmap")] {
        let path = HostPath::new(path)?;
        if stat(exec, &path)?.is_none_or(|m| m.kind == Kind::Dir) {
            return Err(HostError::NoTool { path, package });
        }
    }
    if stat(exec, &under(&HostPath::new(LINGER)?, &kbf.name)?)?.is_none() {
        return Err(HostError::NoLinger {
            user: kbf.name.clone(),
            uid: kbf.uid,
        });
    }
    Ok(())
}

/// A file's text, `None` when there is no regular file at `path`.
pub(super) fn text_of<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<String>, HostError> {
    Ok(match inspect(exec, path)? {
        Some(Entry::File { bytes, .. }) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        _ => None,
    })
}

pub(super) fn systemctl(args: &[&str]) -> Cmd {
    args.iter()
        .fold(Cmd::new(tool(SYSTEMCTL)), |cmd, arg| cmd.arg(*arg))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a `<...>` placeholder missed when another `<` comes first (an XML-ish
    /// comment, `<B>`, `<x1>`), and a `<` with no `>` read as one.
    #[test]
    fn angle_placeholders_are_lower_case_words_in_brackets() {
        assert_eq!(
            angle_placeholder("a <B> <x1> < y> <kbf uid> <z>"),
            Some("<kbf uid>")
        );
        assert_eq!(angle_placeholder("a < b"), None);
        assert_eq!(angle_placeholder("<>"), None);
    }
}
