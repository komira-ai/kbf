use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;
use crate::pkg_image_asset::Image;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-pkg-image-sbom-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A plain tar of `files` (path, bytes).
fn tar_of(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut h = tar::Header::new_ustar();
        h.set_path(path).expect("path");
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append(&h, bytes.as_slice()).expect("append");
    }
    tar.into_inner().expect("tar")
}

/// The digest of an image layout made by [`image`] with `seed` in its config.
fn manifest_of(seed: &str) -> String {
    let (_, manifest) = layout_of(seed);
    manifest
}

/// The tar of an image layout (an index, a manifest and a config, no layers) and the digest of its
/// manifest. `seed` makes the config, and so the manifest, differ.
fn layout_of(seed: &str) -> (Vec<u8>, String) {
    let config = json!({"os": "linux", "architecture": "amd64", "seed": seed})
        .to_string()
        .into_bytes();
    let config_digest = format!("sha256:{}", Sha256::of(&config));
    let manifest = json!({
        "schemaVersion": 2,
        "config": {"digest": config_digest},
        "layers": [],
    })
    .to_string()
    .into_bytes();
    let manifest_digest = format!("sha256:{}", Sha256::of(&manifest));
    let index = json!({"schemaVersion": 2, "manifests": [{"digest": manifest_digest}]})
        .to_string()
        .into_bytes();
    let tar = tar_of(&[
        ("index.json".to_owned(), index),
        (format!("blobs/sha256/{}", Sha256::of(&manifest)), manifest),
        (format!("blobs/sha256/{}", Sha256::of(&config)), config),
    ]);
    (tar, manifest_digest)
}

/// What syft prints for the image with the manifest `manifest`, with the serial number and the
/// time it stamped.
fn syft_says(manifest: &str, serial: &str, stamped: &str) -> Value {
    json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": serial,
        "version": 1,
        "metadata": {
            "timestamp": stamped,
            "tools": {"components": [{"type": "application", "name": "syft"}]},
            "component": {"type": "container", "name": "image", "version": manifest},
        },
        "components": [
            {"type": "library", "name": "libc6", "purl": "pkg:deb/ubuntu/libc6@2.39?arch=amd64"},
            {"type": "operating-system", "name": "ubuntu", "version": "24.04"},
        ],
    })
}

/// The bytes of [`syft_says`].
fn raw(manifest: &str, serial: &str, stamped: &str) -> Vec<u8> {
    syft_says(manifest, serial, stamped)
        .to_string()
        .into_bytes()
}

/// The asset name of the task image for x86_64.
fn image_name() -> String {
    Image::Task.file_name(COMMIT, Platform::LinuxX86_64)
}

/// A directory holding the image `seed` made, named as the asset: (directory, image path,
/// manifest digest).
fn world(name: &str, seed: &str) -> (PathBuf, PathBuf, String) {
    let dir = scratch(name);
    let (tar, manifest) = layout_of(seed);
    let image = dir.join(image_name());
    std::fs::write(&image, tar).expect("image");
    (dir, image, manifest)
}

/// A syft that prints `answer` and records what it was asked in `asked`.
fn fake(
    answer: Vec<u8>,
    asked: &mut Vec<(PathBuf, Vec<String>)>,
) -> impl FnMut(&Path, &[&str]) -> Result<Vec<u8>, Error> {
    move |dir, args| {
        asked.push((
            dir.to_owned(),
            args.iter().map(|a| (*a).to_owned()).collect(),
        ));
        Ok(answer.clone())
    }
}

fn refused(r: Result<Vec<String>, Error>) -> Vec<String> {
    match r {
        Err(Error::Refused(problems)) => problems,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn the_time_is_the_utc_time_of_the_epoch() {
    // Catches: a calendar that is wrong at a leap day, a month or a year end, or past a
    // century that is not a leap year. The times of the rows are the second before and after each
    // edge.
    for (epoch, want) in [
        (1_788_220_800, "2026-09-01T00:00:00Z"),
        (1_798_761_600, "2027-01-01T00:00:00Z"),
        (1_830_297_599, "2027-12-31T23:59:59Z"),
        (1_835_440_496, "2028-02-29T12:34:56Z"),
        (1_835_481_600, "2028-03-01T00:00:00Z"),
        (4_107_542_400, "2100-03-01T00:00:00Z"),
    ] {
        assert_eq!(rfc3339(epoch), want, "{epoch}");
    }
}

#[test]
fn the_serial_is_a_uuid_made_of_the_manifest_digest() {
    // Catches: a serial that CycloneDX's schema refuses (not `urn:uuid:` and 8-4-4-4-12 lower-case
    // hex, version 8, variant 8 to b), one that is the same for two images, and one that changes
    // from one call to the next (it would put randomness back).
    let a = serial("sha256:aa");
    assert_eq!(a, serial("sha256:aa"));
    assert_ne!(a, serial("sha256:ab"));
    let uuid = a.strip_prefix("urn:uuid:").expect("urn");
    let parts: Vec<&str> = uuid.split('-').collect();
    let lens: Vec<usize> = parts.iter().map(|p| p.len()).collect();
    assert_eq!(lens, [8, 4, 4, 4, 12], "{a}");
    assert!(
        uuid.bytes()
            .all(|b| b == b'-' || matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{a}"
    );
    assert!(parts[2].starts_with('8'), "{a}");
    assert!(parts[3].starts_with(['8', '9', 'a', 'b']), "{a}");
}

#[test]
fn the_same_image_has_the_same_sbom_whatever_syft_stamped() {
    // Catches: the serial number or the time syft stamped left in the file (two runs on the
    // same image would differ: the SBOM would not be reproducible), the time not being the
    // commit's, and an SBOM that does not depend on the image or the time at all.
    let manifest = manifest_of("one");
    let first = sbom_text(
        &raw(&manifest, "urn:uuid:one", "stamp one"),
        &manifest,
        1_788_220_800,
    )
    .expect("first");
    let second = sbom_text(
        &raw(&manifest, "urn:uuid:two", "stamp two"),
        &manifest,
        1_788_220_800,
    )
    .expect("second");
    assert_eq!(first, second);
    assert!(first.ends_with("}\n"), "{first}");
    let bom: Value = serde_json::from_str(&first).expect("json");
    assert_eq!(bom["serialNumber"], serial(&manifest));
    assert_eq!(bom["metadata"]["timestamp"], "2026-09-01T00:00:00Z");
    // What syft says of the image is kept.
    assert_eq!(bom["components"][0]["name"], "libc6");
    let later = sbom_text(
        &raw(&manifest, "urn:uuid:one", "x"),
        &manifest,
        1_788_220_801,
    )
    .expect("later");
    assert_ne!(first, later);
    let other = manifest_of("two");
    let of_other =
        sbom_text(&raw(&other, "urn:uuid:one", "x"), &other, 1_788_220_800).expect("other");
    assert_ne!(first, of_other);
}

#[test]
fn what_syft_prints_that_is_not_a_cyclonedx_document_is_refused() {
    // Catches: an error message of syft, an SPDX or another JSON document, or a document with no
    // metadata to stamp, written as the SBOM of the image.
    for (what, bytes) in [
        ("no JSON", b"syft: boom".to_vec()),
        ("not CycloneDX", br#"{"bomFormat":"SPDX"}"#.to_vec()),
        ("no format", br#"{"metadata":{}}"#.to_vec()),
        ("no metadata", br#"{"bomFormat":"CycloneDX"}"#.to_vec()),
    ] {
        let got = sbom_text(&bytes, "sha256:aa", 0);
        match got {
            Err(Error::Refused(p)) => {
                assert!(p[0].starts_with("syft printed "), "{what}: {p:?}");
            }
            other => panic!("{what}: {other:?}"),
        }
    }
}

/// A good SBOM of the image with the manifest `manifest`, edited by `edit`.
fn sbom_with(manifest: &str, edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut bom = syft_says(manifest, &serial(manifest), "2026-09-01T00:00:00Z");
    edit(&mut bom);
    bom.to_string().into_bytes()
}

#[test]
fn an_sbom_is_the_images_when_it_names_its_manifest_and_lists_the_bases_packages() {
    // Catches: each way an SBOM is not the image's, one planted at a time on a good one: another
    // image's serial or manifest (an SBOM of something else signed as this image's), a component
    // that is not a container, no deb package (the SBOM of the binary alone, or of nothing: the
    // base was not read), a document that is not CycloneDX or not JSON, and one over the size cap.
    // The good one passes, so each refusal is for its own defect.
    let manifest = manifest_of("one");
    assert_eq!(
        problems(&sbom_with(&manifest, |_| {}), &manifest),
        Vec::<String>::new()
    );
    let other = manifest_of("two");
    let rows: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "serial",
            sbom_with(&manifest, |b| b["serialNumber"] = json!("urn:uuid:random")),
            "has serialNumber urn:uuid:random, not ",
        ),
        (
            "other manifest",
            sbom_with(&manifest, |b| {
                b["metadata"]["component"]["version"] = json!(other)
            }),
            "describes sha256:",
        ),
        (
            "not a container",
            sbom_with(&manifest, |b| {
                b["metadata"]["component"]["type"] = json!("file")
            }),
            "describes a component of type file, not a container",
        ),
        (
            "no component",
            sbom_with(&manifest, |b| b["metadata"] = json!({})),
            "describes a component of type (none), not a container",
        ),
        (
            "no deb",
            sbom_with(&manifest, |b| {
                b["components"] = json!([{"type": "library", "purl": "pkg:cargo/serde@1.0.0"}]);
            }),
            "lists no deb package: the packages of the base image were not read",
        ),
        (
            "no components",
            sbom_with(&manifest, |b| {
                b.as_object_mut().expect("object").remove("components");
            }),
            "lists no deb package",
        ),
        (
            "not CycloneDX",
            sbom_with(&manifest, |b| b["bomFormat"] = json!("SPDX")),
            "is not a CycloneDX document",
        ),
        ("not JSON", b"{".to_vec(), "is not JSON: "),
        (
            "too big",
            sbom_with(&manifest, |b| {
                b["padding"] = json!("x".repeat(MAX_BYTES));
            }),
            "bytes, over the 8388608 bytes an SBOM of an image may be",
        ),
    ];
    for (what, bytes, wanted) in rows {
        let got = problems(&bytes, &manifest);
        assert!(
            got.iter().any(|p| p.contains(wanted)),
            "{what}: wanted {wanted:?}, got {got:?}"
        );
    }
    // A document that is not CycloneDX is reported alone: its serial is not a second finding.
    let got = problems(
        &sbom_with(&manifest, |b| b["bomFormat"] = json!("SPDX")),
        &manifest,
    );
    assert_eq!(got.len(), 1, "{got:?}");
}

#[test]
fn the_sbom_is_made_beside_the_image_listed_and_verified() {
    // Catches: syft run on another file or in another directory than the asset (the SBOM would
    // name the commit and platform of no asset), with other arguments than the ones the build
    // pins (`scan`, the OCI archive, CycloneDX JSON, the asset's name, quiet), the SBOM left
    // unlisted or listed with another digest than its bytes, the image's own line lost from the
    // sums, and an SBOM the verify step does not accept.
    let (dir, image, manifest) = world("make", "one");
    let image_line = format!(
        "{}  {}\n",
        Sha256::of(&std::fs::read(&image).expect("image")),
        image_name()
    );
    std::fs::write(dir.join("SHA256SUMS.linux-x86_64"), &image_line).expect("sums");
    let mut asked = Vec::new();
    let printed = make(
        &image,
        Image::Task,
        COMMIT,
        Platform::LinuxX86_64,
        1_788_220_800,
        &mut fake(raw(&manifest, "urn:uuid:random", "now"), &mut asked),
    )
    .expect("made");
    let name = format!("kbf-task-image-{COMMIT}-linux-x86_64.cdx.json");
    let stem = format!("kbf-task-image-{COMMIT}-linux-x86_64");
    assert_eq!(
        asked,
        [(
            dir.clone(),
            vec![
                "scan".to_owned(),
                format!("oci-archive:{}", image_name()),
                "--output".to_owned(),
                "cyclonedx-json".to_owned(),
                "--source-name".to_owned(),
                stem,
                "--quiet".to_owned(),
            ]
        )]
    );
    let written = std::fs::read(dir.join(&name)).expect("sbom");
    let sha = Sha256::of(&written);
    assert_eq!(printed, [format!("made {name}:"), format!("{sha}  {name}")]);
    assert_eq!(
        std::fs::read_to_string(dir.join("SHA256SUMS.linux-x86_64")).expect("sums"),
        format!("{image_line}{sha}  {name}\n")
    );
    let text = String::from_utf8(written).expect("utf-8");
    assert_eq!(
        text,
        sbom_text(&raw(&manifest, "x", "y"), &manifest, 1_788_220_800).expect("same")
    );
    let said = verify(&image, Image::Task, COMMIT, Platform::LinuxX86_64).expect("verified");
    assert_eq!(said, [format!("{name}: the SBOM of the image")]);
    // Made again, it is refused, and nothing is written.
    let again = make(
        &image,
        Image::Task,
        COMMIT,
        Platform::LinuxX86_64,
        1_788_220_800,
        &mut fake(raw(&manifest, "x", "y"), &mut Vec::new()),
    );
    assert_eq!(
        again,
        Err(Error::Failed(format!(
            "{} already lists {name}",
            dir.join("SHA256SUMS.linux-x86_64").display()
        )))
    );
}

#[test]
fn an_image_that_is_not_the_asset_or_has_no_sbom_is_refused_before_syft_runs() {
    // Catches: an SBOM made of a copy under another name (the sums would list a file for no
    // asset), for a service that has none (the frontend's `kbf` is in its tarball, whose SBOM
    // exists), for a platform that is not Linux, for a commit that is not one, or of a file that is
    // not an image layout; and syft run before any of it is known. Each row must not call syft.
    let (dir, image, _) = world("refused-before", "one");
    let copy = dir.join("copy.oci.tar");
    std::fs::copy(&image, &copy).expect("copy");
    let not_an_image = dir.join(Image::Task.file_name("abcdefabcdef", Platform::LinuxArm64));
    std::fs::write(&not_an_image, b"not a tar").expect("file");
    let mut asked = Vec::new();
    let mut syft = fake(raw("sha256:aa", "x", "y"), &mut asked);
    let wrong_name = make(
        &copy,
        Image::Task,
        COMMIT,
        Platform::LinuxX86_64,
        0,
        &mut syft,
    );
    assert_eq!(
        refused(wrong_name),
        [format!(
            "{} is not named {}: the SBOM is made of the asset itself",
            copy.display(),
            image_name()
        )]
    );
    let no_sbom = make(
        &image,
        Image::Frontend,
        COMMIT,
        Platform::LinuxX86_64,
        0,
        &mut syft,
    );
    assert_eq!(
        no_sbom,
        Err(Error::Usage(
            "kbf-frontend-image has no SBOM asset: only the task image does".to_owned()
        ))
    );
    let mac = make(
        &image,
        Image::Task,
        COMMIT,
        Platform::DarwinArm64,
        0,
        &mut syft,
    );
    assert_eq!(
        mac,
        Err(Error::Usage(
            "--platform darwin-arm64: the image is for Linux".to_owned()
        ))
    );
    let commit = make(
        &image,
        Image::Task,
        "xyz",
        Platform::LinuxX86_64,
        0,
        &mut syft,
    );
    assert_eq!(commit, Err(Error::NotACommit("xyz".to_owned())));
    let garbage = make(
        &not_an_image,
        Image::Task,
        "abcdefabcdef",
        Platform::LinuxArm64,
        0,
        &mut syft,
    );
    let got = refused(garbage);
    assert!(
        got[0].starts_with(&format!("{}: ", not_an_image.display())),
        "{got:?}"
    );
    drop(syft);
    assert!(asked.is_empty(), "{asked:?}");
    assert!(!dir.join("SHA256SUMS.linux-x86_64").exists());
}

#[test]
fn a_syft_that_fails_or_prints_the_wrong_thing_leaves_nothing_behind() {
    // Catches: an SBOM file or a sums line left by a run that failed (the sums job would refuse
    // the set, or worse, the build would ship it), syft's failure swallowed, a document that is
    // not the image's (an SBOM of another image, one with no deb package) accepted because
    // syft ran, and the refusal naming the image so the log says which one.
    let (dir, image, manifest) = world("failing", "one");
    let name = format!("kbf-task-image-{COMMIT}-linux-x86_64.cdx.json");
    let sums = dir.join("SHA256SUMS.linux-x86_64");
    let mut fails = |_: &Path, _: &[&str]| -> Result<Vec<u8>, Error> {
        Err(Error::Failed("syft scan: boom".to_owned()))
    };
    let got = make(
        &image,
        Image::Task,
        COMMIT,
        Platform::LinuxX86_64,
        0,
        &mut fails,
    );
    assert_eq!(got, Err(Error::Failed("syft scan: boom".to_owned())));
    let other = manifest_of("two");
    let no_debs = {
        let mut bom = syft_says(&manifest, "x", "y");
        bom["components"] = json!([]);
        bom.to_string().into_bytes()
    };
    for (what, answer, wanted) in [
        (
            "another image",
            raw(&other, "x", "y"),
            "the SBOM describes sha256:",
        ),
        ("no debs", no_debs, "the SBOM lists no deb package"),
        ("no JSON", b"syft: boom".to_vec(), "syft printed no JSON"),
    ] {
        let got = make(
            &image,
            Image::Task,
            COMMIT,
            Platform::LinuxX86_64,
            0,
            &mut fake(answer, &mut Vec::new()),
        );
        let problems = refused(got);
        assert!(
            problems.iter().any(|p| p.contains(wanted)),
            "{what}: {problems:?}"
        );
    }
    assert!(!dir.join(&name).exists());
    assert!(!sums.exists());
}

#[test]
fn verify_wants_the_sbom_of_this_image_beside_it() {
    // Catches: a verify job that passes an image with no SBOM beside it, with one that is another
    // image's (the attestation would sign it as this image's), with one edited after it was made
    // (here: a package list that is no longer there), and one left in place of a file that is not
    // the SBOM at all.
    let (dir, image, manifest) = world("verify", "one");
    let name = format!("kbf-task-image-{COMMIT}-linux-x86_64.cdx.json");
    let missing = refused(verify(&image, Image::Task, COMMIT, Platform::LinuxX86_64));
    assert_eq!(
        missing,
        [format!(
            "{}: the image has no SBOM: {name} is not beside it",
            image.display()
        )]
    );
    let beside = dir.join(&name);
    let good = sbom_with(&manifest, |_| {});
    std::fs::write(&beside, &good).expect("sbom");
    assert!(verify(&image, Image::Task, COMMIT, Platform::LinuxX86_64).is_ok());
    let other = manifest_of("two");
    for (what, bytes) in [
        ("another image's", sbom_with(&other, |_| {})),
        (
            "no packages",
            sbom_with(&manifest, |b| b["components"] = json!([])),
        ),
        ("not an SBOM", b"x".to_vec()),
    ] {
        std::fs::write(&beside, bytes).expect("sbom");
        let got = refused(verify(&image, Image::Task, COMMIT, Platform::LinuxX86_64));
        assert!(
            got.iter()
                .all(|p| p.starts_with(&format!("{}: ", beside.display()))),
            "{what}: {got:?}"
        );
    }
}

#[test]
fn syft_runs_with_the_environment_given_and_nothing_else_and_its_scratch_is_removed() {
    // Catches: the runner's environment reaching syft (a token, a configuration, a proxy: the SBOM
    // would depend on where it ran, and syft would ask the network about updates), the home and
    // temporary directories being the runner's or being left behind,
    // a failing syft read as success and a missing one as a failure of the image.
    let cwd = scratch("syft-process-cwd");
    let scratch = scratch("syft-process");
    let mut env = syft_process(Path::new("/usr/bin/env"), &scratch);
    let printed = env(&cwd, &[]).expect("env");
    let text = String::from_utf8(printed).expect("utf-8");
    let names: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_once('=').map(|(n, _)| n))
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        [
            "HOME",
            "SYFT_CHECK_FOR_APP_UPDATE",
            "SYFT_FILE_METADATA_SELECTION",
            "TMPDIR",
            "XDG_CACHE_HOME",
        ]
    );
    assert!(text.contains("SYFT_CHECK_FOR_APP_UPDATE=false\n"), "{text}");
    let home = text
        .lines()
        .find_map(|l| l.strip_prefix("HOME="))
        .expect("home");
    assert!(Path::new(home).starts_with(&scratch), "{home}");
    assert!(!scratch.exists(), "the scratch directory was left behind");
    let mut fails = syft_process(Path::new("/usr/bin/false"), &scratch);
    let got = fails(&cwd, &["scan", "x"]);
    assert!(
        matches!(&got, Err(Error::Failed(m)) if m.starts_with("syft scan x: ")),
        "{got:?}"
    );
    assert!(!scratch.exists());
    let mut missing = syft_process(Path::new("/nonexistent/syft"), &scratch);
    let got = missing(&cwd, &[]);
    assert!(
        matches!(&got, Err(Error::Spawn { what, .. }) if what == "/nonexistent/syft"),
        "{got:?}"
    );
}
