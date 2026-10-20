//! The bundle `cargo xtask release stage-offer` makes, read by the installer's own reader
//! and checked by its own offer check (`deploy --offer - --bundle -`): the join of the two
//! crates, which neither crate's tests can see. A name-list check inside xtask cannot catch
//! a bundle the installer refuses (the release's `.sigstore.json` names, an SBOM, no
//! `SHA256SUMS`), and the installer's tests build their bundles by hand.
//!
//! The reader of the installer this tree is built from takes no `offer.json` entry (the
//! host side that does lands with its own change), so each case leaves that entry out of
//! the tar and gives the check the offer as the document it is. The entry is the stager's
//! own and is pinned by its tests (`crates/xtask/tests/release_stage_offer.rs`).

use std::io::Read;
use std::path::Path;

use kbf_installer::CommitId;
use kbf_installer::deploy::bundle::{Bundle, read_bundle};
use kbf_installer::deploy::offer::{Probe, ProbeFault, check_bundle, read};
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
/// own: for each component of each platform its tarball, SBOM and bundle, and the
/// `SHA256SUMS` over the tarballs and SBOMs. The offer for them is beside, as `OFFER_DOC`.
fn release_dir(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stage-offer-bundle");
    let dir = TestDir::under(root, name);
    let mut lines = Vec::new();
    for platform in ReleasePlatform::ALL {
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

/// The entries of the bundle the stager makes for the release in `dir`, less `offer.json`.
fn staged(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let offer = std::fs::read(dir.join(OFFER_DOC)).expect("offer");
    let bundle = build_bundle(dir, SHA, &ReleasePlatform::ALL, &offer).expect("a bundle");
    let mut entries = entries_of(&bundle);
    assert_eq!(entries[1].0, "offer.json");
    entries.remove(1);
    entries
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

/// The installer's verdict on the bundle of `entries` for the offer in `dir`, on a host of
/// linux-x86_64: `ok`, or its reader's refusal, or the code and detail of its offer check.
fn verdict(dir: &Path, entries: &[(String, Vec<u8>)]) -> String {
    let doc = read(&dir.join(OFFER_DOC)).expect("an offer the installer accepts");
    let bundle = match read_entries(entries) {
        Ok(bundle) => bundle,
        Err(why) => return format!("reader: {why}"),
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
    // under another name, or a platform or component the installer's store does not take.
    let dir = release_dir("accept");
    let entries = staged(&dir);
    assert_eq!(verdict(&dir, &entries), "ok");
    let bundle = read_entries(&entries).expect("the installer reads it");
    assert_eq!(bundle.entries.len(), 6);
    let sums = std::fs::read(dir.join("SHA256SUMS")).expect("sums");
    assert_eq!(bundle.sums, Some(sums));
}

#[test]
fn a_bundle_without_the_sums_is_refused_by_the_offer_check() {
    // Catches: the stager leaving SHA256SUMS out: this is the shape it would have then.
    let dir = release_dir("no-sums");
    let mut entries = staged(&dir);
    entries.retain(|(name, _)| name != "SHA256SUMS");
    let said = verdict(&dir, &entries);
    assert!(said.starts_with("offer_set_unverified:"), "{said}");
}

#[test]
fn sums_that_are_not_the_offers_set_are_refused_by_the_offer_check() {
    // Catches: a copy of SHA256SUMS that differs from the release's by a byte.
    let dir = release_dir("edited-sums");
    let mut entries = staged(&dir);
    entries[0].1.push(b'\n');
    let said = verdict(&dir, &entries);
    assert!(said.starts_with("offer_set_mismatch:"), "{said}");
}

#[test]
fn a_tarball_that_is_not_the_sums_file_is_refused_by_the_offer_check() {
    // Catches: a tarball in the bundle that is not the one the release set names.
    let dir = release_dir("edited-tarball");
    let mut entries = staged(&dir);
    entries[1].1.push(b'!');
    let said = verdict(&dir, &entries);
    assert!(said.starts_with("offer_file_mismatch:"), "{said}");
}

#[test]
fn an_entry_under_the_releases_own_name_or_an_sbom_is_refused_by_the_reader() {
    // Catches: the attestation left under the release's name for it (`.sigstore.json`) and
    // an SBOM in the tar: the reader takes `.attestation` and tarballs only.
    let dir = release_dir("odd-names");
    let entries = staged(&dir);
    let mut renamed = entries.clone();
    let name = format!("kbf-{SHA}-linux-x86_64.tar.gz.attestation");
    let at = renamed.iter().position(|(n, _)| *n == name);
    renamed[at.expect("the attestation")].0 = name.replace(".attestation", ".sigstore.json");
    let refusal = "not <name>-<commit>-<platform>.tar.gz";
    let said = verdict(&dir, &renamed);
    assert!(said.starts_with("reader:"), "{said}");
    assert!(said.contains(refusal), "{said}");

    let mut with_sbom = entries.clone();
    let sbom = format!("kbf-{SHA}-linux-x86_64.cdx.json");
    with_sbom.push((sbom, b"{}".to_vec()));
    let said = verdict(&dir, &with_sbom);
    assert!(said.starts_with("reader:"), "{said}");
    assert!(said.contains(refusal), "{said}");
}
