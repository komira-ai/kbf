//! Where `setup` puts things, as the contract names them (docs/design/kbf-installer.md
//! section 3). These are the paths of files `setup` itself writes or reads; the release tree
//! and the state file are `layout.rs`'s.

use crate::layout::Platform;

/// kbf's configuration directory (root:root 0755).
pub const ETC_DIR: &str = "/etc/kbf";
/// kbf's runtime config: mode, the frontend URL, the enrollment token's path.
pub const KBF_TOML: &str = "/etc/kbf/kbf.toml";
/// The installer's settings; `setup` is its only writer.
pub const INSTALLER_TOML: &str = "/etc/kbf/installer.toml";
/// The enrollment token `setup` places from `--token-file`.
pub const ENROLL_TOKEN: &str = "/etc/kbf/enroll.token";
/// The first operator token, written once on a frontend or all-in-one host.
pub const API_TOKEN: &str = "/etc/kbf/api-token";

/// The installer tree: root's; only `orders/` inside it is the service user's.
pub fn installer_dir(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "/var/lib/kbf-installer",
        Platform::MacOs => "/var/db/kbf-installer",
    }
}

/// `orders/` in the installer tree: where `kbf` writes order records.
pub fn orders_dir(platform: Platform) -> String {
    format!("{}/orders", installer_dir(platform))
}

/// The link to the release that runs. `setup` only asks whether it is there.
pub fn current_release(platform: Platform) -> String {
    format!("{}/releases/current", installer_dir(platform))
}

/// kbf's own state directory, where enrollment keeps the node's certificates (`pki/`).
pub fn kbf_state_dir(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => "/var/lib/kbf",
        Platform::MacOs => "/var/db/kbf",
    }
}

/// Where enrollment writes the certificate and key of a role (`daemon` or `storage`).
pub fn node_identity(platform: Platform, role: &str) -> (String, String) {
    let pki = format!("{}/pki", kbf_state_dir(platform));
    (format!("{pki}/{role}.pem"), format!("{pki}/{role}.key"))
}

/// Where the Envoy that fronts a frontend is installed.
pub const ENVOY: &str = "/usr/bin/envoy";
