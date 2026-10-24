//! Where `setup` puts things, as the contract names them (docs/design/kbf-installer.md
//! section 3). These are the paths of files `setup` itself writes or reads; the installer's
//! tree (releases, links, state, `orders/`) is `layout.rs`'s, the one source of those.

use crate::layout::Platform;

/// kbf's configuration directory (root:root 0755): `/etc/kbf`, `/usr/local/etc/kbf` on a Mac.
pub fn etc_dir(platform: Platform) -> &'static str {
    platform.config_dir()
}
/// kbf's runtime config: mode, the frontend URL, the enrollment token's path.
pub fn kbf_toml(platform: Platform) -> &'static str {
    platform.kbf_toml()
}
/// The installer's settings; `setup` is its only writer.
pub fn installer_toml(platform: Platform) -> &'static str {
    platform.installer_toml()
}
/// The enrollment token `setup` places from `--token-file`.
pub fn enroll_token(platform: Platform) -> &'static str {
    platform.enroll_token()
}
/// The first operator token, written once on a frontend host.
pub fn api_token(platform: Platform) -> &'static str {
    platform.api_token()
}

/// The directory of `kbf-proxy.socket`'s drop-ins (root:root 0755), where systemd reads
/// what overrides the packaged unit.
pub const PROXY_SOCKET_DIR: &str = "/etc/systemd/system/kbf-proxy.socket.d";
/// The drop-in `setup` writes from `[proxy] listen` of `installer.toml`: it is the only file
/// of that name and its only writer.
pub const PROXY_LISTEN_DROPIN: &str = "/etc/systemd/system/kbf-proxy.socket.d/listen.conf";

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
