//! Where `setup` puts things, as the contract names them (docs/design/kbf-installer.md
//! section 3). These are the paths `setup` itself names; the installer's tree and the configuration
//! directory (`kbf.toml`, `installer.toml`, the tokens) are `layout.rs`'s, the one source of those.

use crate::layout::Platform;

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

/// What the retired `kbf-helper` and `kbf-deploy host` left on a host: their release tree and
/// state. `kbf-installer` and they never both manage one host (contract section 8), and the
/// migration is not built, so `setup` stops where either is there.
pub const OLD_LAYOUT: [&str; 2] = ["/usr/local/kbf", "/var/lib/kbf-deploy"];
