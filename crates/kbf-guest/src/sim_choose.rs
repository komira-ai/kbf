//! The simulator probe's choices from `simctl` output: which runtime, and which device type.
//!
//! Pure functions of the JSON text, so they run in tests on any OS. [`crate::sim`] calls them
//! as the listing steps finish and re-exports them.

use serde_json::Value;

use crate::sim::State;

const RUNTIME_ID_PREFIX: &str = "com.apple.CoreSimulator.SimRuntime.iOS-";

/// The version of a runtime as a comparable list of numbers (`"27.0"` -> `[27, 0]`).
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Picks the iOS runtime to create the device with: the newest available one.
///
/// # Errors
/// The text is not `simctl list runtimes -j` output or names no available iOS runtime.
pub fn choose_runtime(list: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(list).map_err(|e| format!("not JSON: {e}"))?;
    let runtimes = v["runtimes"].as_array().ok_or("no `runtimes` array")?;
    runtimes
        .iter()
        .filter(|r| {
            r["identifier"]
                .as_str()
                .is_some_and(|i| i.starts_with(RUNTIME_ID_PREFIX))
                && r["isAvailable"].as_bool().unwrap_or(false)
        })
        .max_by_key(|r| version_key(r["version"].as_str().unwrap_or("")))
        .and_then(|r| r["identifier"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "no available iOS runtime among {} listed (was a runtime staged and added?)",
                runtimes.len()
            )
        })
}

/// Picks the device type: the last one listed whose name starts with `iPhone` (the list
/// is oldest first).
///
/// # Errors
/// The text is not `simctl list devicetypes -j` output or lists no iPhone.
pub fn choose_device_type(list: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(list).map_err(|e| format!("not JSON: {e}"))?;
    let types = v["devicetypes"]
        .as_array()
        .ok_or("no `devicetypes` array")?;
    types
        .iter()
        .rev()
        .find(|t| t["name"].as_str().is_some_and(|n| n.starts_with("iPhone")))
        .and_then(|t| t["identifier"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("no iPhone among {} device types", types.len()))
}

/// What `simctl list runtimes -j` says about the device types a runtime can create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeDeviceType {
    /// The last iPhone in the runtime's `supportedDeviceTypes`.
    Chosen(String),
    /// The runtime lists no `supportedDeviceTypes` array (older `simctl`): the caller falls
    /// back to the `list devicetypes` rule, which cannot know what the runtime supports.
    NotListed,
}

/// Picks the device type for `runtime` from that runtime's own `supportedDeviceTypes` in
/// `simctl list runtimes -j` output: the last entry whose `productFamily` is `iPhone` (the
/// list is oldest first, so that is the newest iPhone the runtime can create). An entry with
/// no `productFamily` counts as an iPhone when its name starts with `iPhone`.
///
/// `list devicetypes` lists every device type Xcode knows, including ones the new runtime
/// cannot create (its last iPhone was an iPhone 6s Plus, which iOS 27 refuses), hence this.
///
/// # Errors
/// The text is not `simctl list runtimes -j` output, does not list `runtime`, or its
/// `supportedDeviceTypes` holds no iPhone (the error names the runtime).
pub fn choose_device_type_for_runtime(
    list: &str,
    runtime: &str,
) -> Result<RuntimeDeviceType, String> {
    let v: Value = serde_json::from_str(list).map_err(|e| format!("not JSON: {e}"))?;
    let runtimes = v["runtimes"].as_array().ok_or("no `runtimes` array")?;
    let entry = runtimes
        .iter()
        .find(|r| r["identifier"].as_str() == Some(runtime))
        .ok_or_else(|| format!("runtime {runtime} is not in the runtimes listing"))?;
    let Some(supported) = entry["supportedDeviceTypes"].as_array() else {
        return Ok(RuntimeDeviceType::NotListed);
    };
    let is_iphone = |t: &&Value| match t["productFamily"].as_str() {
        Some(family) => family == "iPhone",
        None => t["name"].as_str().is_some_and(|n| n.starts_with("iPhone")),
    };
    supported
        .iter()
        .rev()
        .find(is_iphone)
        .and_then(|t| t["identifier"].as_str())
        .map(|id| RuntimeDeviceType::Chosen(id.to_owned()))
        .ok_or_else(|| {
            format!(
                "runtime {runtime} supports no iPhone among its {} supported device types",
                supported.len()
            )
        })
}

/// Takes the runtime and, from its `supportedDeviceTypes`, the device type. A runtime that
/// lists none leaves the device type to the `list-devicetypes` step.
pub(crate) fn take_runtime(text: &str, state: &mut State) -> Result<String, String> {
    let runtime = choose_runtime(text)?;
    state.runtime = Some(runtime.clone());
    match choose_device_type_for_runtime(text, &runtime)? {
        RuntimeDeviceType::Chosen(device) => {
            state.device_type = Some(device.clone());
            Ok(format!(
                "{runtime} (the newest available iOS runtime) and device type {device} (the last iPhone in its supportedDeviceTypes)"
            ))
        }
        RuntimeDeviceType::NotListed => Ok(format!(
            "{runtime} (the newest available iOS runtime); it lists no supportedDeviceTypes, so the device type comes from list-devicetypes"
        )),
    }
}

/// Takes the device type from `list devicetypes` unless the runtime's own list already gave
/// one, which is the only list that knows what the runtime can create.
pub(crate) fn take_fallback_device_type(text: &str, state: &mut State) -> Result<String, String> {
    if let Some(chosen) = &state.device_type {
        return Ok(format!(
            "the device type {chosen} from the runtime's supportedDeviceTypes (the devicetypes listing was not used)"
        ));
    }
    let device = choose_device_type(text)?;
    state.device_type = Some(device.clone());
    Ok(format!(
        "{device} (the last iPhone listed; the runtime gave no supportedDeviceTypes, so this may not be one it supports)"
    ))
}
