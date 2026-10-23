//! The golden `installer.toml` fixtures of the installer contract (section 9, P1) and the
//! values they decode to. A change to the shape of `installer.toml` moves the files in the
//! same change, so the diff of `testdata/golden/` is the review of the format.

use crate::installer_toml::{FrontendPull, InstallerToml, Source};
use crate::sha256_hex::Sha256Hex;

pub const INSTALLER_TOML_URL: &str = include_str!("../testdata/golden/installer.toml");
pub const INSTALLER_TOML_FRONTEND: &str =
    include_str!("../testdata/golden/installer_frontend.toml");
pub const INSTALLER_TOML_BUNDLED: &str = include_str!("../testdata/golden/installer_bundled.toml");
pub const INSTALLER_TOML_FRONTEND_PULL: &str =
    include_str!("../testdata/golden/installer_frontend_pull.toml");
pub const INSTALLER_TOML_URL_CA: &str = include_str!("../testdata/golden/installer_url_ca.toml");

pub fn installer_toml_url() -> InstallerToml {
    InstallerToml {
        source: Source::Url("https://releases.example.com/kbf".to_owned()),
        source_refs: vec!["refs/heads/main".to_owned(), "refs/tags/v1".to_owned()],
        confirm_window_s: 600,
        confirm_window_max_s: 1200,
        min_generation: 17,
        min_protocol: 11,
        fetch_timeout_s: 1200,
        max_component_bytes: 268_435_456,
        min_activation_interval_s: 120,
        node_cert: None,
        node_key: None,
        release_ca: None,
        frontend: None,
        proxy: None,
        database: None,
        frontend_config: None,
    }
}

pub fn installer_toml_frontend() -> InstallerToml {
    InstallerToml {
        node_cert: Some("/etc/kbf/node.crt".into()),
        node_key: Some("/etc/kbf/node.key".into()),
        ..InstallerToml::default()
    }
}

/// A host that fetches nothing: the defaults, with the release the package carries.
pub fn installer_toml_bundled() -> InstallerToml {
    InstallerToml {
        source: Source::Bundled,
        ..InstallerToml::default()
    }
}

/// A node that pulls over the LAN: the frontend's node identity and the snapshot of its worker
/// listener and CA.
pub fn installer_toml_frontend_pull() -> InstallerToml {
    InstallerToml {
        frontend: Some(FrontendPull {
            url: "https://frontend.lan:8443".to_owned(),
            ca: "/var/lib/kbf-installer/frontend-ca.pem".into(),
            ca_sha256: Sha256Hex::try_from(
                "d2a84f4b8b650937ec8f73cd8be2c74add5a911ba64df27458ed8229da804a26".to_owned(),
            )
            .expect("a digest"),
        }),
        ..installer_toml_frontend()
    }
}

/// A file server under a private CA.
pub fn installer_toml_url_ca() -> InstallerToml {
    InstallerToml {
        source: Source::Url("https://releases.example.com/kbf".to_owned()),
        release_ca: Some("/var/lib/kbf-installer/release-ca.pem".into()),
        ..InstallerToml::default()
    }
}
