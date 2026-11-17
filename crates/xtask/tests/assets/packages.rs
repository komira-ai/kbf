//! `verify-assets attestations` on the native packages: provenance and their own bundle.

use sha2::Digest;

use super::fixture::*;

/// A release of one tarball and the kbf-installer deb of the same platform, attested as
/// the attest job leaves them: the deb has build provenance and a bundle, no SBOM.
fn with_deb(name: &str) -> (Release, String) {
    let one = [("linux-x86_64", &["kbf-installer"][..])];
    let r = Release::build_with(name, &one);
    let deb = r.add_deb("linux-x86_64");
    r.attest(None);
    (r, deb)
}

#[test]
fn the_mac_package_is_verified_like_the_deb_for_provenance_and_its_own_bundle() {
    // Catches: a `.pkg` in the macOS SHA256SUMS refused as "neither a tarball, a package
    // nor an SBOM" (verify-darwin-arm64 failing on the first push that lists one), a `.pkg`
    // verified without its bundle, asked for an SBOM attestation it does not have, and the
    // package controls (its bundle verifying the tarball and the reverse) not run for it.
    let one = [("darwin-arm64", &["kbf-installer"][..])];
    let r = Release::build_with("pkg-good", &one);
    let pkg = r.add_package("darwin-arm64", "pkg");
    r.attest(None);
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}: {out}");
    assert!(
        out.lines()
            .any(|l| l == format!("verified: {pkg} (provenance, bundle)")),
        "{out}"
    );
    let calls = r.gh_calls();
    let path = r.dist.join(&pkg);
    let on_pkg: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with(&format!("attestation verify {} ", s(&path))))
        .collect();
    assert_eq!(on_pkg.len(), 4, "{on_pkg:#?}");
    assert!(
        on_pkg.iter().all(|c| !c.contains(SBOM_PREDICATE)),
        "{on_pkg:#?}"
    );
    // Its bundle is required: removed, the listing is refused.
    std::fs::remove_file(r.bundle_of(&pkg)).expect("remove");
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1));
    assert!(
        err.contains(&format!("{pkg}: no provenance bundle")),
        "{err}"
    );
}

#[test]
fn a_package_is_verified_for_provenance_and_its_own_bundle_and_for_no_sbom() {
    // Catches: a deb in SHA256SUMS refused as "neither a tarball nor an SBOM" (the
    // verifier and so the verify-linux job failing on the first push that lists one), a
    // deb verified without its bundle (an unsigned bundle a host would rely on), one
    // asked for an SBOM attestation it does not have, the bundle checked with other
    // flags than the installer's, and the package controls not run (the deb's bundle
    // verifying the tarball, the tarball verifying under the deb's bundle).
    let (r, deb) = with_deb("deb-good");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}: {out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.contains(&format!("verified: {deb} (provenance, bundle)").as_str()),
        "{out}"
    );
    let calls = r.gh_calls();
    let deb_path = r.dist.join(&deb);
    let file = s(&deb_path);
    let verifying = format!("attestation verify {file} ");
    let on_deb: Vec<&String> = calls.iter().filter(|c| c.starts_with(&verifying)).collect();
    // Provenance, its bundle, and the controls: against the tarball's bundle, against
    // its own for another commit. (The tarball against the deb's bundle has the tarball
    // as its file.)
    assert_eq!(on_deb.len(), 4, "{on_deb:#?}");
    assert_eq!(
        on_deb[0],
        &format!("attestation verify {file} {}", flags(WORKFLOW))
    );
    let bundle = r.bundle_of(&deb);
    assert_eq!(
        on_deb[1],
        &format!(
            "attestation verify {file} --bundle {} {}",
            s(&bundle),
            bundle_flags("refs/heads/main", COMMIT)
        )
    );
    assert!(
        on_deb.iter().all(|c| !c.contains(SBOM_PREDICATE)),
        "an SBOM attestation was asked of the deb: {on_deb:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains(&format!(".tar.gz --bundle {} ", s(&bundle)))),
        "the tarball was not tried against the deb's bundle: {calls:#?}"
    );
}

#[test]
fn a_package_without_its_bundle_or_with_anothers_is_refused_before_gh_is_asked() {
    // Catches: the pairing blind to a deb (a package released with no bundle a host can
    // verify it with, a stray bundle of an unlisted deb, or the tarball's bundle copied
    // under the deb's name, which only the subject comparison finds).
    let (r, deb) = with_deb("deb-bundles");
    std::fs::remove_file(r.bundle_of(&deb)).expect("remove");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {deb}: no provenance bundle {deb}.sigstore.json\n")
    );

    let (r, deb) = with_deb("deb-swapped");
    let tarball = format!("kbf-installer-{COMMIT}-linux-x86_64.tar.gz");
    std::fs::copy(r.bundle_of(&tarball), r.bundle_of(&deb)).expect("swap");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        err.starts_with(&format!(
            "xtask: {deb}.sigstore.json: has the subject digest sha256:"
        )),
        "{err}"
    );

    let (r, _) = with_deb("deb-stray");
    let stray = format!("kbf-installer-{COMMIT}-linux-arm64.deb.sigstore.json");
    std::fs::copy(
        r.bundle_of(&format!("kbf-installer-{COMMIT}-linux-x86_64.deb")),
        r.dist.join(&stray),
    )
    .expect("stray");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {stray}: not the bundle of a listed tarball\n")
    );
    assert_eq!(
        std::fs::read_to_string(r.gh.with_file_name("gh-args")).unwrap_or_default(),
        "",
        "gh was run before the pairing was checked"
    );
}

#[test]
fn a_package_bundle_that_does_not_verify_fails_the_run_and_names_it() {
    // Catches: the deb's own bundle never put to gh (only its provenance in GitHub's
    // store), so a bundle signed for another commit, or by no one, would ship.
    let (r, deb) = with_deb("deb-bad-bundle");
    let sha = hex::encode(sha2::Sha256::digest(
        "the kbf-installer package for linux-x86_64",
    ));
    std::fs::write(
        r.bundle_of(&deb),
        bundle_json(&sha, &bundle_flags("refs/heads/main", OTHER)),
    )
    .expect("bundle");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        err.starts_with(&format!(
            "xtask: {}: its bundle {deb}.sigstore.json does not verify: ",
            s(&r.dist.join(&deb))
        )),
        "{err}"
    );
}

#[test]
fn a_verifier_that_mixes_package_and_tarball_bundles_is_caught_by_the_package_controls() {
    // Catches: each package control skipped. With a gh that verifies any file against a
    // bundle, the deb passes the tarball's bundle; with one that takes any file under a
    // deb's bundle, the tarball passes the deb's; with one that ignores the commit, the
    // deb passes for another commit. Each message names the package.
    let (r, deb) = with_deb("deb-controls-file");
    let tarball = format!("kbf-installer-{COMMIT}-linux-x86_64.tar.gz");
    let (code, out, err) = r.verify(WORKFLOW, Some("bundle-any-file"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: control failed: the bundle of {tarball} verified the package {deb}\n")
    );
    let (r, deb) = with_deb("deb-controls-bundle");
    let (code, out, err) = r.verify(WORKFLOW, Some("bundle-deb-bundle-any"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: control failed: the bundle of the package {deb} verified {tarball}\n")
    );
    let (r, deb) = with_deb("deb-controls-commit");
    let (code, out, err) = r.verify(WORKFLOW, Some("bundle-any-commit"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: control failed: the bundle of {deb} verified for commit {}, not the commit it attests\n",
            control_commit(COMMIT)
        )
    );
}
