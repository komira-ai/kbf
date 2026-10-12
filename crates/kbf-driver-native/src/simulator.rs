//! A simulator of the lease's own: an action that names the platform property
//! [`PROPERTY`] (`simulator=<device type>,<runtime>`) gets a new simulator, made for it
//! when its lease starts and deleted when the lease ends. The device is in the daemon
//! user's default device set (`~/Library/Developer/CoreSimulator/Devices`), named
//! [`NAME_PREFIX`]`<term>-<seq>` for its lease: `xcodebuild -destination` finds only the
//! devices of that set, not those of a set elsewhere (`simctl --set`).
//!
//! - **At start**, after the inputs are written and before the action runs, the driver
//!   writes [`MARKER`] in the lease directory, then runs `xcrun simctl create
//!   kbf-lease-<term>-<seq> <device type> <runtime>` (with the action's
//!   `DEVELOPER_DIR` when it names an `xcode`). It must exit 0 within [`ANSWER_WITHIN`]
//!   and print the new device's UDID; otherwise the lease fails
//!   ([`RuntimeError::Failed`]: a runtime the node lacks is the node's to fix, and the
//!   action is not run). The action finds the UDID in [`UDID_VAR`], which wins over the
//!   Command's environment. The device is made shut down: the action boots it (`xcrun
//!   simctl boot "$KBF_SIMULATOR_UDID"`) or lets `xcodebuild` do it.
//! - **At the end**, whatever the outcome (an exit, a failure, a timeout, a kill, a run
//!   the daemon drops), before the lease directory is removed: the device the create
//!   printed, and every device named for the lease in `simctl list devices` (one a
//!   create cut short made late), is shut down and then deleted, by UDID, with the
//!   node's own Xcode. The UDID the create printed is kept by the daemon, so an action
//!   that renames its device or edits its lease directory cannot point the delete at
//!   another device. A shutdown that fails is not looked at (a device that is not
//!   booted refuses one); a delete that fails fails the lease, as a lease directory
//!   that stays does, so a dirty node is loud. Each command names one device, never
//!   `all`.
//! - **At daemon start**, when a lease directory a killed daemon left holds [`MARKER`]
//!   (a regular file, looked at without following a link), every device in the
//!   default set whose name starts with [`NAME_PREFIX`] is shut down and deleted, with
//!   `simctl` run in the first such directory before the start-up sweep removes it:
//!   only the daemon's leases make such devices, and none of them runs before the
//!   daemon starts. A daemon that was never killed during a simulator lease asks
//!   `simctl` nothing at start.
//!
//! `simctl` runs as an action does ([`crate::xcode::Sandbox`]): under the actions'
//! sandbox with the network off and the lease directory as its own, so the daemon runs
//! no developer tool outside the sandbox. `simctl` asks `CoreSimulatorService`, which
//! runs outside the sandbox, to make, boot and delete the device. So do the action's
//! own `simctl` and `xcodebuild`: the processes of a booted simulator run under that
//! service, outside the action's sandbox and its network-off rule, and the action can
//! reach every device of the default set, not only its own.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_daemon::RuntimeError;
use kbf_proto::reapi::{Action, Command, Platform};

use crate::network::Isolation;
use crate::xcode::{self, DEVELOPER_DIR, Sandbox};

/// The platform property that asks for a simulator of the lease's own.
pub const PROPERTY: &str = "simulator";

/// The variable that names the lease's simulator's UDID to the action.
pub const UDID_VAR: &str = "KBF_SIMULATOR_UDID";

/// How the name of every lease's simulator starts; the rest is `<term>-<seq>`.
pub const NAME_PREFIX: &str = "kbf-lease-";

/// The file in the lease directory that says the lease may have made a simulator:
/// written before the create, so the start after a killed daemon sweeps the devices.
pub const MARKER: &str = "simulator";

/// How long each `simctl` command has to exit.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(120);

/// The simulator an action asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// A device type, by identifier
    /// (`com.apple.CoreSimulator.SimDeviceType.iPhone-16`) or name (`iPhone 16`).
    pub device_type: String,
    /// A runtime, by identifier (`com.apple.CoreSimulator.SimRuntime.iOS-18-4`).
    pub runtime: String,
}

/// The simulator the action's platform (the Action's, or the Command's for clients
/// older than REAPI 2.2) asks for with [`PROPERTY`], its name read in any case; `None`
/// when it asks for none.
///
/// # Errors
/// [`RuntimeError::Invalid`] for a value that is not `<device type>,<runtime>`, both
/// parts not empty: the client's to fix.
#[allow(deprecated)]
pub fn request_of(action: &Action, command: &Command) -> Result<Option<Request>, RuntimeError> {
    let platform: Option<&Platform> = action
        .platform
        .as_ref()
        .filter(|p| !p.properties.is_empty())
        .or(command.platform.as_ref());
    let Some(value) = platform
        .into_iter()
        .flat_map(|p| &p.properties)
        .find(|p| p.name.eq_ignore_ascii_case(PROPERTY))
        .map(|p| p.value.as_str())
    else {
        return Ok(None);
    };
    match value.split_once(',') {
        Some((device_type, runtime))
            if !device_type.is_empty() && !runtime.is_empty() && !runtime.contains(',') =>
        {
            Ok(Some(Request {
                device_type: device_type.to_owned(),
                runtime: runtime.to_owned(),
            }))
        }
        _ => Err(RuntimeError::Invalid(format!(
            "platform property {PROPERTY}={value:?}: use <device type>,<runtime>"
        ))),
    }
}

/// How the driver runs `simctl`: this `xcrun`, under this isolation with these
/// user-folder rules.
#[derive(Clone, Debug)]
pub(crate) struct Simctl {
    pub xcrun: PathBuf,
    pub isolation: Isolation,
    pub rules: String,
    pub within: Duration,
}

/// The simulator a lease may have: what its end needs, set before the create runs.
#[derive(Clone, Debug)]
pub(crate) struct Made {
    /// The lease directory's real path, the sandbox's directory for `simctl`.
    pub lease: PathBuf,
    /// The device's name, [`NAME_PREFIX`]`<term>-<seq>`.
    pub name: String,
    /// The UDID the create printed, once it did.
    pub udid: Option<String>,
}

impl Simctl {
    /// `xcrun simctl args` under the sandbox of `lease` (a real path), with the Xcode
    /// at `developer_dir`, or the node's own with the daemon's `DEVELOPER_DIR` removed.
    fn run(
        &self,
        lease: &Path,
        developer_dir: Option<&Path>,
        args: &[&str],
    ) -> Result<std::process::Output, String> {
        let mut all = vec!["simctl"];
        all.extend_from_slice(args);
        let sandbox = Sandbox {
            isolation: self.isolation.clone(),
            dir: lease.to_owned(),
            rules: self.rules.clone(),
        };
        let mut command = sandbox.command(&self.xcrun, &all);
        match developer_dir {
            Some(dir) => command.env(DEVELOPER_DIR, dir),
            None => command.env_remove(DEVELOPER_DIR),
        };
        let shown = || format!("simctl {}", args.join(" "));
        let out = xcode::output_within(command, self.within, lease)
            .map_err(|e| format!("{}: {e}", shown()))?;
        if out.status.success() {
            Ok(out)
        } else {
            Err(format!(
                "{}: {}: {}",
                shown(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Writes [`MARKER`] in `made.lease`, then makes a new simulator named
    /// `made.name`; its UDID.
    pub(crate) fn create(
        &self,
        made: &Made,
        request: &Request,
        developer_dir: Option<&Path>,
    ) -> Result<String, String> {
        let marker = made.lease.join(MARKER);
        at(&marker, std::fs::write(&marker, b""))?;
        let args = ["create", &made.name, &request.device_type, &request.runtime];
        let out = self.run(&made.lease, developer_dir, &args)?;
        let printed = String::from_utf8_lossy(&out.stdout);
        let udid = printed.trim();
        if is_udid(udid) {
            Ok(udid.to_owned())
        } else {
            Err(format!("simctl create printed no UDID: {udid:?}"))
        }
    }

    /// Shuts down and deletes the lease's simulator: the device the create printed and
    /// every device named for the lease; why each delete that failed, or the listing,
    /// did. The end of every lease that asked for a simulator.
    pub(crate) fn end(&self, made: &Made) -> Result<(), String> {
        let named = self.devices(&made.lease);
        let mut udids: Vec<String> = made.udid.iter().cloned().collect();
        let mut failed = Vec::new();
        match named {
            Ok(named) => {
                for (name, udid) in named {
                    if name == made.name && !udids.contains(&udid) {
                        udids.push(udid);
                    }
                }
            }
            Err(why) => failed.push(why),
        }
        failed.extend(self.delete(&made.lease, &udids));
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }

    /// Shuts down and deletes every device whose name starts with [`NAME_PREFIX`],
    /// running `simctl` in the lease directory `lease` a killed daemon left; why the
    /// listing or each delete that failed did. The start after a killed daemon left a
    /// lease that may have made one.
    pub(crate) fn sweep(&self, lease: &Path) -> Result<(), String> {
        let lease = at(lease, std::fs::canonicalize(lease))?;
        let udids: Vec<String> = (self.devices(&lease)?.into_iter())
            .filter(|(name, _)| name.starts_with(NAME_PREFIX))
            .map(|(_, udid)| udid)
            .collect();
        let failed = self.delete(&lease, &udids);
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }

    /// Shuts down and deletes each of `udids`, one command each; why each delete that
    /// failed did.
    fn delete(&self, dir: &Path, udids: &[String]) -> Vec<String> {
        let mut failed = Vec::new();
        for udid in udids {
            // A device that is not booted refuses a shutdown: only the delete counts.
            let _ = self.run(dir, None, &["shutdown", udid]);
            if let Err(why) = self.run(dir, None, &["delete", udid]) {
                failed.push(why);
            }
        }
        failed
    }

    /// The name and UDID of every device `simctl list devices -j` prints whose UDID
    /// is one, in its order.
    fn devices(&self, dir: &Path) -> Result<Vec<(String, String)>, String> {
        let out = self.run(dir, None, &["list", "devices", "-j"])?;
        devices_in(&out.stdout).map_err(|e| format!("simctl list devices -j: {e}"))
    }
}

/// [`Simctl::devices`] from what `simctl list devices -j` printed:
/// `{"devices": {"<runtime>": [{"name": .., "udid": ..}, ..], ..}}`.
fn devices_in(json: &[u8]) -> Result<Vec<(String, String)>, String> {
    let list: serde_json::Value = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    let Some(runtimes) = list.get("devices").and_then(serde_json::Value::as_object) else {
        return Err("no \"devices\" object".to_owned());
    };
    let mut found = Vec::new();
    for device in runtimes
        .values()
        .filter_map(serde_json::Value::as_array)
        .flatten()
    {
        let field = |key: &str| device.get(key).and_then(serde_json::Value::as_str);
        if let (Some(name), Some(udid)) = (field("name"), field("udid"))
            && is_udid(udid)
        {
            found.push((name.to_owned(), udid.to_owned()));
        }
    }
    Ok(found)
}

/// Whether the lease directory at `lease` holds [`MARKER`] as a regular file, looked at
/// without following a link.
pub(crate) fn marked(lease: &Path) -> bool {
    std::fs::symlink_metadata(lease.join(MARKER)).is_ok_and(|m| m.file_type().is_file())
}

/// `result`, its error said with the `path` it is about.
fn at<T>(path: &Path, result: std::io::Result<T>) -> Result<T, String> {
    result.map_err(|e| format!("{}: {e}", path.display()))
}

/// Whether `text` is a UDID as `simctl` prints one: 8-4-4-4-12 hexadecimal digits.
fn is_udid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::platform::Property;

    use super::*;

    fn with(name: &str, value: &str) -> Option<Platform> {
        Some(Platform {
            properties: vec![Property {
                name: name.to_owned(),
                value: value.to_owned(),
            }],
        })
    }

    /// Catches: the property read only in one spelling or only from the Action, a
    /// value split anywhere but at its one comma, and a malformed value let through to
    /// `simctl` (which would fail the lease as the node's fault, not the client's).
    #[test]
    #[allow(deprecated)]
    fn the_request_is_a_device_type_and_a_runtime() {
        let want = Request {
            device_type: "iPhone 16".to_owned(),
            runtime: "com.apple.CoreSimulator.SimRuntime.iOS-18-4".to_owned(),
        };
        let value = "iPhone 16,com.apple.CoreSimulator.SimRuntime.iOS-18-4";
        for name in ["simulator", "Simulator", "SIMULATOR"] {
            let action = Action {
                platform: with(name, value),
                ..Action::default()
            };
            let got = request_of(&action, &Command::default()).expect("valid");
            assert_eq!(got.as_ref(), Some(&want), "{name}");
        }
        let command = Command {
            platform: with("simulator", value),
            ..Command::default()
        };
        assert_eq!(
            request_of(&Action::default(), &command).expect("valid"),
            Some(want)
        );
        let none = Action {
            platform: with("xcode", "16C5032a"),
            ..Action::default()
        };
        assert_eq!(request_of(&none, &Command::default()).expect("none"), None);
        for bad in ["", "iPhone 16", "iPhone 16,", ",iOS-18-4", "a,b,c"] {
            let action = Action {
                platform: with("simulator", bad),
                ..Action::default()
            };
            // Read through Debug: a pattern guard here would be a branch never taken.
            let got = format!("{:?}", request_of(&action, &Command::default()));
            assert!(got.starts_with("Err(Invalid("), "{bad:?}: {got}");
            assert!(got.contains(PROPERTY), "{bad:?}: {got}");
        }
    }

    /// Catches: a device of another runtime missed (the list is grouped by runtime), an
    /// entry whose UDID is not one taken for a device, and a list that is not one read
    /// as empty (the end would delete nothing and say nothing).
    #[test]
    fn the_device_list_is_every_runtimes_devices() {
        let json = br#"{"devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-18-4": [
                {"name": "kbf-lease-1-1", "udid": "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"},
                {"name": "odd", "udid": "not-a-udid"},
                {"udid": "1A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"}
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
                {"name": "iPhone 17", "udid": "2A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"}
            ],
            "empty": "not a list"
        }}"#;
        assert_eq!(
            devices_in(json).expect("a list"),
            [
                (
                    "kbf-lease-1-1".to_owned(),
                    "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9".to_owned()
                ),
                (
                    "iPhone 17".to_owned(),
                    "2A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9".to_owned()
                ),
            ]
        );
        for bad in [&b"{}"[..], b"{\"devices\": []}", b"not JSON"] {
            assert!(devices_in(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// Catches: a name that is not a UDID taken for a device (the set's own
    /// `device_set.plist`, a file the action left), so the end would ask `simctl` to
    /// delete it, or a UDID in lower case missed.
    #[test]
    fn udids_are_8_4_4_4_12_hex_digits() {
        for udid in [
            "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9",
            "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9",
        ] {
            assert!(is_udid(udid), "{udid}");
        }
        for not in [
            "",
            "device_set.plist",
            "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F",
            "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9-0",
            "0A1B2C3D4-E5F-6071-8293-A4B5C6D7E8F9",
            "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8FG",
        ] {
            assert!(!is_udid(not), "{not}");
        }
    }
}
