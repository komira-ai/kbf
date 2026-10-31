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

/// The signers of a channel for `prefix` files that names `pair`, as owned strings.
fn signers_of(pair: (&str, &str), prefix: &str) -> Vec<(String, String)> {
    let origin = Origin::new(pair.0, pair.1, prefix).unwrap();
    origin
        .signers()
        .into_iter()
        .map(|(r, w)| (r.to_owned(), w.to_owned()))
        .collect()
}

fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(r, w)| ((*r).to_owned(), (*w).to_owned()))
        .collect()
}

fn has(signers: &[(String, String)], pair: (&str, &str)) -> bool {
    signers.contains(&(pair.0.to_owned(), pair.1.to_owned()))
}

/// A tierstore asset signed by kbf's identity (the last release kbf shipped, a rollback)
/// verifies, under the name kbf had then as well.
#[test]
fn a_tierstore_asset_signed_by_kbf_is_accepted() {
    let signers = signers_of(KBF, "tierstore");
    assert!(has(&signers, KBF), "{signers:?}");
    assert!(has(&signers, FORMER), "{signers:?}");
}

/// A tierstore asset signed by komira-ai/tierstore's artifacts workflow is accepted, whether
/// the channel names kbf's repository or tierstore's.
#[test]
fn a_tierstore_asset_signed_by_the_tierstore_repository_is_accepted() {
    for channel in [KBF, TIERSTORE] {
        let signers = signers_of(channel, "tierstore");
        assert!(has(&signers, TIERSTORE), "{channel:?}: {signers:?}");
        assert!(has(&signers, KBF), "{channel:?}: {signers:?}");
    }
}

/// The exact set for the tierstore asset: nothing else rides along.
#[test]
fn the_tierstore_asset_accepts_exactly_kbf_its_former_name_and_tierstore() {
    assert_eq!(
        signers_of(KBF, "tierstore"),
        owned(&[KBF, FORMER, TIERSTORE])
    );
    assert_eq!(
        signers_of(TIERSTORE, "tierstore"),
        owned(&[TIERSTORE, KBF, FORMER])
    );
}

/// The kbf binary (and every other asset) signed by komira-ai/tierstore is refused: the pair
/// is not in the list for any prefix but `tierstore`.
#[test]
fn another_asset_does_not_accept_the_tierstore_signer() {
    for prefix in ["kbf", "kbf-daemon", "kbf-host-install", "other"] {
        let signers = signers_of(KBF, prefix);
        assert_eq!(signers, owned(&[KBF, FORMER]), "{prefix}");
        assert!(!has(&signers, TIERSTORE), "{prefix}");
    }
    let own = signers_of(TIERSTORE, "kbf");
    assert_eq!(
        own,
        owned(&[TIERSTORE]),
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
        let signers = signers_of(KBF, prefix);
        assert!(!has(&signers, TIERSTORE), "{prefix}: {signers:?}");
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
        let signers = signers_of(pair, "kbf");
        assert_eq!(signers, owned(&[pair]), "{pair:?}");
        let for_tierstore = signers_of(pair, "tierstore");
        if pair.0 == "komira-ai/tierstore" {
            // The repository is the pinned one, so the pinned pairs follow the channel's own
            // (which a channel still needs a `trusted_workflows` entry for).
            assert_eq!(for_tierstore[0], owned(&[pair])[0], "{pair:?}");
            assert_eq!(for_tierstore.len(), 4, "{pair:?}");
        } else {
            assert_eq!(for_tierstore, owned(&[pair]), "{pair:?}");
        }
    }
}

/// A channel for any other trusted repository gets exactly its own pair for the `tierstore`
/// prefix: the pinned pairs are added only for kbf's, its former name's and tierstore's own
/// repository.
#[test]
fn another_trusted_repository_with_the_tierstore_prefix_gets_only_its_own_pair() {
    let other = ("owner/other", "owner/other/.github/workflows/release.yml");
    assert_eq!(signers_of(other, "tierstore"), owned(&[other]));
    for channel in [KBF, FORMER, TIERSTORE] {
        let signers = signers_of(channel, "tierstore");
        assert!(has(&signers, TIERSTORE), "{channel:?}");
        assert!(has(&signers, KBF), "{channel:?}");
        assert!(has(&signers, FORMER), "{channel:?}");
    }
}
