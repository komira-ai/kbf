//! Probe P0: a root offline write into the Data volume of a guest's disk image.
//!
//! The raw image is attached with `hdiutil` (ownership honoured), the volume that holds
//! `private/var/db` is found among the mounts, files are written and read back, and the
//! image is detached again. macOS only; the guest must be off.

use std::io::Cursor;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::system::OfflineFile;

/// What `hdiutil attach -plist` reported.
#[derive(Debug, PartialEq, Eq)]
pub struct Attach {
    /// The whole-disk device node (`/dev/disk4`).
    pub device: String,
    /// Every `dev-entry`, in order.
    pub entries: Vec<String>,
    pub mounts: Vec<PathBuf>,
}

/// Reads the plist `hdiutil attach -plist` prints.
///
/// # Errors
/// The text is not that plist, or it names no device.
pub fn parse_attach(plist_bytes: &[u8]) -> Result<Attach, String> {
    let value = plist::Value::from_reader(Cursor::new(plist_bytes))
        .map_err(|e| format!("not a plist: {e}"))?;
    let entities = value
        .as_dictionary()
        .and_then(|d| d.get("system-entities"))
        .and_then(plist::Value::as_array)
        .ok_or("no system-entities in the attach output")?;
    let mut entries = Vec::new();
    let mut mounts = Vec::new();
    for e in entities {
        let Some(d) = e.as_dictionary() else { continue };
        if let Some(dev) = d.get("dev-entry").and_then(plist::Value::as_string) {
            entries.push(dev.to_owned());
        }
        if let Some(m) = d.get("mount-point").and_then(plist::Value::as_string) {
            mounts.push(PathBuf::from(m));
        }
    }
    let device = entries
        .first()
        .cloned()
        .ok_or("the attach output names no device")?;
    Ok(Attach {
        device,
        entries,
        mounts,
    })
}

/// The mount that is the Data volume: the one holding `private/var/db`.
#[must_use]
pub fn data_volume(mounts: &[PathBuf]) -> Option<&PathBuf> {
    mounts.iter().find(|m| m.join("private/var/db").is_dir())
}

fn run(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run {program}: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn put(root: &Path, f: &OfflineFile) -> Result<Value, String> {
    let path = root.join(&f.path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, &f.contents).map_err(|e| format!("write {}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(f.mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    Ok(json!({
        "path": f.path,
        "uid": meta.uid(),
        "gid": meta.gid(),
        "mode": format!("{:o}", meta.mode() & 0o7777),
        "bytes": meta.len(),
    }))
}

/// Attaches `disk`, writes `files` into its Data volume, and detaches.
///
/// # Errors
/// A step failed; the image is detached whenever the attach worked.
pub fn write(disk: &Path, files: &[OfflineFile]) -> Result<Value, String> {
    let disk_s = disk.to_string_lossy();
    let out = run(
        "/usr/bin/hdiutil",
        &[
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-owners",
            "on",
            "-nobrowse",
            "-plist",
            &disk_s,
        ],
    )?;
    let attach = parse_attach(&out)?;
    let result = (|| {
        let root = data_volume(&attach.mounts).ok_or_else(|| {
            format!(
                "no mounted volume holds private/var/db (mounts: {:?}, devices: {:?})",
                attach.mounts, attach.entries
            )
        })?;
        let written = files
            .iter()
            .map(|f| put(root, f))
            .collect::<Result<Vec<_>, _>>()?;
        Ok::<_, String>(json!({"data_volume": root, "written": written}))
    })();
    let detached = run("/usr/bin/hdiutil", &["detach", "-force", &attach.device]);
    let mut observed = result?;
    observed["attached"] = json!(attach.entries);
    observed["mounts"] = json!(attach.mounts);
    observed["detached"] = json!(detached.is_ok());
    if let Err(e) = detached {
        observed["detach_error"] = json!(e);
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>system-entities</key><array>
<dict><key>content-hint</key><string>GUID_partition_scheme</string><key>dev-entry</key><string>/dev/disk4</string></dict>
<dict><key>dev-entry</key><string>/dev/disk4s1</string></dict>
<dict><key>dev-entry</key><string>/dev/disk5s1</string><key>mount-point</key><string>/Volumes/Data</string></dict>
</array></dict></plist>"#;

    /// Catches a parser that takes the first mount-less entry as a mount, or loses the whole-disk device.
    #[test]
    fn attach_output_gives_the_device_entries_and_mounts() {
        let a = parse_attach(SAMPLE.as_bytes()).expect("parse");
        assert_eq!(a.device, "/dev/disk4");
        assert_eq!(a.entries.len(), 3);
        assert_eq!(a.mounts, vec![PathBuf::from("/Volumes/Data")]);
    }

    /// Catches a parser that accepts a plist with no entities as an attach.
    #[test]
    fn attach_output_without_devices_is_refused() {
        let bare = b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>system-entities</key><array/></dict></plist>";
        assert!(parse_attach(bare).is_err());
        assert!(parse_attach(b"not a plist").is_err());
    }
}
