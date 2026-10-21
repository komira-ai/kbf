//! `deploy --offer` with a bundle that carries a platform's version facts
//! (`kbf-<commit>-<platform>.version.json`) beside the tarballs, as `release stage-offer`
//! makes it. The facts are a file of the release set: the set's sums name them, so the offer
//! check holds them to the sums as it holds a tarball.

use super::*;

/// The name of the facts of `kbf` for linux-x86_64 in the release.
fn facts_name() -> String {
    format!("kbf-{TARGET}-linux-x86_64.version.json")
}

/// The good release with its facts `bytes` added.
fn with_facts(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut list = files();
    list.push((facts_name(), bytes.to_vec()));
    list
}

/// The offer for the release `list`: the good one, for the set of its sums.
fn offer_for(list: &[(String, Vec<u8>)]) -> OfferPolicy {
    let mut fields = Fields::good();
    fields.set_hash = SetHash::of_sums(&sums_of(list)).as_str().to_owned();
    policy(Stage::Gamma, fields.doc())
}

/// Proves a bundle with a platform's version facts is accepted when the sums name the very
/// bytes, and that the facts are uploaded with the tarballs. Catches a facts entry named like
/// a tarball (`version.json-<commit>-<platform>.tar.gz`), which no sums list, so that every
/// bundle `release stage-offer` makes would be refused.
///
/// Mutant it catches: the entry name built from the entry's store name whatever it is.
#[test]
fn a_bundle_with_the_version_facts_the_sums_name_is_accepted() {
    let list = with_facts(b"{\"meta_log\":4}\n");
    let sums = sums_of(&list);
    let (f, s) = deploy_with(
        offer_for(&list),
        Some(bundle_of(&list, Some(&sums))),
        Ok(Some(4)),
    );
    assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
    assert_eq!(f.requests("PUT"), 3);
}

/// Proves version facts that are not the file the sums name, and facts the sums do not list,
/// are refused with the file's name, and nothing is uploaded: the facts are bytes a node's
/// offer is made from, so they are as attested as a tarball.
///
/// Mutants it catches: the facts left out of the digest comparison (as the attestations are);
/// an unlisted facts file allowed.
#[test]
fn version_facts_the_sums_do_not_name_are_refused() {
    let list = with_facts(b"{\"meta_log\":4}\n");
    let sums = sums_of(&list);
    let mut swapped = list.clone();
    swapped[2].1 = b"{\"meta_log\":9}\n".to_vec();
    let (f, s) = deploy_with(
        offer_for(&list),
        Some(bundle_of(&swapped, Some(&sums))),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_file_mismatch");
    assert!(s.detail.contains(&facts_name()), "{}", s.detail);
    assert!(s.detail.contains("not the file"), "{}", s.detail);

    let unlisted = sums_of(&files());
    let (f, s) = deploy_with(
        offer_for(&files()),
        Some(bundle_of(&list, Some(&unlisted))),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_file_mismatch");
    assert!(s.detail.contains(&facts_name()), "{}", s.detail);
    assert!(s.detail.contains("not in the release set"), "{}", s.detail);
}
