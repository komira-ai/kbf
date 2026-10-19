//! Values the unit tests share: offers and scratch directories.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::{Component, Expected, Offer};

pub(crate) const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
pub(crate) const PLATFORM: &str = "linux-x86_64";

pub(crate) fn component(name: &str) -> Component {
    Component {
        name: name.to_owned(),
        sha256: "ab".repeat(32),
        size: 1000,
        attestation: b"{\"bundle\":1}\n".to_vec(),
    }
}

pub(crate) fn offer() -> Offer {
    Offer {
        generation: 1,
        commit: COMMIT.to_owned(),
        platform: PLATFORM.to_owned(),
        components: vec![component("kbf")],
        expected: Expected::default(),
    }
}

/// The 40-digit commit of generation `generation`: the number in hex, zero padded.
pub(crate) fn commit(generation: u64) -> String {
    format!("{generation:040x}")
}

/// The offer of `generation`, for `commit(generation)`, with every expected fact set.
pub(crate) fn offer_of(generation: u64) -> Offer {
    Offer {
        generation,
        commit: commit(generation),
        expected: Expected {
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
            profile: "release".to_owned(),
        },
        ..offer()
    }
}

/// An empty directory for one test, under the test binary's own directory.
pub(crate) fn scratch(name: &str) -> PathBuf {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-update-unit")
        .join(format!(
            "{name}-{}-{}",
            std::process::id(),
            CALLS.fetch_add(1, Ordering::Relaxed)
        ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}
