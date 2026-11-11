//! The targets `release desired` hands to hostkeeper's `set-desired`: for each platform one
//! entry of the installer `apt`, naming the host's repository entry of the stable suite, the
//! package version, the SHA-256 of the `.deb` and the hook manifest of kbf.
//!
//! hostkeeper's native entries carry the manifest in the call, not in the package. The entry is
//! the proto3 canonical JSON of hostkeeper's `Target` (lowerCamel names, 64-bit integers as JSON
//! strings), the form its coordinator writes.
//!
//! The hook's command, unit, params and bounds are `kbf-package-hook`'s own constants, the
//! hook the package installs.

use serde_json::{Map, Value, json};

use crate::package::Platform;

/// The name of the host's repository entry of the stable suite: the file stem of
/// `/etc/hostkeeper/repositories/komira-stable.sources`.
pub const REPOSITORY: &str = "komira-stable";

/// The oldest hostkeeper that reads the entry: the first release whose daemon runs a native
/// entry's `hook.command` from the sealed tree of the package.
pub const MIN_HOSTKEEPER: &str = "0.1.225";

// The hook's command, the unit, its params and its bounds: one definition, the hook's own, so
// the manifest names exactly what the package's `kbf package-hook` reads.
pub use kbf_package_hook::{
    CLEANUP_RETRIES, CLEANUP_SECS, CUTOVER_SECS, HOOK_COMMAND, MODE, PACKAGE, PREPARE_SECS,
    ROLLBACK_SECS, RUN_UNIT, STATE_DIR, VERIFY_DEADLINE_SECS, VERIFY_SECS,
};

/// The bounds of the installer's steps, in seconds: staging the package, activating it and
/// removing it.
pub const INSTALLER_BOUNDS: [(&str, u64); 3] = [
    ("stageSecs", 600),
    ("activateSecs", 300),
    ("removeSecs", 300),
];

/// The hook's `params`, all text: exactly the keys the hook requires.
pub const PARAMS: [(&str, &str); 3] =
    [("mode", MODE), ("state_dir", STATE_DIR), ("unit", RUN_UNIT)];

/// A whole number as proto3 JSON writes a 64-bit one: a string.
fn int64(n: u64) -> Value {
    Value::String(n.to_string())
}

/// The `params` table of the manifest.
#[must_use]
pub fn params() -> Value {
    let table: Map<String, Value> = PARAMS
        .iter()
        .map(|(key, text)| ((*key).to_owned(), json!({ "text": text })))
        .collect();
    Value::Object(table)
}

/// The hook manifest of kbf.
#[must_use]
pub fn manifest() -> Value {
    json!({
        "package": PACKAGE,
        "hook": {
            "command": HOOK_COMMAND,
            "timeoutSecs": {
                "prepare": int64(PREPARE_SECS),
                "cutover": int64(CUTOVER_SECS),
                "verify": int64(VERIFY_SECS),
                "rollback": int64(ROLLBACK_SECS),
                "cleanup": int64(CLEANUP_SECS),
            },
            "verifyDeadlineSecs": int64(VERIFY_DEADLINE_SECS),
            "cleanupRetries": CLEANUP_RETRIES,
        },
        "run": [RUN_UNIT],
        "params": params(),
        "minHostkeeper": MIN_HOSTKEEPER,
        "installerBounds": INSTALLER_BOUNDS
            .iter()
            .map(|(name, secs)| ((*name).to_owned(), int64(*secs)))
            .collect::<Map<String, Value>>(),
    })
}

/// The entry of the installer `apt` for the `.deb` of `version` whose SHA-256 is `digest`.
#[must_use]
pub fn entry(version: &str, digest: &str) -> Value {
    json!({
        "installer": "apt",
        "repository": REPOSITORY,
        "packageVersion": version,
        "digest": digest,
        "manifest": manifest(),
    })
}

/// The targets: an object keyed by platform, each with its one entry, for the `.deb` digests
/// `digests` (`(platform, sha256)`).
#[must_use]
pub fn targets(version: &str, digests: &[(Platform, String)]) -> Value {
    let by_platform: Map<String, Value> = digests
        .iter()
        .map(|(platform, digest)| {
            (
                platform.name().to_owned(),
                json!({ "entries": [entry(version, digest)] }),
            )
        })
        .collect();
    Value::Object(by_platform)
}
