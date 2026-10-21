//! The bundle `cargo xtask release stage-offer` makes, read by the installer's own reader
//! and checked by its own offer check (`deploy --offer - --bundle -`): the join of the two
//! crates, which neither crate's tests can see. A name-list check inside xtask cannot catch
//! a bundle the installer refuses (the release's `.sigstore.json` names, an SBOM, no
//! `SHA256SUMS`, version facts the sums do not name), and the installer's tests build their
//! bundles by hand.
//!
//! The offer is read back out of the bundle as a deploy reads it, by `from_bundle` on the
//! reader's `offer.json` entry, never by a parser of this file's own. The stager's bytes for
//! it are pinned by its tests (`crates/xtask/tests/release_stage_offer.rs`); here the offer is
//! a document written beside the release files, put in the bundle by `build_bundle`.

use std::io::Read;
use std::path::Path;

use kbf_installer::CommitId;
use kbf_installer::deploy::bundle::{Bundle, FACTS, read_bundle};
use kbf_installer::deploy::offer::{Probe, ProbeFault, check_bundle, from_bundle, read};
use kbf_testkit::dir::TestDir;
use kbf_types::ReleasePlatform;
use sha2::{Digest, Sha256};
use xtask::release::stage_offer::build_bundle;

const SHA: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
/// The offer, written beside the release files.
const OFFER_DOC: &str = "offer-doc.json";

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The files of a release of `SHA` as `release publish` leaves them, in a directory of its
/// own: for each platform the version facts of its `kbf`, and for each component of it the
/// tarball, SBOM and bundle, and the `SHA256SUMS` over the tarballs, SBOMs and facts. The
/// offer for them is beside, as `OFFER_DOC`.
fn release_dir(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stage-offer-bundle");
    let dir = TestDir::under(root, name);
    let mut lines = Vec::new();
    for platform in ReleasePlatform::ALL {
        let facts = format!("kbf-{SHA}-{platform}.version.json");
        let facts_bytes = format!("the facts of {facts}");
        std::fs::write(dir.join(&facts), &facts_bytes).expect("facts");
        let line = format!("{}  {facts}\n", sha256_hex(facts_bytes.as_bytes()));
        lines.push(line);
        for component in platform.components() {
            let stem = format!("{component}-{SHA}-{platform}");
            let tarball = format!("{stem}.tar.gz");
            let bytes = format!("the bytes of {tarball}");
            std::fs::write(dir.join(&tarball), &bytes).expect("tarball");
            let bundle = dir.join(format!("{tarball}.sigstore.json"));
            std::fs::write(bundle, format!("the bundle of {tarball}")).expect("bundle");
            let sbom = format!("{stem}.cdx.json");
            let sbom_bytes = format!("the SBOM of {tarball}");
            std::fs::write(dir.join(&sbom), &sbom_bytes).expect("sbom");
            let line = format!("{}  {tarball}\n", sha256_hex(bytes.as_bytes()));
            lines.push(line);
            let line = format!("{}  {sbom}\n", sha256_hex(sbom_bytes.as_bytes()));
            lines.push(line);
        }
    }
    std::fs::write(dir.join("SHA256SUMS"), lines.concat()).expect("sums");
    let sums = std::fs::read(dir.join("SHA256SUMS")).expect("sums");
    let set = sha256_hex(&sums);
    let offer = format!(
        "{{\"version\":1,\"created\":1700000000,\"target\":\"gamma\",\"commit\":\"{SHA}\",\
         \"set_hash\":\"{set}\",\
         \"platforms\":[\"darwin-arm64\",\"linux-arm64\",\"linux-x86_64\"],\
         \"beta\":{{\"kind\":\"waived\",\"waiver\":{{\"what\":\"beta\",\"by\":\"canary\",\
         \"reason\":\"canary\"}}}},\"meta_log\":4,\"previous\":null,\
         \"previous_meta_log\":null,\"reversible\":false}}\n"
    );
    std::fs::write(dir.join(OFFER_DOC), offer).expect("offer");
    dir
}

/// The entries `(name, bytes)` of a tar.
fn entries_of(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut archive = tar::Archive::new(bytes);
    let mut out = Vec::new();
    for entry in archive.entries().expect("a tar") {
        let mut entry = entry.expect("an entry");
        let path = entry.path().expect("a name").into_owned();
        let name = path.to_string_lossy().into_owned();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).expect("the content");
        out.push((name, content));
    }
    out
}

/// A tar of `entries`, each a regular file.
fn tar_of(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (name, bytes) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_path(name).expect("a name");
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &bytes[..]).expect("append");
    }
    tar.into_inner().expect("the tar")
}

/// The entries of the bundle the stager makes for the release in `dir`, `offer.json` among
/// them: the offer is the document beside the release files.
fn staged(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let offer = std::fs::read(dir.join(OFFER_DOC)).expect("offer");
    let bundle = build_bundle(dir, SHA, &ReleasePlatform::ALL, &offer).expect("a bundle");
    entries_of(&bundle)
}

/// The bytes of the entry named `name`, to edit.
fn bytes_of<'a>(entries: &'a mut [(String, Vec<u8>)], name: &str) -> &'a mut Vec<u8> {
    let at = entries.iter().position(|(n, _)| n == name);
    &mut entries[at.expect("the entry")].1
}

/// A probe that finds the staged `kbf` to say what the offer says: the join is the bundle's
/// shape, not the run of a binary.
struct Says(Option<u64>);

impl Probe for Says {
    fn meta_log(
        &self,
        _: &CommitId,
        _: &[u8],
        _: Option<&[u8]>,
    ) -> Result<Option<u64>, ProbeFault> {
        Ok(self.0)
    }
}

/// What the installer's reader makes of the tar of `entries`.
fn read_entries(entries: &[(String, Vec<u8>)]) -> Result<Bundle, String> {
    let commit = CommitId::new(SHA).expect("a commit");
    read_bundle(&tar_of(entries)[..], &commit)
}

/// The installer's verdict on the bundle of `entries`, on a host of linux-x86_64: `ok`, or its
/// reader's refusal, or the code and detail of its refusal of the offer it carries or of its
/// offer check. The offer is the bundle's own, as `deploy --offer -` reads it.
fn verdict(entries: &[(String, Vec<u8>)]) -> String {
    let bundle = match read_entries(entries) {
        Ok(bundle) => bundle,
        Err(why) => return format!("reader: {why}"),
    };
    let doc = match from_bundle(bundle.offer.as_deref()) {
        Ok(doc) => doc,
        Err((code, why)) => return format!("{code}: {why}"),
    };
    let commit = CommitId::new(SHA).expect("a commit");
    let probe = Says(doc.meta_log);
    match check_bundle(&doc, &bundle, &commit, "linux-x86_64", &probe) {
        Ok(()) => "ok".to_owned(),
        Err((code, why)) => format!("{code}: {why}"),
    }
}

#[test]
fn the_bundle_the_stager_makes_is_read_and_accepted_by_the_installer() {
    // Catches: a bundle the installer refuses: the release's `.sigstore.json` names kept
    // for the attestations, an SBOM or a bundle in the tar, no `SHA256SUMS` (the offer's set
    // could not be checked), a copy of `SHA256SUMS` that is not the release's, a tarball
    // under another name, a platform or component the installer's store does not take,
    // version facts under a name the reader does not know or that the sums do not list, an
    // `offer.json` the reader does not take or `from_bundle` does not read as the offer
    // (the document written to `out/`).
    // Mutants it catches: the facts left out of the tar (nine entries are read, and every
    // platform has its facts); `offer.json` renamed or left out; the facts named like
    // tarballs.
    let dir = release_dir("accept");
    let entries = staged(&dir);
    assert_eq!(verdict(&entries), "ok");
    let bundle = read_entries(&entries).expect("the installer reads it");
    assert_eq!(bundle.entries.len(), 9);
    let sums = std::fs::read(dir.join("SHA256SUMS")).expect("sums");
    assert_eq!(bundle.sums, Some(sums));
    let from_file = read(&dir.join(OFFER_DOC)).expect("an offer the installer accepts");
    let from_tar = from_bundle(bundle.offer.as_deref()).expect("the bundle's own offer");
    assert_eq!(from_tar, from_file);
    let facts: Vec<&str> = bundle
        .entries
        .iter()
        .filter(|e| e.name == FACTS)
        .map(|e| e.platform.as_str())
        .collect();
    assert_eq!(facts, ["linux-x86_64", "linux-arm64", "darwin-arm64"]);
    let file = format!("kbf-{SHA}-linux-x86_64.version.json");
    let released = std::fs::read(dir.join(file)).expect("the release's facts");
    assert_eq!(bundle.entries[0].bytes, released);
}

#[test]
fn a_bundle_without_the_sums_is_refused_by_the_offer_check() {
    // Catches: the stager leaving SHA256SUMS out: this is the shape it would have then.
    // Mutant it catches: `check_bundle` letting a bundle with no `SHA256SUMS` through (the
    // refusal of a missing set removed). The stager's own omission is the accept test's.
    let dir = release_dir("no-sums");
    let mut entries = staged(&dir);
    entries.retain(|(name, _)| name != "SHA256SUMS");
    let said = verdict(&entries);
    assert!(said.starts_with("offer_set_unverified:"), "{said}");
}

#[test]
fn a_bundle_without_its_offer_is_refused_as_having_none() {
    // Catches: the stager leaving `offer.json` out of the tar, so that `deploy --offer -`
    // has no offer to check; the reader takes the bundle, and `from_bundle` says so.
    // Mutant it catches: `from_bundle(None)` returning an offer. The stager's own omission is
    // the accept test's.
    let dir = release_dir("no-offer");
    let mut entries = staged(&dir);
    entries.retain(|(name, _)| name != "offer.json");
    let said = verdict(&entries);
    assert!(said.starts_with("offer_missing:"), "{said}");
}

#[test]
fn sums_that_are_not_the_offers_set_are_refused_by_the_offer_check() {
    // Catches: a copy of SHA256SUMS that differs from the release's by a byte.
    // Mutant it catches: the comparison of the sums' set hash with the offer's removed from
    // `check_bundle`.
    let dir = release_dir("edited-sums");
    let mut entries = staged(&dir);
    bytes_of(&mut entries, "SHA256SUMS").push(b'\n');
    let said = verdict(&entries);
    assert!(said.starts_with("offer_set_mismatch:"), "{said}");
}

#[test]
fn a_tarball_that_is_not_the_sums_file_is_refused_by_the_offer_check() {
    // Catches: a tarball in the bundle that is not the one the release set names.
    // Mutant it catches: the comparison of each entry's digest with its line of the sums
    // removed from `check_bundle`.
    let dir = release_dir("edited-tarball");
    let mut entries = staged(&dir);
    bytes_of(&mut entries, &format!("kbf-{SHA}-linux-x86_64.tar.gz")).push(b'!');
    let said = verdict(&entries);
    assert!(said.starts_with("offer_file_mismatch:"), "{said}");
}

#[test]
fn version_facts_that_are_not_the_sums_file_are_refused_by_the_offer_check() {
    // Catches: facts in the bundle that are not the release's (the store would take them and
    // make a node's offer from them), and an installer that checks the tarballs and lets the
    // facts through; the refusal names the facts file.
    // Mutant it catches: the installer's offer check passing the facts entries over.
    let dir = release_dir("edited-facts");
    let mut entries = staged(&dir);
    let facts = format!("kbf-{SHA}-linux-arm64.version.json");
    bytes_of(&mut entries, &facts).push(b'!');
    let said = verdict(&entries);
    assert!(said.starts_with("offer_file_mismatch:"), "{said}");
    assert!(said.contains(&facts), "{said}");
}

#[test]
fn an_entry_under_the_releases_own_name_or_an_sbom_is_refused_by_the_reader() {
    // Catches: the attestation left under the release's name for it (`.sigstore.json`) and
    // an SBOM in the tar: the reader takes `.attestation`, tarballs and version facts only.
    // Mutant it catches: the `.attestation` rename skipped (the `expect` below finds none).
    // The reader taking an SBOM entry is caught by the second half.
    let dir = release_dir("odd-names");
    let entries = staged(&dir);
    let mut renamed = entries.clone();
    let name = format!("kbf-{SHA}-linux-x86_64.tar.gz.attestation");
    let at = renamed.iter().position(|(n, _)| *n == name);
    renamed[at.expect("the attestation")].0 = name.replace(".attestation", ".sigstore.json");
    let refusal = "not <name>-<commit>-<platform>.tar.gz";
    let said = verdict(&renamed);
    assert!(said.starts_with("reader:"), "{said}");
    assert!(said.contains(refusal), "{said}");

    let mut with_sbom = entries.clone();
    let sbom = format!("kbf-{SHA}-linux-x86_64.cdx.json");
    with_sbom.push((sbom, b"{}".to_vec()));
    let said = verdict(&with_sbom);
    assert!(said.starts_with("reader:"), "{said}");
    assert!(said.contains(refusal), "{said}");
}
