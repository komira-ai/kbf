//! Which release a version names, the names of a release's files and whose attestation a file
//! may carry.

use crate::fetch::AssetKind;
use crate::release::{
    Origin, PickError, SUMS_NAME, bundle_of, commit_prefix, is_commit, listed_hash, named_asset_on,
};

const COMMIT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn a_version_names_the_commit_digits_after_the_last_plus() {
    for (version, want) in [
        ("0.1.7+gbbbbbbbbbbbb-3", Some("bbbbbbbbbbbb")),
        ("0.1.7+g0123456789ab", Some("0123456789ab")),
        ("0.1.7~feat+g0123456789ab-9", Some("0123456789ab")),
        ("0.1.7+gx+g0123456789ab-9", Some("0123456789ab")),
        ("0.1.7+g0123456789a-3", None),
        ("0.1.7+g0123456789abc-3", None),
        ("0.1.7+g0123456789AB-3", None),
        ("0.1.7+g0123456789ab.1", None),
        ("0.1.7-3", None),
        ("1.2.3-1", None),
        // A deb's version has no `g`: `<semver>+<12 hex>`, with or without `-<run>`.
        ("0.1.0+cb6f107dd94c", Some("cb6f107dd94c")),
        ("0.1.7+0123456789ab-3", Some("0123456789ab")),
        ("0.1.7~feat+0123456789ab-9", Some("0123456789ab")),
        ("0.1.7+g0123456789ab+0123456789cd", Some("0123456789cd")),
        ("0.1.7+0123456789a", None),
        ("0.1.7+0123456789abc", None),
        ("0.1.7+0123456789AB", None),
        ("0.1.7+0123456789ax", None),
        ("0.1.7+gg0123456789ab", None),
        ("0.1.7+0123456789ab.1", None),
        ("0.1.7+", None),
        ("0123456789ab", None),
    ] {
        assert_eq!(commit_prefix(version), want, "{version}");
    }
}

#[test]
fn a_commit_is_forty_lower_case_hex_digits() {
    assert!(is_commit(COMMIT));
    assert!(!is_commit(&COMMIT[1..]));
    assert!(!is_commit(&format!("{}G", &COMMIT[1..])));
    assert!(!is_commit(&COMMIT.to_uppercase()));
    assert_eq!(
        named_asset_on("kbf", "linux-x86_64", "tar.gz", COMMIT),
        format!("kbf-{COMMIT}-linux-x86_64.tar.gz")
    );
    assert_eq!(
        bundle_of(&named_asset_on("kbf", "linux-x86_64", "tar.gz", COMMIT)),
        format!("kbf-{COMMIT}-linux-x86_64.tar.gz.sigstore.json")
    );
}

// ---- the files of a release ----

/// A release file is named for its kind, platform and commit, its bundle is the same name with
/// `.sigstore.json`, and a name gives its commit back only for its own kind and platform.
#[test]
fn a_release_file_has_its_own_name_bundle_and_commit() {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let tarball = AssetKind::Tarball;
    let image = AssetKind::FrontendImage;
    assert_eq!(
        tarball.name_on("kbf", "linux-arm64", commit),
        format!("kbf-{commit}-linux-arm64.tar.gz")
    );
    assert_eq!(
        image.name_on("kbf", "linux-arm64", commit),
        format!("kbf-frontend-image-{commit}-linux-arm64.oci.tar")
    );
    assert_eq!(
        bundle_of(&image.name_on("kbf", "linux-x86_64", commit)),
        format!("kbf-frontend-image-{commit}-linux-x86_64.oci.tar.sigstore.json")
    );
    assert_eq!(
        bundle_of(&tarball.name_on("kbf", "linux-x86_64", commit)),
        format!("kbf-{commit}-linux-x86_64.tar.gz.sigstore.json")
    );

    assert_eq!(
        AssetKind::Deb.name_on("kbf", "linux-arm64", commit),
        format!("kbf-{commit}-linux-arm64.deb")
    );
    for kind in [tarball, image, AssetKind::Deb, AssetKind::Rpm] {
        let name = kind.name_on("kbf", "linux-arm64", commit);
        assert_eq!(
            kind.commit_of_name_on("kbf", "linux-arm64", &name),
            Some(commit)
        );
        assert_eq!(kind.commit_of_name_on("kbf", "linux-x86_64", &name), None);
        assert_eq!(
            kind.commit_of_name_on("kbf", "linux-arm64", &format!("{name}.sigstore.json")),
            None
        );
    }
    // One kind's name is not the other's.
    let image_name = image.name_on("kbf", "linux-arm64", commit);
    assert_eq!(
        tarball.commit_of_name_on("kbf", "linux-arm64", &image_name),
        None
    );
    let tarball_name = tarball.name_on("kbf", "linux-arm64", commit);
    assert_eq!(
        image.commit_of_name_on("kbf", "linux-arm64", &tarball_name),
        None
    );
    // A commit that is not 40 hex digits is none.
    assert_eq!(
        image.commit_of_name_on(
            "kbf",
            "linux-arm64",
            "kbf-frontend-image-abc-linux-arm64.oci.tar"
        ),
        None
    );
}

/// The sha256 a release's sums list for a file is found by the file's whole name; sums that are
/// not lines of `<64 hex>  <name>` (or name a file twice, or a path) are malformed, and a file
/// not listed is unknown.
#[test]
fn the_sums_give_the_sha256_listed_for_one_file() {
    let (a, b) = ("a".repeat(64), "b".repeat(64));
    let sums = format!("{a}  one.tar\n{b}  two.tar\n");
    assert_eq!(SUMS_NAME, "SHA256SUMS");
    assert_eq!(listed_hash(&sums, "two.tar").as_deref(), Ok(b.as_str()));
    assert_eq!(listed_hash(&sums, "one.tar").as_deref(), Ok(a.as_str()));
    assert_eq!(listed_hash(&sums, "two"), Err(PickError::Unknown));
    assert_eq!(listed_hash("", "one.tar"), Err(PickError::Unknown));
    for bad in [
        format!("{a}  one.tar"),
        format!("{a} one.tar\n"),
        format!("{}  one.tar\n", "a".repeat(63)),
        format!("{}  one.tar\n", "A".repeat(64)),
        format!("{a}  \n"),
        format!("{a}  dir/one.tar\n"),
        format!("{a}  one.tar\n{b}  one.tar\n"),
    ] {
        assert_eq!(
            listed_hash(&bad, "one.tar"),
            Err(PickError::Malformed),
            "{bad:?}"
        );
    }
}

/// A channel's artifact prefix names its files and is read back from them: the prefix is the
/// whole of the start of the name, the frontend image keeps its `-frontend-image-` word after
/// it, and the files of another prefix (or of `kbf`) are not this channel's.
#[test]
fn the_artifact_prefix_names_and_recognises_the_channels_files() {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let cases = [
        (AssetKind::Tarball, "other-{c}-linux-arm64.tar.gz"),
        (AssetKind::Deb, "other-{c}-linux-arm64.deb"),
        (AssetKind::Rpm, "other-{c}-linux-arm64.rpm"),
        (
            AssetKind::FrontendImage,
            "other-frontend-image-{c}-linux-arm64.oci.tar",
        ),
    ];
    for (kind, pattern) in cases {
        let name = pattern.replace("{c}", commit);
        assert_eq!(kind.name_on("other", "linux-arm64", commit), name);
        assert_eq!(
            kind.commit_of_name_on("other", "linux-arm64", &name),
            Some(commit)
        );
        assert_eq!(kind.commit_of_name_on("kbf", "linux-arm64", &name), None);
        assert_eq!(kind.commit_of_name_on("oth", "linux-arm64", &name), None);
        let kbfs = kind.name_on("kbf", "linux-arm64", commit);
        assert!(kbfs.starts_with("kbf-"), "{kbfs}");
        assert_eq!(kind.commit_of_name_on("other", "linux-arm64", &kbfs), None);
    }
    // The default is the name of today.
    assert_eq!(
        named_asset_on("kbf", "linux-x86_64", "tar.gz", commit),
        format!("kbf-{commit}-linux-x86_64.tar.gz")
    );
}

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
