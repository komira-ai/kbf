//! What the installer reads from a Mac's `kbf.toml` to find the daemon's lease
//! directories: the daemon's mode, its driver and its `data-dir`, and when the file last
//! changed.
//!
//! The file is the one source of the daemon's settings (the launchd job names only
//! `--config <file>`), and provisioning makes its directory root's, so it is read as the
//! other root-trusted files are: through [`trusted_chain`] (a plain file of root's that
//! group and others cannot write, in such directories). Anything the installer cannot
//! be sure describes a daemon whose leases it can see is an `Err`, which the lease check
//! reports as "cannot tell":
//!
//! - a file that is missing, a link, untrusted, over 1 MiB, not UTF-8 or not TOML;
//! - a top-level `mode` that is not `"daemon"`;
//! - a `[daemon] driver` that is not exactly `["native"]` (another driver holds leases
//!   this check does not see);
//! - a `[daemon] data-dir` that is missing (the daemon would default to a directory this
//!   crate does not copy) or not absolute;
//! - a `[macos]` pool key (`pool-users`, `pool-first-uid`): pool mode is not read by this
//!   source yet.
//!
//! The file's last change is the later of its modification and status-change times, read
//! after its bytes: a file replaced between the two reads then reads as newer, never older.

use std::os::unix::fs::MetadataExt;

use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;

use crate::layout::Layout;
use crate::trust::trusted_chain;

/// The most of `kbf.toml` that is read.
const MAX_BYTES: usize = 1 << 20;

/// The part of the daemon's configuration the lease check uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// `[daemon] data-dir`: the lease directories are `<data-dir>/leases/lease-*`.
    pub data_dir: HostPath,
    /// When the file last changed, in seconds since the epoch (whole seconds, rounded
    /// down).
    pub changed_at: i64,
}

/// The daemon's configuration on the host `layout` describes, with the file judged by
/// `trust`; why not when it cannot be relied on.
pub fn read(trust: &dyn Exec, layout: &Layout) -> Result<DaemonConfig, String> {
    let file = layout.kbf_toml();
    let entry = trust
        .inspect(&file)
        .map_err(|e| format!("{file}: {e}"))?
        .ok_or_else(|| format!("{file} is not there"))?;
    let Entry::File { bytes, .. } = entry else {
        return Err(format!("{file} is not a plain file"));
    };
    trusted_chain(trust, &file, Kind::File).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("{file} is over {MAX_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{file} is not UTF-8 text"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("{file} is not TOML: {}", e.message()))?;
    check_daemon(&file, &table)?;
    let data_dir = data_dir(&file, &table)?;
    let meta = std::fs::symlink_metadata(layout.join(&file)).map_err(|e| format!("{file}: {e}"))?;
    Ok(DaemonConfig {
        data_dir,
        changed_at: meta.mtime().max(meta.ctime()),
    })
}

/// `Ok` when the file describes a daemon in daemon-user mode with the native driver.
fn check_daemon(file: &HostPath, table: &toml::Table) -> Result<(), String> {
    match table.get("mode").and_then(toml::Value::as_str) {
        Some("daemon") => {}
        Some(other) => {
            return Err(format!(
                "{file} mode is {other:?}: only a daemon host's leases are known"
            ));
        }
        None => return Err(format!("{file} has no mode")),
    }
    let driver: Option<Vec<&str>> = section(table, "daemon")
        .and_then(|d| d.get("driver"))
        .and_then(toml::Value::as_array)
        .map(|a| a.iter().filter_map(toml::Value::as_str).collect());
    if driver.as_deref() != Some(&["native"]) {
        return Err(format!(
            "{file} driver is {driver:?}, not [\"native\"]: another driver holds leases this installer cannot see"
        ));
    }
    if let Some(macos) = section(table, "macos")
        && ["pool-users", "pool-first-uid"]
            .iter()
            .any(|k| macos.contains_key(*k))
    {
        return Err(format!(
            "{file} sets a pool key: pool mode leases are not read by this installer yet"
        ));
    }
    Ok(())
}

/// `[daemon] data-dir`, an absolute path.
fn data_dir(file: &HostPath, table: &toml::Table) -> Result<HostPath, String> {
    section(table, "daemon")
        .and_then(|d| d.get("data-dir"))
        .and_then(toml::Value::as_str)
        .and_then(|d| HostPath::new(d).ok())
        .ok_or_else(|| format!("{file} has no absolute data-dir"))
}

/// The table `name` of the file.
fn section<'a>(table: &'a toml::Table, name: &str) -> Option<&'a toml::Table> {
    table.get(name).and_then(toml::Value::as_table)
}
