use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::*;
use crate::fetch::Sha256;

const TARBALL: &str = "kbf-0123456789abcdef0123456789abcdef01234567-linux-x86_64.tar.gz";
const SBOM: &str = "kbf-0123456789abcdef0123456789abcdef01234567-linux-x86_64.cdx.json";

/// The digest the tarball of a test is listed with.
fn digest() -> Sha256 {
    Sha256::of(TARBALL.as_bytes())
}

/// A bundle file: one JSON line of the shape of a Sigstore bundle, whose DSSE envelope
/// holds `payload` as it is (or none when it is `null`).
fn bundle_with_payload(payload: serde_json::Value) -> String {
    let mut envelope = serde_json::json!({
        "payloadType": "application/vnd.in-toto+json",
        "signatures": [],
    });
    if !payload.is_null() {
        envelope["payload"] = payload;
    }
    let bundle = serde_json::json!({
        "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
        "verificationMaterial": {},
        "dsseEnvelope": envelope,
    });
    format!("{bundle}\n")
}

/// A bundle file whose payload is the in-toto statement of an attestation of
/// `predicate_type` over the files with the SHA-256 digests `subjects`.
fn bundle_with(subjects: &[&str], predicate_type: &str) -> String {
    let subject: Vec<serde_json::Value> = subjects
        .iter()
        .map(|sha| serde_json::json!({ "name": "asset", "digest": { "sha256": sha } }))
        .collect();
    let statement = serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": subject,
        "predicateType": predicate_type,
        "predicate": {},
    });
    bundle_with_payload(serde_json::Value::String(
        STANDARD.encode(statement.to_string()),
    ))
}

/// The bundle file of the tarball with the digest `sha`: one provenance attestation
/// with that one subject.
fn good(sha: &Sha256) -> String {
    bundle_with(&[&sha.to_string()], PROVENANCE)
}

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-bundles-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn line(name: &str) -> Line {
    Line {
        sha256: Sha256::of(name.as_bytes()),
        name: name.to_owned(),
    }
}

#[test]
fn a_bundle_is_named_after_its_tarball_and_only_the_suffix_marks_one() {
    // Catches: a bundle name that is not the tarball's name and the suffix (the
    // pairing, the release assets and the docs all rest on it), and a bundle test that
    // takes any `.json` file for one (so `verify-assets sums` would stop refusing an
    // unlisted `notes.json`) or misses the bare suffix.
    assert_eq!(name_of(TARBALL), format!("{TARBALL}.sigstore.json"));
    assert!(is_bundle(&name_of(TARBALL)));
    assert!(is_bundle(".sigstore.json"));
    for not in [
        SBOM,
        TARBALL,
        "notes.json",
        "sigstore.json",
        "a.sigstore.jsonl",
    ] {
        assert!(!is_bundle(not), "{not}");
    }
}

#[test]
fn the_control_commit_differs_in_its_last_digit_alone_and_is_still_a_commit() {
    // Catches: a control commit equal to the real one (the control would pass a
    // verification that does not compare the commit), one that changes more than the
    // last digit (a gh that compared only a prefix would be missed), or one that is no
    // longer 40 lowercase hex digits (gh would refuse it for the wrong reason).
    for last in ['0', '1', '9', 'a', 'f'] {
        let commit = format!("0123456789abcdef0123456789abcdef0123456{last}");
        let other = other_commit(&commit);
        assert_ne!(other, commit);
        assert_eq!(other.len(), 40);
        assert_eq!(other[..39], commit[..39]);
        assert!(crate::build_commit::is_commit(&other), "{other}");
    }
}

#[test]
fn one_json_line_of_a_sigstore_bundles_fields_is_the_shape_of_a_bundle() {
    // Control: catches a check that refuses a correct bundle, with or without the final
    // newline, or with blank lines around it (a file `gh` reads as it is).
    let text = good(&digest());
    for text in [
        text.trim_end().to_owned(),
        format!("{text}\n"),
        format!("\n{text}\n\n"),
    ] {
        assert_eq!(check(text.as_bytes(), &digest()), Vec::<String>::new());
    }
}

#[test]
fn the_shape_names_every_way_a_file_is_not_one_bundle() {
    // Catches: each defect accepted as "a bundle" because only the name was looked at.
    let text = good(&digest());
    let cases: [(&str, Vec<u8>, &str); 6] = [
        ("not utf-8", vec![0xff, 0xfe], "is not UTF-8"),
        ("empty", Vec::new(), "holds 0 lines"),
        (
            "two",
            format!("{text}\n{text}\n").into_bytes(),
            "holds 2 lines",
        ),
        ("text", b"gh: no attestations\n".to_vec(), "is not JSON"),
        ("scalar", b"\"bundle\"\n".to_vec(), "is not a JSON object"),
        (
            "fields",
            b"{\"mediaType\":\"text/plain\"}\n".to_vec(),
            "has no `mediaType`",
        ),
    ];
    for (what, bytes, want) in cases {
        let got = check(&bytes, &digest());
        assert!(
            got.iter().any(|p| p.contains(want)),
            "{what}: wanted {want:?}, got {got:?}"
        );
    }
    // The envelope and the verification material are both named when both are gone.
    let got = check(
        b"{\"mediaType\":\"application/vnd.dev.sigstore.bundle+json\"}\n",
        &digest(),
    );
    assert_eq!(
        got,
        vec![
            "has no `dsseEnvelope` object".to_owned(),
            "has no `verificationMaterial` object".to_owned()
        ]
    );
    // The size cap is on the file, whatever is in it.
    let big = vec![b' '; MAX_BYTES + 1];
    assert!(
        check(&big, &digest())[0]
            .starts_with(&format!("is {} bytes, over the {MAX_BYTES}", MAX_BYTES + 1))
    );
    let pad = MAX_BYTES - text.trim_end().len();
    let at_cap = format!("{}{}", text.trim_end(), " ".repeat(pad));
    assert_eq!(at_cap.len(), MAX_BYTES);
    assert_eq!(check(at_cap.as_bytes(), &digest()), Vec::<String>::new());
}

#[test]
fn the_subject_is_the_one_digest_of_the_tarball_and_the_statement_is_provenance() {
    // Catches: a bundle taken on its name and shape. A bundle of another file (the
    // swap the contract's digest comparison is for), one with two subjects or none (not
    // the tarball's own), a digest spelled another way, an SBOM attestation, and a
    // payload that is missing, not base64 or not a statement; each named.
    let sha = digest().to_string();
    let other = Sha256::of(b"another file").to_string();
    let wrong = format!("has the subject digest sha256:{other}, not the tarball's sha256:{sha}");
    let sbom = "https://cyclonedx.org/bom";
    let cases = [
        (
            "another file",
            bundle_with(&[&other], PROVENANCE),
            wrong.clone(),
        ),
        (
            "upper case",
            bundle_with(&[&sha.to_uppercase()], PROVENANCE),
            format!(
                "has the subject digest sha256:{}, not the tarball's sha256:{sha}",
                sha.to_uppercase()
            ),
        ),
        (
            "two subjects",
            bundle_with(&[&sha, &other], PROVENANCE),
            "names 2 subjects, not exactly one".to_owned(),
        ),
        (
            "no subject",
            bundle_with(&[], PROVENANCE),
            "names 0 subjects, not exactly one".to_owned(),
        ),
        (
            "an SBOM",
            bundle_with(&[&sha], sbom),
            format!(
                "has a payload of predicate type {sbom:?}, not build provenance ({PROVENANCE})"
            ),
        ),
        (
            "no payload",
            bundle_with_payload(serde_json::Value::Null),
            "has no `dsseEnvelope.payload` text".to_owned(),
        ),
        (
            "a payload that is not text",
            bundle_with_payload(serde_json::json!(7)),
            "has no `dsseEnvelope.payload` text".to_owned(),
        ),
        (
            "not base64",
            bundle_with_payload(serde_json::json!("not base64!")),
            "has a `dsseEnvelope.payload` that is not base64".to_owned(),
        ),
        (
            "not a statement",
            bundle_with_payload(serde_json::Value::String(STANDARD.encode("hello"))),
            "has a payload that is not a JSON statement".to_owned(),
        ),
        (
            "a statement without subjects",
            bundle_with_payload(serde_json::Value::String(STANDARD.encode("{}"))),
            "names 0 subjects, not exactly one".to_owned(),
        ),
    ];
    // Every case is tried and the ones not named reported together, so a check that
    // stops naming one shows which and not only the first.
    let missed: Vec<String> = cases
        .iter()
        .filter_map(|(what, text, want)| {
            let got = check(text.as_bytes(), &digest());
            (!got.iter().any(|p| p == want))
                .then(|| format!("{what}: wanted {want:?}, got {got:?}"))
        })
        .collect();
    assert!(
        missed.is_empty(),
        "{} not named:\n{missed:#?}",
        missed.len()
    );
    // A subject with no sha256 digest says so (the digest of another algorithm is not
    // the tarball's).
    let statement = serde_json::json!({
        "subject": [{ "digest": { "sha512": "00" } }],
        "predicateType": PROVENANCE,
    });
    let text = bundle_with_payload(serde_json::Value::String(
        STANDARD.encode(statement.to_string()),
    ));
    assert_eq!(
        check(text.as_bytes(), &digest()),
        vec![format!(
            "has the subject digest sha256:(none), not the tarball's sha256:{sha}"
        )]
    );
    // Both the kind and the subject are named when both are wrong.
    let got = check(bundle_with(&[&other], sbom).as_bytes(), &digest());
    assert_eq!(got.len(), 2, "{got:?}");
    // A shape problem is reported alone: the subject of a file that is not a bundle is
    // not a second finding.
    let got = check(b"{\"mediaType\":\"text/plain\"}\n", &digest());
    assert!(got.iter().all(|p| !p.contains("subject")), "{got:?}");
}

#[test]
fn the_pairing_is_one_bundle_per_listed_tarball_and_nothing_else() {
    // Catches: a missing bundle passed (count of tarballs and bundles compared instead
    // of names), a stray one passed (only the tarballs' side looked at), a bundle of
    // the SBOM taken for a tarball's, a symlink standing in for a file, and a bundle
    // that is another listed file's (names equal, subjects not).
    let dir = scratch("pairing");
    let lines = [line(SBOM), line(TARBALL)];
    let bundle = dir.join(name_of(TARBALL));
    assert_eq!(
        pairing(&dir, &lines).expect("pairing"),
        vec![format!(
            "{TARBALL}: no provenance bundle {}",
            name_of(TARBALL)
        )]
    );
    std::fs::write(&bundle, good(&digest())).expect("bundle");
    assert_eq!(
        pairing(&dir, &lines).expect("pairing"),
        Vec::<String>::new()
    );
    // A second bundle, for the SBOM: left over, not a tarball's.
    let stray = name_of(SBOM);
    std::fs::write(dir.join(&stray), good(&line(SBOM).sha256)).expect("stray");
    assert_eq!(
        pairing(&dir, &lines).expect("pairing"),
        vec![format!("{stray}: not the bundle of a listed tarball")]
    );
    std::fs::remove_file(dir.join(&stray)).expect("remove");
    // The tarball's name, the SBOM's bundle: a whole bundle, of another file.
    std::fs::write(&bundle, good(&line(SBOM).sha256)).expect("bundle");
    assert_eq!(
        pairing(&dir, &lines).expect("pairing"),
        vec![format!(
            "{}: has the subject digest sha256:{}, not the tarball's sha256:{}",
            name_of(TARBALL),
            line(SBOM).sha256,
            digest()
        )]
    );
    // The right name, a link to a bundle: not a regular file, so the bundle is missing.
    std::fs::remove_file(&bundle).expect("remove");
    let target = dir.join("elsewhere");
    std::fs::write(&target, good(&digest())).expect("target");
    std::os::unix::fs::symlink(&target, &bundle).expect("link");
    assert_eq!(
        pairing(&dir, &lines).expect("pairing"),
        vec![format!(
            "{TARBALL}: no provenance bundle {}",
            name_of(TARBALL)
        )]
    );
}
