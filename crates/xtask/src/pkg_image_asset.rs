//! `pkg-image-asset --image <oci.tar> --platform <platform> --commit <sha> --kbf-tarball <file>
//! (--out <dir> | --verify-only)`: the frontend image as a release asset,
//! `kbf-frontend-image-<commit>-<platform>.oci.tar`, and the checks that tie it to the bytes the
//! attestation covers. The image is an OCI image layout in one tar (what `docker buildx build
//! --output type=oci` writes); `docker image load` and `ctr images import` read it.
//!
//! The image is opened (its `index.json`, the manifest, the config and the layers it names, each
//! blob checked against the digest that names it) and refused unless:
//!
//! - its config says `os` linux and the `architecture` of `--platform`;
//! - its entrypoint is exactly `/usr/bin/kbf --mode=frontend` (the configuration is the
//!   mounted `/etc/kbf/kbf.toml`, so no flag carries a secret or an address);
//! - it runs as a user that is not root (a numeric `User`, not `0`, not `root`);
//! - `usr/bin/kbf`, as the layers leave it, is byte for byte the `kbf` inside `--kbf-tarball`,
//!   the tarball `build-commit check-asset` has already shown names the commit: the image
//!   carries the bytes the build identified, not a rebuild. It is mode 0755.
//!
//! Then, without `--verify-only`, it copies the image to
//! `<out>/kbf-frontend-image-<commit>-<platform>.oci.tar` and appends its line to
//! `<out>/SHA256SUMS.<platform>` ([`crate::sums`]), refusing an image already listed. With
//! `--verify-only` it makes the checks on `--image`, which must already be named as the asset
//! (the file the attest job signed, downloaded again by the verify job) and writes nothing.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde_json::Value;

use crate::Error;
use crate::fetch::Sha256;
use crate::package::{Platform, read};
use crate::pkg_asset::{self, Mode};

/// The name of the asset before its commit.
pub const BINARY: &str = "kbf-frontend-image";

/// The end of the asset's file name: an OCI image layout in a tar.
pub const SUFFIX: &str = ".oci.tar";

/// Where the image holds `kbf`, as a layer tar names it (no leading slash).
const KBF_IN_IMAGE: &str = "usr/bin/kbf";

/// The entrypoint of the image.
const ENTRYPOINT: [&str; 2] = ["/usr/bin/kbf", "--mode=frontend"];

/// The file name of the asset.
pub fn file_name(commit: &str, platform: Platform) -> String {
    format!("{BINARY}-{commit}-{}{SUFFIX}", platform.name())
}

/// The inputs of the checks.
pub struct Check<'a> {
    pub platform: Platform,
    /// The `kbf-<commit>-<platform>.tar.gz` the image's `kbf` must equal.
    pub kbf_tarball: &'a Path,
}

/// The `architecture` an image for `platform` carries.
fn architecture(platform: Platform) -> &'static str {
    match platform {
        Platform::LinuxArm64 => "arm64",
        _ => "amd64",
    }
}

/// The regular files of the tar `bytes`, plain or gzip-compressed (a layer is either): path
/// without a leading `./` or `/`, mode, bytes.
fn files(bytes: &[u8]) -> Result<Vec<(String, u32, Vec<u8>)>, String> {
    let plain;
    let tar: &[u8] = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut out)
            .map_err(|e| format!("not a gzip stream: {e}"))?;
        plain = out;
        &plain
    } else {
        bytes
    };
    let not_a_tar = |e: std::io::Error| format!("not a tar: {e}");
    let mut archive = tar::Archive::new(tar);
    let mut found = Vec::new();
    for entry in archive.entries().map_err(not_a_tar)? {
        let mut entry = entry.map_err(not_a_tar)?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let mut body = Vec::new();
        let mode = entry
            .header()
            .mode()
            .and_then(|mode| entry.read_to_end(&mut body).map(|_| mode))
            .map_err(|e| format!("{path}: {e}"))?;
        let path = path
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_owned();
        found.push((path, mode & 0o777, body));
    }
    Ok(found)
}

/// The blob `digest` names (`sha256:<hex>`) in the layout `blobs`, or what is wrong with it:
/// absent, or bytes that do not hash to the digest.
fn blob<'a>(
    blobs: &'a BTreeMap<String, Vec<u8>>,
    digest: &str,
    what: &str,
) -> Result<&'a [u8], String> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(format!("{what} {digest:?} is not a sha256 digest"));
    };
    let bytes = blobs
        .get(&format!("blobs/sha256/{hex}"))
        .ok_or_else(|| format!("holds no blob for the {what} {digest}"))?;
    if Sha256::of(bytes).to_string() != hex {
        return Err(format!(
            "the blob of the {what} {digest} has another digest"
        ));
    }
    Ok(bytes)
}

/// The JSON of the blob `digest` names.
fn json(blobs: &BTreeMap<String, Vec<u8>>, digest: &str, what: &str) -> Result<Value, String> {
    serde_json::from_slice(blob(blobs, digest, what)?)
        .map_err(|e| format!("the {what} {digest} is not JSON: {e}"))
}

/// The string at `value[key]`.
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// What is wrong with `image` (the bytes of the tar) as the frontend image for `c`: one line
/// per problem, none when it meets every check above.
///
/// # Errors
/// The tarball `c` names cannot be read.
pub fn problems(image: &[u8], c: &Check) -> Result<Vec<String>, Error> {
    let shipped = pkg_asset::members(&read(c.kbf_tarball)?)
        .ok()
        .and_then(|m| m.into_iter().find(|m| m.path == "kbf"));
    let Some(shipped) = shipped else {
        return Ok(vec![format!("{} holds no kbf", c.kbf_tarball.display())]);
    };
    Ok(match opened(image, c.platform, &shipped.bytes) {
        Ok(found) | Err(found) => found,
    })
}

/// The problems of `image`; the `Err` is the same list, for the early exits of the walk.
fn opened(image: &[u8], platform: Platform, kbf: &[u8]) -> Result<Vec<String>, Vec<String>> {
    let one = |p: String| vec![p];
    let outer = files(image).map_err(|e| one(format!("is not an OCI image tar: {e}")))?;
    let blobs: BTreeMap<String, Vec<u8>> = outer.into_iter().map(|(p, _, b)| (p, b)).collect();
    let index: Value = blobs
        .get("index.json")
        .ok_or_else(|| one("holds no index.json: not an OCI image layout".to_owned()))
        .and_then(|b| serde_json::from_slice(b).map_err(|e| one(format!("index.json: {e}"))))?;
    let manifests = index["manifests"].as_array().map_or(&[][..], Vec::as_slice);
    let [entry] = manifests else {
        return Err(one(format!(
            "index.json lists {} manifests, not exactly one",
            manifests.len()
        )));
    };
    let digest = text(entry, "digest").unwrap_or_default();
    let manifest = json(&blobs, digest, "manifest").map_err(one)?;
    let config_digest = text(&manifest["config"], "digest").unwrap_or_default();
    let config = json(&blobs, config_digest, "config").map_err(one)?;
    let mut problems = Vec::new();
    let (os, arch) = (text(&config, "os"), text(&config, "architecture"));
    if os != Some("linux") || arch != Some(architecture(platform)) {
        problems.push(format!(
            "config says {}/{}, not linux/{}",
            os.unwrap_or("(none)"),
            arch.unwrap_or("(none)"),
            architecture(platform)
        ));
    }
    let entrypoint: Vec<&str> = config["config"]["Entrypoint"]
        .as_array()
        .map_or(Vec::new(), |a| a.iter().filter_map(Value::as_str).collect());
    if entrypoint != ENTRYPOINT {
        problems.push(format!("entrypoint is {entrypoint:?}, not {ENTRYPOINT:?}"));
    }
    let user = text(&config["config"], "User").unwrap_or_default();
    let uid = user.split(':').next().unwrap_or_default();
    if uid.is_empty() || uid == "root" || uid.parse::<u32>().is_ok_and(|n| n == 0) {
        problems.push(format!(
            "config runs as user {:?}, which is root or unset",
            user
        ));
    }
    // The layers in order: a later one replaces an earlier one's file.
    let mut in_image = None;
    let layers = manifest["layers"].as_array().map_or(&[][..], Vec::as_slice);
    for layer in layers {
        let digest = text(layer, "digest").unwrap_or_default();
        let bytes = blob(&blobs, digest, "layer").map_err(one)?;
        let held = files(bytes).map_err(|e| one(format!("layer {digest}: {e}")))?;
        if let Some(found) = held.into_iter().find(|(p, _, _)| p == KBF_IN_IMAGE) {
            in_image = Some(found);
        }
    }
    match in_image {
        None => problems.push(format!("holds no {KBF_IN_IMAGE}")),
        Some((_, mode, bytes)) => {
            if bytes != kbf {
                problems.push(format!(
                    "{KBF_IN_IMAGE} (sha256:{}) is not the kbf of the attested tarball (sha256:{})",
                    Sha256::of(&bytes),
                    Sha256::of(kbf)
                ));
            }
            if mode != 0o755 {
                problems.push(format!("{KBF_IN_IMAGE} has mode {mode:04o}, not 0755"));
            }
        }
    }
    Ok(problems)
}

/// The `pkg-image-asset` verb: the lines to print.
///
/// # Errors
/// The commit is not one, the image is refused, or a file cannot be read or written.
pub fn asset(image: &Path, commit: &str, mode: &Mode, c: &Check) -> Result<Vec<String>, Error> {
    let said = format!(
        "a {} frontend image of the kbf in {}",
        c.platform.name(),
        c.kbf_tarball.display()
    );
    let name = file_name(commit, c.platform);
    let found = |bytes: &[u8]| problems(bytes, c);
    pkg_asset::publish(image, (commit, c.platform), &name, mode, found, &said)
}

#[cfg(test)]
mod tests;
