//! Which signers an attestation may name, by asset: `tierstore` files also accept the
//! identity of komira-ai/tierstore, beside kbf's own; every other asset accepts kbf's alone.
//! The identities are written out here, not read from the constants.

use crate::mac::release::Origin;

const KBF: (&str, &str) = (
    "komira-ai/kbf",
    "komira-ai/kbf/.github/workflows/artifacts.yml",
);
const FORMER: (&str, &str) = (
    "komira-ai/komira-build-farm",
    "komira-ai/komira-build-farm/.github/workflows/artifacts.yml",
);
const TIERSTORE: (&str, &str) = (
    "komira-ai/tierstore",
    "komira-ai/tierstore/.github/workflows/artifacts.yml",
);

fn origin(pair: (&str, &str), prefix: &str) -> Origin {
    Origin::new(pair.0, pair.1, prefix).unwrap()
}

/// A tierstore asset signed by kbf's identity (the last release kbf shipped, a rollback)
/// verifies, under the name kbf had then as well.
#[test]
fn a_tierstore_asset_signed_by_kbf_is_accepted() {
    let signers = origin(KBF, "tierstore").signers();
    assert!(signers.contains(&KBF), "{signers:?}");
    assert!(signers.contains(&FORMER), "{signers:?}");
}

/// A tierstore asset signed by komira-ai/tierstore's artifacts workflow is accepted, whether
/// the channel names kbf's repository or tierstore's.
#[test]
fn a_tierstore_asset_signed_by_the_tierstore_repository_is_accepted() {
    for channel in [KBF, TIERSTORE] {
        let signers = origin(channel, "tierstore").signers();
        assert!(signers.contains(&TIERSTORE), "{channel:?}: {signers:?}");
        assert!(signers.contains(&KBF), "{channel:?}: {signers:?}");
    }
}

/// The exact set for the tierstore asset: nothing else rides along.
#[test]
fn the_tierstore_asset_accepts_exactly_kbf_its_former_name_and_tierstore() {
    assert_eq!(origin(KBF, "tierstore").signers(), [KBF, FORMER, TIERSTORE]);
    assert_eq!(
        origin(TIERSTORE, "tierstore").signers(),
        [TIERSTORE, KBF, FORMER]
    );
}

/// The kbf binary (and every other asset) signed by komira-ai/tierstore is refused: the pair
/// is not in the list for any prefix but `tierstore`.
#[test]
fn another_asset_does_not_accept_the_tierstore_signer() {
    for prefix in ["kbf", "kbf-daemon", "kbf-host-install", "other"] {
        let signers = origin(KBF, prefix).signers();
        assert_eq!(signers, [KBF, FORMER], "{prefix}");
        assert!(!signers.contains(&TIERSTORE), "{prefix}");
    }
    let own = origin(TIERSTORE, "kbf");
    assert_eq!(
        own.signers(),
        [TIERSTORE],
        "a tierstore channel for kbf files"
    );
}

/// Prefixes that merely look like `tierstore` are other assets.
#[test]
fn a_lookalike_prefix_is_not_the_tierstore_asset() {
    for prefix in [
        "tierstore-x",
        "tierstor",
        "tierstore-bench",
        "tier",
        "xtierstore",
    ] {
        let signers = origin(KBF, prefix).signers();
        assert!(!signers.contains(&TIERSTORE), "{prefix}: {signers:?}");
    }
}

/// A repository or workflow that is not exactly the pinned pair is not widened to it: the
/// signers are compared whole, so a lookalike repository, a lookalike workflow path or a
/// workflow of the right repository under another name stays itself and gains nothing.
#[test]
fn lookalike_identities_are_not_the_tierstore_signer() {
    for pair in [
        (
            "komira-ai/tierstore-x",
            "komira-ai/tierstore-x/.github/workflows/artifacts.yml",
        ),
        (
            "komira-ai/tierstor",
            "komira-ai/tierstor/.github/workflows/artifacts.yml",
        ),
        (
            "komira-ai/tierstore",
            "komira-ai/tierstore/.github/workflows/ci.yml",
        ),
        (
            "komira-ai/tierstore",
            "komira-ai/tierstore/.github/workflows/artifacts.yaml",
        ),
        (
            "other/tierstore",
            "other/tierstore/.github/workflows/artifacts.yml",
        ),
    ] {
        let signers = origin(pair, "kbf").signers();
        assert_eq!(signers, [pair], "{pair:?}");
        let for_tierstore = origin(pair, "tierstore").signers();
        assert_eq!(for_tierstore[0], pair, "{pair:?}");
        // The lookalike stays a distinct entry beside the pinned pair; it is never merged
        // into it, and the pinned pair's own strings are exact.
        assert_eq!(
            for_tierstore.iter().filter(|p| **p == TIERSTORE).count(),
            1,
            "{pair:?}"
        );
    }
}
