//! `pkg-image-sbom --service task --image <asset> --platform <platform> --commit <sha>
//! (--syft <program> --source-date-epoch <seconds> | --verify-only)`: the CycloneDX SBOM of a
//! release image, `kbf-task-image-<commit>-<platform>.cdx.json`, made from the OCI layout of the
//! image the release ships. The SBOM of the `kbf-task` tarball describes the binary alone; this
//! one is read from the layers, so it names the Ubuntu base's packages too, which is what a
//! consumer of the image needs to know.
//!
//! Without `--verify-only` (the build job, after [`crate::pkg_image_asset`] made the image an
//! asset):
//!
//! - `--image` must be the asset itself (`<out>/kbf-task-image-<commit>-<platform>.oci.tar`: the
//!   name is the one the sums list, and the SBOM goes beside it);
//! - the pinned `--syft` is run in that directory on `oci-archive:<file name>` with an empty
//!   environment (no update check, no configuration of the runner's, its own home and temporary
//!   directories, removed after) and prints the SBOM as CycloneDX JSON;
//! - the SBOM is made reproducible ([`sbom_text`]): syft stamps a random `serialNumber` and the
//!   time it ran. The serial becomes one derived from the digest of the image's manifest
//!   ([`serial`]) and the time becomes `--source-date-epoch` (the commit's time, as the image's
//!   own build uses), so the same image has the same SBOM bytes whenever and wherever it is made
//!   (everything else syft prints is a function of the image and of its version, which is pinned);
//! - the result is refused unless it meets [`problems`]: a CycloneDX document of a container
//!   whose version is this image's manifest digest, that lists the deb packages of the base
//!   (an SBOM of the binary alone, or of nothing, is not the image's), and is not larger than
//!   [`MAX_BYTES`];
//! - the SBOM is written beside the image and its line is appended to
//!   `<out>/SHA256SUMS.<platform>`, as `pkg-image-asset` does for the image; either already
//!   there is refused.
//!
//! With `--verify-only` (the verify job, on the files the attest job signed) it runs nothing: the
//! SBOM beside `--image` must meet [`problems`] for that image. The attestation says the SBOM was
//! signed; this says it is the SBOM of the image that was signed.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::fetch::Sha256;
use crate::package::{Platform, open_sums, read, write_new, write_sums};
use crate::pkg_image_asset::{self, Image};
use crate::{Error, build_commit, sums};

/// The most an SBOM of an image may be: well under what the attestation of an SBOM takes, and
/// far above what the packages of a minimal base need.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

/// The `type` CycloneDX gives the component that is the image.
const CONTAINER: &str = "container";

/// The purl prefix of a package of a Debian-family base.
const DEB_PURL: &str = "pkg:deb/";

/// Runs syft in the directory given with the arguments given and says what it printed on stdout.
pub type Syft<'a> = dyn FnMut(&Path, &[&str]) -> Result<Vec<u8>, Error> + 'a;

/// The closure that runs the program `syft`: with the environment [`syft_env`] and nothing else,
/// in the directory it is given, with the directory `scratch` for syft's home, cache and
/// temporary files, which it makes and removes after (the caller names one no one else uses).
pub fn syft_process<'a>(
    syft: &'a Path,
    scratch: &'a Path,
) -> impl FnMut(&Path, &[&str]) -> Result<Vec<u8>, Error> + 'a {
    move |dir, args| {
        for sub in ["home", "cache", "tmp"] {
            std::fs::create_dir_all(scratch.join(sub))
                .map_err(Error::spawn(format!("create {}", scratch.display())))?;
        }
        let out = Command::new(syft)
            .current_dir(dir)
            .env_clear()
            .envs(syft_env(scratch))
            .args(args)
            .output();
        // The scratch directory holds nothing the build keeps.
        let _ = std::fs::remove_dir_all(scratch);
        let out = out.map_err(Error::spawn(syft.display().to_string()))?;
        if !out.status.success() {
            return Err(Error::Failed(format!(
                "syft {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim_end()
            )));
        }
        Ok(out.stdout)
    }
}

/// The whole environment syft runs in: no update check (it would ask the network), no
/// configuration of the runner's (home is the scratch directory), and no per-file records, which
/// an SBOM of packages does not need and which would make it megabytes.
fn syft_env(scratch: &Path) -> Vec<(&'static str, String)> {
    let at = |sub: &str| scratch.join(sub).display().to_string();
    vec![
        ("HOME", at("home")),
        ("XDG_CACHE_HOME", at("cache")),
        ("TMPDIR", at("tmp")),
        ("SYFT_CHECK_FOR_APP_UPDATE", "false".to_owned()),
        ("SYFT_FILE_METADATA_SELECTION", "none".to_owned()),
    ]
}

/// The arguments syft is run with, in the directory of the image: `source` is the image as an
/// `oci-archive:` reference by file name, and `stem` names the SBOM's subject (the asset's name
/// without its ending), so the document does not carry the runner's paths.
fn syft_args<'a>(source: &'a str, stem: &'a str) -> Vec<&'a str> {
    vec![
        "scan",
        source,
        "--output",
        "cyclonedx-json",
        "--source-name",
        stem,
        "--quiet",
    ]
}

/// The serial number of the SBOM of the image whose manifest digest is `manifest`: a UUID (the
/// form CycloneDX requires) made of the digest, so it is the image's and nothing else's.
pub fn serial(manifest: &str) -> String {
    let hash = Sha256::of(format!("kbf-image-sbom\n{manifest}").as_bytes()).to_string();
    let variant = ["8", "9", "a", "b"][usize::from_str_radix(&hash[16..17], 16).unwrap_or(0) & 3];
    format!(
        "urn:uuid:{}-{}-8{}-{variant}{}-{}",
        &hash[0..8],
        &hash[8..12],
        &hash[13..16],
        &hash[17..20],
        &hash[20..32]
    )
}

/// `epoch` (seconds since 1970) as the UTC time CycloneDX writes: `YYYY-MM-DDThh:mm:ssZ`.
pub fn rfc3339(epoch: u64) -> String {
    let days = i64::try_from(epoch / 86_400).unwrap_or(i64::MAX / 2);
    let secs = epoch % 86_400;
    // The civil date of a day count (days from 0000-03-01, in 400-year eras).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// The string at `value[key]`.
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// What syft printed, `raw`, as the text of the SBOM file: the same JSON with the serial number
/// of the image whose manifest digest is `manifest` and the time `epoch`, pretty-printed with a
/// trailing newline. The same image gives the same bytes: nothing else of what syft prints
/// varies.
///
/// # Errors
/// `raw` is not a CycloneDX JSON object with a `metadata` object.
pub fn sbom_text(raw: &[u8], manifest: &str, epoch: u64) -> Result<String, Error> {
    let refuse = |why: &str| Error::Refused(vec![format!("syft printed {why}")]);
    let mut bom: Value = serde_json::from_slice(raw)
        .map_err(|e| Error::Refused(vec![format!("syft printed no JSON: {e}")]))?;
    if text(&bom, "bomFormat") != Some("CycloneDX") {
        return Err(refuse(
            "no CycloneDX document (`bomFormat` is not CycloneDX)",
        ));
    }
    let Some(metadata) = bom.get_mut("metadata").and_then(Value::as_object_mut) else {
        return Err(refuse("a document with no `metadata` object"));
    };
    metadata.insert("timestamp".to_owned(), Value::String(rfc3339(epoch)));
    bom["serialNumber"] = Value::String(serial(manifest));
    let mut out =
        serde_json::to_string_pretty(&bom).map_err(|e| Error::Failed(format!("sbom: {e}")))?;
    out.push('\n');
    Ok(out)
}

/// What is wrong with `bytes` as the SBOM of the image whose manifest digest is `manifest`: one
/// line per problem, none when it is at most [`MAX_BYTES`], a CycloneDX JSON object, has the
/// serial number of that image ([`serial`]), describes a container whose version is that digest,
/// and lists at least one deb package. (A document that is not CycloneDX JSON is reported alone.)
pub fn problems(bytes: &[u8], manifest: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if bytes.len() > MAX_BYTES {
        problems.push(format!(
            "is {} bytes, over the {MAX_BYTES} bytes an SBOM of an image may be",
            bytes.len()
        ));
    }
    let bom: Value = match serde_json::from_slice(bytes) {
        Ok(bom) => bom,
        Err(e) => {
            problems.push(format!("is not JSON: {e}"));
            return problems;
        }
    };
    if text(&bom, "bomFormat") != Some("CycloneDX") {
        problems.push("is not a CycloneDX document (`bomFormat` is not CycloneDX)".to_owned());
        return problems;
    }
    let want = serial(manifest);
    if text(&bom, "serialNumber") != Some(want.as_str()) {
        problems.push(format!(
            "has serialNumber {}, not {want}: it is not the SBOM of the image with the manifest {manifest}",
            text(&bom, "serialNumber").unwrap_or("(none)")
        ));
    }
    let described = &bom["metadata"]["component"];
    if text(described, "type") != Some(CONTAINER) {
        problems.push(format!(
            "describes a component of type {}, not a {CONTAINER}: it is not the SBOM of an image",
            text(described, "type").unwrap_or("(none)")
        ));
    }
    if text(described, "version") != Some(manifest) {
        problems.push(format!(
            "describes {}, not the image with the manifest {manifest}",
            text(described, "version").unwrap_or("(no version)")
        ));
    }
    let debs = bom["components"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|c| text(c, "purl").is_some_and(|p| p.starts_with(DEB_PURL)))
        .count();
    if debs == 0 {
        problems
            .push("lists no deb package: the packages of the base image were not read".to_owned());
    }
    problems
}

/// An image checked and read: where its SBOM goes and what the SBOM must say of it.
struct Prepared {
    /// The directory of the image, which holds the SBOM and the sums.
    dir: PathBuf,
    /// The file name of the image, which is the asset's.
    image_name: String,
    /// The file name of the SBOM.
    name: String,
    /// The digest `index.json` names for the image's manifest.
    manifest: String,
}

/// The checks `make` and `verify` share: the commit and platform are valid, the service has an
/// SBOM, and `image` is the asset (named for the commit and the platform), an image layout.
fn prepare(image: &Path, kind: Image, commit: &str, platform: Platform) -> Result<Prepared, Error> {
    if !build_commit::is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    if !matches!(platform, Platform::LinuxX86_64 | Platform::LinuxArm64) {
        return Err(Error::Usage(format!(
            "--platform {}: the image is for Linux",
            platform.name()
        )));
    }
    let Some(name) = kind.sbom_name(commit, platform) else {
        return Err(Error::Usage(format!(
            "{} has no SBOM asset: only the task image does",
            kind.asset_name()
        )));
    };
    let image_name = kind.file_name(commit, platform);
    if image.file_name().and_then(|n| n.to_str()) != Some(image_name.as_str()) {
        return Err(Error::Refused(vec![format!(
            "{} is not named {image_name}: the SBOM is made of the asset itself",
            image.display()
        )]));
    }
    let manifest = pkg_image_asset::manifest_digest(&read(image)?)
        .map_err(|e| Error::Refused(vec![format!("{}: {e}", image.display())]))?;
    let dir = image
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_owned();
    Ok(Prepared {
        dir,
        image_name,
        name,
        manifest,
    })
}

/// The `pkg-image-sbom` verb without `--verify-only`: the lines to print. `epoch` is the time the
/// SBOM carries ([`sbom_text`]), `syft` runs the program.
///
/// # Errors
/// The image is not the asset or not an image layout, syft fails or prints no CycloneDX document,
/// the SBOM is refused ([`problems`]), or it or its line is already there.
pub fn make(
    image: &Path,
    kind: Image,
    commit: &str,
    platform: Platform,
    epoch: u64,
    syft: &mut Syft,
) -> Result<Vec<String>, Error> {
    let p = prepare(image, kind, commit, platform)?;
    let (sums_path, mut lines) = open_sums(&p.dir, platform)?;
    if lines.iter().any(|l| l.name == p.name) {
        return Err(Error::Failed(format!(
            "{} already lists {}",
            sums_path.display(),
            p.name
        )));
    }
    let stem = p.name.trim_end_matches(pkg_image_asset::SBOM_SUFFIX);
    let source = format!("oci-archive:{}", p.image_name);
    let raw = syft(&p.dir, &syft_args(&source, stem))?;
    let text = sbom_text(&raw, &p.manifest, epoch)?;
    let found = problems(text.as_bytes(), &p.manifest);
    if !found.is_empty() {
        return Err(Error::Refused(
            found
                .into_iter()
                .map(|f| format!("{}: the SBOM {f}", p.image_name))
                .collect(),
        ));
    }
    write_new(&p.dir.join(&p.name), text.as_bytes())?;
    let sha256 = Sha256::of(text.as_bytes());
    lines.push(sums::Line {
        sha256,
        name: p.name.clone(),
    });
    write_sums(&sums_path, &lines)?;
    Ok(vec![
        format!("made {}:", p.name),
        format!("{sha256}  {}", p.name),
    ])
}

/// The `pkg-image-sbom --verify-only` verb: the lines to print.
///
/// # Errors
/// The image is not the asset or not an image layout, the SBOM is not beside it, or it is
/// refused ([`problems`]).
pub fn verify(
    image: &Path,
    kind: Image,
    commit: &str,
    platform: Platform,
) -> Result<Vec<String>, Error> {
    let p = prepare(image, kind, commit, platform)?;
    let beside = p.dir.join(&p.name);
    if !beside.exists() {
        return Err(Error::Refused(vec![format!(
            "{}: the image has no SBOM: {} is not beside it",
            image.display(),
            p.name
        )]));
    }
    let held = read(&beside)?;
    let found = problems(&held, &p.manifest);
    if !found.is_empty() {
        return Err(Error::Refused(
            found
                .into_iter()
                .map(|f| format!("{}: {f}", beside.display()))
                .collect(),
        ));
    }
    Ok(vec![format!("{}: the SBOM of the image", p.name)])
}

#[cfg(test)]
mod tests;
