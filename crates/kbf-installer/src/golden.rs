//! The golden fixtures of the installer contract (section 9, P1) and the values they decode
//! to. Every later pull request that touches an order record, `state.json`, `status.json` or
//! `installer.toml` reads these: a change to a shape moves the file in the same change, so the
//! diff of `testdata/golden/` is the review of the format.

use crate::CommitId;
use crate::installer_toml::{FrontendPull, InstallerToml, Source};
use crate::record::{
    ActivationId, Component, Expected, OrderRecord, OrderVerb, Sha256Hex, TargetPlatform,
};
use crate::seed::Available;
use crate::state_v2::{Floor, Instance, PendingConfirm, SCHEMA as STATE_SCHEMA, StateV2, Unacked};
use crate::status_v2::{Envoy, Phase, SCHEMA as STATUS_SCHEMA, StagedFacts, StatusV2};
use crate::testkit::{A, B, C, c};

pub const ORDER_ACTIVATE: &str = include_str!("../testdata/golden/order_activate.json");
pub const ORDER_CONFIRM: &str = include_str!("../testdata/golden/order_confirm.json");
pub const ORDER_REVERT: &str = include_str!("../testdata/golden/order_revert.json");
pub const ORDER_FRONTEND_UPDATE: &str =
    include_str!("../testdata/golden/order_frontend_update.json");
pub const ORDER_FRONTEND_UPDATE_STAGED: &str =
    include_str!("../testdata/golden/order_frontend_update_staged.json");
pub const STATE: &str = include_str!("../testdata/golden/state.json");
pub const STATUS: &str = include_str!("../testdata/golden/status.json");
pub const INSTALLER_TOML_URL: &str = include_str!("../testdata/golden/installer.toml");
pub const INSTALLER_TOML_FRONTEND: &str =
    include_str!("../testdata/golden/installer_frontend.toml");
pub const INSTALLER_TOML_BUNDLED: &str = include_str!("../testdata/golden/installer_bundled.toml");
pub const INSTALLER_TOML_FRONTEND_PULL: &str =
    include_str!("../testdata/golden/installer_frontend_pull.toml");
pub const INSTALLER_TOML_URL_CA: &str = include_str!("../testdata/golden/installer_url_ca.toml");

fn commit(id: &str) -> CommitId {
    c(id)
}

pub fn order_activate() -> OrderRecord {
    OrderRecord {
        order_id: 42,
        epoch: 3,
        generation: 17,
        verb: OrderVerb::Activate,
        commit: commit(B),
        window_s: 900,
        components: vec![Component {
            name: "kbf".to_owned(),
            sha256: Sha256Hex::try_from("ab".repeat(32)).expect("a digest"),
            size: 52_428_800,
            attestation: "{\"mediaType\":\"application/vnd.dev.sigstore.bundle.v0.3+json\",\"verificationMaterial\":{},\"messageSignature\":{}}".to_owned(),
        }],
        expected: Some(Expected {
            platform: TargetPlatform::LinuxX86_64,
            protocol: 12,
            min_protocol: 11,
            state_schema: 4,
            profile: "release".to_owned(),
        }),
        source: kbf_update::Source::Fetch,
    }
}

pub fn order_revert() -> OrderRecord {
    OrderRecord {
        order_id: 44,
        verb: OrderVerb::Revert,
        ..order_confirm()
    }
}

/// The frontend's own update: a locator and nothing else, like a `confirm`, a later order.
/// The root half fetches the commit itself; the record names no file, digest or fact.
pub fn order_frontend_update() -> OrderRecord {
    OrderRecord {
        order_id: 45,
        generation: 18,
        verb: OrderVerb::FrontendUpdate,
        ..order_confirm()
    }
}

/// The same update of the frontend, of the release the installer's package carries: the
/// record names its source, which a record that fetches leaves out.
pub fn order_frontend_update_staged() -> OrderRecord {
    OrderRecord {
        source: kbf_update::Source::Staged,
        ..order_frontend_update()
    }
}

pub fn order_confirm() -> OrderRecord {
    OrderRecord {
        order_id: 43,
        epoch: 3,
        generation: 17,
        verb: OrderVerb::Confirm,
        commit: commit(B),
        window_s: 900,
        components: Vec::new(),
        expected: None,
        source: kbf_update::Source::Fetch,
    }
}

pub fn state() -> StateV2 {
    StateV2 {
        schema: STATE_SCHEMA,
        current: Some(commit(A)),
        previous: Some(commit(C)),
        staged: Some(commit(B)),
        pending_confirm: Some(PendingConfirm {
            activation: ActivationId {
                epoch: 3,
                order_id: 42,
            },
            commit: commit(B),
            previous: Some(commit(A)),
            deadline_wall: Some(1_792_000_000),
            deadline_mono_s: Some(86_400),
            starts_left: Some(2),
        }),
        floor: Floor {
            epoch: 3,
            order_id: 42,
            generation: 17,
        },
        serving_instance: Some(Instance::try_from("a".to_owned()).expect("an instance")),
        last_acked_order_id: 41,
        unacked: Some(Unacked {
            order: order_activate(),
            first_seen: 1_791_999_000,
        }),
    }
}

/// The host the golden `status.json` describes: running A with C before it, B staged and
/// refused, a source that fetches nothing and a package that carries a seed of B.
pub fn status() -> StatusV2 {
    StatusV2 {
        schema: STATUS_SCHEMA,
        installed_commit: Some(commit(A)),
        previous_commit: Some(commit(C)),
        staged_commit: Some(commit(B)),
        state: Phase::Refused,
        refusal: "attestation: no valid attestation for kbf at commit 0123456789ab".to_owned(),
        acked_order_id: 41,
        refused_order_id: None,
        staged: Some(StagedFacts {
            protocol: 12,
            min_protocol: 11,
            state_schema: 4,
            profile: Some("release".to_owned()),
        }),
        envoy: Envoy {
            current: None,
            previous: None,
        },
        release_source: Some("bundled".to_owned()),
        available: Some(Available {
            commit: commit(B),
            seq: 812,
        }),
        unverified: Vec::new(),
    }
}

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
