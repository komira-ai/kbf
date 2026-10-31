//! `pkg-image-asset --image <oci.tar> --service frontend|coordinator --platform <platform>
//! --commit <sha> (--kbf-tarball <file> | --binary <file>) (--out <dir> | --verify-only)`: the
//! frontend or coordinator image as a release asset, `kbf-frontend-image-<commit>-<platform>.oci.tar`
//! or `kbf-coordinator-image-<commit>-<platform>.oci.tar`, and the checks that tie it to the
//! bytes the attestation covers. The image is an OCI image layout in one tar (what `docker buildx build
//! --output type=docker,oci-mediatypes=true` writes); `docker image load` and `ctr images import` read it.
//!
//! The image is opened (its `index.json`, the manifest, the config and the layers it names, each
//! blob checked against the digest that names it) and refused unless:
//!
//! - its config says `os` linux and the `architecture` of `--platform`;
//! - its entrypoint is exactly `/usr/bin/kbf --mode=frontend` (the configuration is the
//!   mounted `/etc/kbf/kbf.toml`, so no flag carries a secret or an address), or for the
//!   coordinator exactly `/usr/bin/kbf-coordinator`;
//! - it runs as a user that is not root (a numeric `User`, not `0`, not `root`);
//! - for the task image, its layers are those of its base image, the pinned manifest committed as
//!   `packaging/container/kbf-task/base-<platform>.manifest.json`, in order, and exactly one layer
//!   above them, which holds `usr/bin/kbf-task` and no other file ([`Base`], [`layer_problems`]);
//! - `usr/bin/kbf`, as the layers leave it, is byte for byte the `kbf` inside `--kbf-tarball`,
//!   the tarball `build-commit check-asset` has already shown names the commit: the image
//!   carries the bytes the build identified, not a rebuild. It is mode 0755. The coordinator
//!   has no tarball: its `usr/bin/kbf-coordinator` is held to `--binary`, the file the job
//!   built and `build-commit check` has shown names the commit. The verify job has no such
//!   file, so `--verify-only` on a coordinator image makes the other checks alone.
//!
//! Then, without `--verify-only`, it copies the image to
//! `<out>/<service>-image-<commit>-<platform>.oci.tar` and appends its line to
//! `<out>/SHA256SUMS.<platform>` ([`crate::sums`]), refusing an image already listed. For an
//! image with a base (the task image) it also writes its facts, `<service>-image-<commit>-<platform>.facts.json`
//! ([`facts_text`]: the digest of the image's manifest as `index.json` names it, the digest of
//! its config, every layer, and the digest of the base manifest), and lists them too: the
//! attestation of `SHA256SUMS` then covers the digest a registry pin names. With
//! `--verify-only` it makes the checks on `--image`, which must already be named as the asset
//! (the file the attest job signed, downloaded again by the verify job), checks that the facts
//! beside it are the facts of that image, and writes nothing.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde_json::Value;

use crate::fetch::Sha256;
use crate::package::{Platform, open_sums, read, write_new, write_sums};
use crate::pkg_asset::{self, Mode};
use crate::{Error, build_commit, sums};

/// The end of the asset's file name: an OCI image layout in a tar.
pub const SUFFIX: &str = ".oci.tar";

/// The end of the name of the facts of an image: `<image asset name without SUFFIX>.facts.json`.
pub const FACTS_SUFFIX: &str = ".facts.json";

/// The end of the name of the SBOM of an image: `<image asset name without SUFFIX>.cdx.json`,
/// the same ending as the SBOM of a tarball has ([`crate::package`]).
pub const SBOM_SUFFIX: &str = ".cdx.json";

/// The manifest of the base of the task image for linux/amd64, byte for byte as the registry
/// serves it: its sha256 is the digest the Dockerfile pins (a test holds the two together).
const BASE_LINUX_X86_64: &[u8] =
    include_bytes!("../../../packaging/container/kbf-task/base-linux-x86_64.manifest.json");

/// The same for linux/arm64.
const BASE_LINUX_ARM64: &[u8] =
    include_bytes!("../../../packaging/container/kbf-task/base-linux-arm64.manifest.json");

/// A service the release ships as an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Image {
    /// `kbf --mode=frontend`.
    Frontend,
    /// `kbf-coordinator`.
    Coordinator,
    /// `kbf-task`.
    Task,
}

impl Image {
    /// The name of the asset before its commit.
    pub fn asset_name(self) -> &'static str {
        match self {
            Image::Frontend => "kbf-frontend-image",
            Image::Coordinator => "kbf-coordinator-image",
            Image::Task => "kbf-task-image",
        }
    }

    /// The image the asset name `binary` (before `-<commit>-<platform>`) belongs to.
    pub fn named(binary: &str) -> Option<Image> {
        [Image::Frontend, Image::Coordinator, Image::Task]
            .into_iter()
            .find(|i| i.asset_name() == binary)
    }

    /// The program the image runs.
    fn program(self) -> &'static str {
        match self {
            Image::Frontend => "kbf",
            Image::Coordinator => "kbf-coordinator",
            Image::Task => "kbf-task",
        }
    }

    /// Where the image holds the program, as a layer tar names it (no leading slash).
    fn path_in_image(self) -> String {
        format!("usr/bin/{}", self.program())
    }

    /// The entrypoint of the image.
    fn entrypoint(self) -> &'static [&'static str] {
        match self {
            Image::Frontend => &["/usr/bin/kbf", "--mode=frontend"],
            Image::Coordinator => &["/usr/bin/kbf-coordinator"],
            Image::Task => &["/usr/bin/kbf-task"],
        }
    }

    /// How the checks name the image.
    fn label(self) -> &'static str {
        match self {
            Image::Frontend => "frontend",
            Image::Coordinator => "coordinator",
            Image::Task => "task",
        }
    }

    /// The file name of the asset.
    pub fn file_name(self, commit: &str, platform: Platform) -> String {
        format!("{}-{commit}-{}{SUFFIX}", self.asset_name(), platform.name())
    }

    /// The file name of the facts of the asset ([`facts_text`]).
    pub fn facts_name(self, commit: &str, platform: Platform) -> String {
        format!(
            "{}-{commit}-{}{FACTS_SUFFIX}",
            self.asset_name(),
            platform.name()
        )
    }

    /// Whether the release records the facts of this image: it has a base.
    fn has_facts(self) -> bool {
        matches!(self, Image::Task)
    }

    /// Whether the release has an SBOM of this image: the one whose layers hold more than the
    /// program of its own tarball (the Ubuntu base's packages). The `kbf` of the frontend image
    /// has the SBOM of its tarball, and the coordinator has no SBOM asset yet.
    fn has_sbom(self) -> bool {
        matches!(self, Image::Task)
    }

    /// The file name of the CycloneDX SBOM of the asset ([`crate::pkg_image_sbom`]); none for an
    /// image the release has no SBOM of.
    pub fn sbom_name(self, commit: &str, platform: Platform) -> Option<String> {
        self.has_sbom().then(|| {
            format!(
                "{}-{commit}-{}{SBOM_SUFFIX}",
                self.asset_name(),
                platform.name()
            )
        })
    }

    /// The base the image for `platform` is built on, parsed from the manifest committed
    /// beside its Dockerfile; none for an image that has no base to check or a platform with
    /// no image.
    ///
    /// # Errors
    /// The committed manifest is not one ([`Base::parse`]).
    pub fn base(self, platform: Platform) -> Result<Option<Base>, Error> {
        match (self, platform) {
            (Image::Task, Platform::LinuxX86_64) => self.pinned(platform, BASE_LINUX_X86_64),
            (Image::Task, Platform::LinuxArm64) => self.pinned(platform, BASE_LINUX_ARM64),
            _ => Ok(None),
        }
    }

    /// The base the committed manifest `bytes` is, for the image of `platform`.
    fn pinned(self, platform: Platform, bytes: &[u8]) -> Result<Option<Base>, Error> {
        Base::parse(bytes).map(Some).map_err(|e| {
            Error::Failed(format!(
                "the pinned base manifest of the {} image for {} {e}",
                self.label(),
                platform.name()
            ))
        })
    }
}

/// The image, commit and platform an asset or facts file name `name` ending `suffix` is the
/// name of (`<image>-<commit>-<platform><suffix>`, for a Linux platform).
fn parsed(name: &str, suffix: &str) -> Option<(Image, String, Platform)> {
    let stem = name.strip_suffix(suffix)?;
    let image = [Image::Frontend, Image::Coordinator, Image::Task]
        .into_iter()
        .find(|i| {
            stem.strip_prefix(i.asset_name())
                .is_some_and(|r| r.starts_with('-'))
        })?;
    let rest = &stem[image.asset_name().len() + 1..];
    let (commit, platform) = rest.split_once('-')?;
    let platform = Platform::named(platform)
        .filter(|p| matches!(p, Platform::LinuxX86_64 | Platform::LinuxArm64))?;
    build_commit::is_commit(commit).then(|| (image, commit.to_owned(), platform))
}

/// The facts file of the image asset `asset`, when the release records facts for it.
pub fn facts_of(asset: &str) -> Option<String> {
    let (image, commit, platform) = parsed(asset, SUFFIX)?;
    image
        .has_facts()
        .then(|| image.facts_name(&commit, platform))
}

/// The image asset the facts file `facts` states the facts of: the inverse of [`facts_of`].
pub fn image_of(facts: &str) -> Option<String> {
    let (image, commit, platform) = parsed(facts, FACTS_SUFFIX)?;
    image
        .has_facts()
        .then(|| image.file_name(&commit, platform))
}

/// The SBOM file of the image asset `asset`, when the release has an SBOM of it.
pub fn sbom_of(asset: &str) -> Option<String> {
    let (image, commit, platform) = parsed(asset, SUFFIX)?;
    image.sbom_name(&commit, platform)
}

/// The image asset the SBOM file `sbom` describes: the inverse of [`sbom_of`]. The SBOM of a
/// binary's tarball (`kbf-task-<commit>-<platform>.cdx.json`) is not one.
pub fn image_of_sbom(sbom: &str) -> Option<String> {
    let (image, commit, platform) = parsed(sbom, SBOM_SUFFIX)?;
    image.has_sbom().then(|| image.file_name(&commit, platform))
}

/// The digest `index.json` names for the manifest of the image `image` (the bytes of its tar),
/// the manifest blob hashed to it.
///
/// # Errors
/// The image is not a layout with one manifest whose blobs match their digests.
pub(crate) fn manifest_digest(image: &[u8]) -> Result<String, String> {
    layout(image).map(|l| l.manifest_digest)
}

/// Whether `file` is the facts file of an image of `commit`: exactly [`Image::facts_name`], not
/// any file that ends in [`FACTS_SUFFIX`].
pub fn is_facts_of(file: &str, commit: &str) -> bool {
    parsed(file, FACTS_SUFFIX).is_some_and(|(image, c, _)| image.has_facts() && c == commit)
}

/// The image a task image is built on: the manifest the Dockerfile pins by digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    /// `sha256:<hex>` of the manifest's bytes: the digest the Dockerfile's `FROM` names.
    pub digest: String,
    /// The digests of its layers, bottom first.
    pub layers: Vec<String>,
}

impl Base {
    /// The base the manifest `bytes` is: refused when it is an image index (the release pins
    /// the manifest of one platform), lists no layer, or names a layer by anything but sha256.
    pub fn parse(bytes: &[u8]) -> Result<Base, String> {
        let manifest: Value =
            serde_json::from_slice(bytes).map_err(|e| format!("is not JSON: {e}"))?;
        if manifest.get("manifests").is_some() {
            return Err(
                "is an image index, not the manifest of one platform: pin a platform's digest"
                    .to_owned(),
            );
        }
        let listed = manifest["layers"].as_array().map_or(&[][..], Vec::as_slice);
        if listed.is_empty() {
            return Err("lists no layers".to_owned());
        }
        let mut layers = Vec::new();
        for layer in listed {
            let digest = text(layer, "digest").unwrap_or_default();
            let hex = digest.strip_prefix("sha256:").unwrap_or_default();
            if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return Err(format!("names a layer {digest:?}, not a sha256 digest"));
            }
            layers.push(digest.to_owned());
        }
        Ok(Base {
            digest: format!("sha256:{}", Sha256::of(bytes)),
            layers,
        })
    }
}

/// What is wrong with the layer digests `layers` of an image as the layers of an image on a
/// base whose layers are `base`: the base's layers first, in order, and exactly one more.
pub fn layer_problems(layers: &[String], base: &[String]) -> Vec<String> {
    if layers.len() < base.len() {
        return vec![format!(
            "the image has {} layers, fewer than the {} of its base",
            layers.len(),
            base.len()
        )];
    }
    let mut problems: Vec<String> = base
        .iter()
        .zip(layers)
        .enumerate()
        .filter(|(_, (b, l))| b != l)
        .map(|(i, (b, l))| format!("layer {} is {l}, the base's is {b}", i + 1))
        .collect();
    match layers.len() - base.len() {
        0 if problems.is_empty() => problems.push(
            "the image has no layer above its base: the kbf-task layer is missing".to_owned(),
        ),
        0 | 1 => {}
        more => problems.push(format!(
            "the image has {more} layers above its base, not exactly one"
        )),
    }
    problems
}

/// The bytes the image's program must equal.
#[derive(Clone, Copy)]
pub enum Reference<'a> {
    /// The `kbf` inside a `kbf-<commit>-<platform>.tar.gz`.
    KbfTarball(&'a Path),
    /// A built program (the coordinator and the task image have no tarball).
    Binary(&'a Path),
}

/// The inputs of the checks.
pub struct Check<'a> {
    pub image: Image,
    pub platform: Platform,
    /// What the program of the image must equal; `None` only to check an asset where the
    /// build's own file is not at hand (`--verify-only` of a coordinator or task image).
    pub reference: Option<Reference<'a>>,
    /// The base the image is built on ([`Image::base`]); `None` for an image that has none.
    pub base: Option<&'a Base>,
}

/// The `architecture` an image for `platform` carries.
fn architecture(platform: Platform) -> &'static str {
    match platform {
        Platform::LinuxArm64 => "arm64",
        _ => "amd64",
    }
}

/// The tar `bytes` as a plain tar: gunzipped when it is gzip-compressed.
fn plain_tar(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes.to_vec());
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| format!("not a gzip stream: {e}"))?;
    Ok(out)
}

/// The entries of the layer `bytes` that a layer holding one program file may not have: anything
/// that is neither a regular file nor the directories `usr/` and `usr/bin/` (a symlink, hard
/// link, device, fifo, or another directory, whose mode or owner the layer would change), each as
/// `path (kind)`. Regular files are judged by their paths, not here.
fn odd_entries(bytes: &[u8]) -> Result<Vec<String>, String> {
    let plain = plain_tar(bytes)?;
    let not_a_tar = |e: std::io::Error| format!("not a tar: {e}");
    let mut archive = tar::Archive::new(plain.as_slice());
    let mut odd = Vec::new();
    for entry in archive.entries().map_err(not_a_tar)? {
        let entry = entry.map_err(not_a_tar)?;
        let kind = entry.header().entry_type();
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let path = path
            .trim_start_matches("./")
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_owned();
        let allowed = match kind {
            tar::EntryType::Regular => true,
            tar::EntryType::Directory => path == "usr" || path == "usr/bin",
            _ => false,
        };
        if !allowed {
            odd.push(format!("{path} ({kind:?})"));
        }
    }
    Ok(odd)
}

/// The regular files of the tar `bytes`, plain or gzip-compressed (a layer is either): path
/// without a leading `./` or `/`, mode, bytes.
fn files(bytes: &[u8]) -> Result<Vec<(String, u32, Vec<u8>)>, String> {
    let plain = plain_tar(bytes)?;
    let tar: &[u8] = &plain;
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
        let mode = entry.header().mode().map_err(not_a_tar)?;
        entry.read_to_end(&mut body).map_err(not_a_tar)?;
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

/// What is wrong with `image` (the bytes of the tar) as the image of `c`: one line per
/// problem, none when it meets every check above.
///
/// # Errors
/// The file `c` names as the reference cannot be read.
pub fn problems(image: &[u8], c: &Check) -> Result<Vec<String>, Error> {
    let expected = match c.reference {
        None => None,
        Some(Reference::Binary(path)) => {
            Some((read(path)?, format!("the built {}", c.image.program())))
        }
        Some(Reference::KbfTarball(path)) => {
            let shipped = pkg_asset::members(&read(path)?)
                .ok()
                .and_then(|m| m.into_iter().find(|m| m.path == "kbf"));
            let Some(shipped) = shipped else {
                return Ok(vec![format!("{} holds no kbf", path.display())]);
            };
            Some((shipped.bytes, "the kbf of the attested tarball".to_owned()))
        }
    };
    let expected = expected.as_ref().map(|(b, w)| (b.as_slice(), w.as_str()));
    Ok(match opened(image, c.image, c.platform, expected, c.base) {
        Ok(found) | Err(found) => found,
    })
}

/// An image layout opened: its one manifest and config, each hashed to the digest that names it.
struct Layout {
    blobs: BTreeMap<String, Vec<u8>>,
    /// The digest `index.json` gives the manifest.
    manifest_digest: String,
    manifest: Value,
    config_digest: String,
    config: Value,
}

/// The layout in the tar `image`, or what is wrong with it.
fn layout(image: &[u8]) -> Result<Layout, String> {
    let outer = files(image).map_err(|e| format!("is not an OCI image tar: {e}"))?;
    let blobs: BTreeMap<String, Vec<u8>> = outer.into_iter().map(|(p, _, b)| (p, b)).collect();
    let index: Value = blobs
        .get("index.json")
        .ok_or_else(|| "holds no index.json: not an OCI image layout".to_owned())
        .and_then(|b| serde_json::from_slice(b).map_err(|e| format!("index.json: {e}")))?;
    let manifests = index["manifests"].as_array().map_or(&[][..], Vec::as_slice);
    let [entry] = manifests else {
        return Err(format!(
            "index.json lists {} manifests, not exactly one",
            manifests.len()
        ));
    };
    let manifest_digest = text(entry, "digest").unwrap_or_default().to_owned();
    let manifest = json(&blobs, &manifest_digest, "manifest")?;
    let config_digest = text(&manifest["config"], "digest")
        .unwrap_or_default()
        .to_owned();
    let config = json(&blobs, &config_digest, "config")?;
    Ok(Layout {
        blobs,
        manifest_digest,
        manifest,
        config_digest,
        config,
    })
}

/// The digests of the layers `manifest` lists, bottom first.
fn layer_digests(manifest: &Value) -> Vec<String> {
    manifest["layers"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|l| text(l, "digest").unwrap_or_default().to_owned())
        .collect()
}

/// The facts of the image `image` (the bytes of its tar) built on `base`, as the text of its
/// facts file: JSON with a trailing newline, the same bytes for the same image. `manifest` is
/// the digest `index.json` names (the blob it names is hashed to it), the digest a registry
/// pin or pull by digest uses.
///
/// # Errors
/// The image is not a layout with one manifest whose blobs match their digests.
pub fn facts_text(
    image: &[u8],
    kind: Image,
    commit: &str,
    platform: Platform,
    base: &Base,
) -> Result<String, Error> {
    let layout = layout(image).map_err(|e| Error::Refused(vec![e]))?;
    let facts = serde_json::json!({
        "schema": 1,
        "image": kind.asset_name(),
        "commit": commit,
        "platform": platform.name(),
        "manifest": layout.manifest_digest,
        "config": layout.config_digest,
        "base": base.digest,
        "layers": layer_digests(&layout.manifest),
    });
    let mut text =
        serde_json::to_string_pretty(&facts).map_err(|e| Error::Failed(format!("facts: {e}")))?;
    text.push('\n');
    Ok(text)
}

/// The problems of `image`; the `Err` is the same list, for the early exits of the walk.
fn opened(
    image: &[u8],
    kind: Image,
    platform: Platform,
    expected: Option<(&[u8], &str)>,
    base: Option<&Base>,
) -> Result<Vec<String>, Vec<String>> {
    let one = |p: String| vec![p];
    let Layout {
        blobs,
        manifest,
        config,
        ..
    } = layout(image).map_err(one)?;
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
    if entrypoint != kind.entrypoint() {
        problems.push(format!(
            "entrypoint is {entrypoint:?}, not {:?}",
            kind.entrypoint()
        ));
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
    let in_image_path = kind.path_in_image();
    let mut in_image = None;
    let layers = manifest["layers"].as_array().map_or(&[][..], Vec::as_slice);
    // Where the layer above the base is, and what it holds besides the program.
    let above = base.map_or(usize::MAX, |b| b.layers.len());
    let mut others = Vec::new();
    for (at, layer) in layers.iter().enumerate() {
        let digest = text(layer, "digest").unwrap_or_default();
        let bytes = blob(&blobs, digest, "layer").map_err(one)?;
        // A layer of the base is its digest, which the base names: nothing in it is read.
        if base.is_some_and(|b| b.layers.get(at).is_some_and(|d| d == digest)) {
            continue;
        }
        let held = files(bytes).map_err(|e| one(format!("layer {digest}: {e}")))?;
        if at == above {
            others = held
                .iter()
                .map(|(p, _, _)| p.clone())
                .filter(|p| *p != in_image_path)
                .collect();
            others.extend(odd_entries(bytes).map_err(|e| one(format!("layer {digest}: {e}")))?);
        }
        if let Some(found) = held.into_iter().find(|(p, _, _)| *p == in_image_path) {
            in_image = Some(found);
        }
    }
    if let Some(base) = base {
        problems.extend(layer_problems(&layer_digests(&manifest), &base.layers));
        if !others.is_empty() {
            others.sort();
            problems.push(format!(
                "the layer above the base holds {others:?} besides {in_image_path}"
            ));
        }
    }
    match in_image {
        None => problems.push(format!("holds no {in_image_path}")),
        Some((_, mode, bytes)) => {
            if let Some((reference, what)) = expected
                && bytes != reference
            {
                problems.push(format!(
                    "{in_image_path} (sha256:{}) is not {what} (sha256:{})",
                    Sha256::of(&bytes),
                    Sha256::of(reference)
                ));
            }
            if mode != 0o755 {
                problems.push(format!("{in_image_path} has mode {mode:04o}, not 0755"));
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
    let said = match c.reference {
        Some(Reference::KbfTarball(p) | Reference::Binary(p)) => format!(
            "a {} {} image of the {} in {}",
            c.platform.name(),
            c.image.label(),
            c.image.program(),
            p.display()
        ),
        None => format!("a {} {} image", c.platform.name(), c.image.label()),
    };
    let name = c.image.file_name(commit, c.platform);
    let found = |bytes: &[u8]| problems(bytes, c);
    let mut printed = pkg_asset::publish(image, (commit, c.platform), &name, mode, found, &said)?;
    if let Some(base) = c.base {
        printed.extend(facts(image, commit, mode, c, base)?);
    }
    Ok(printed)
}

/// The facts of the image `image` that `asset` has accepted: written beside the asset and listed
/// in the sums with [`Mode::Add`]; with [`Mode::VerifyOnly`] the facts file beside `image` must
/// be exactly the facts of it.
fn facts(
    image: &Path,
    commit: &str,
    mode: &Mode,
    c: &Check,
    base: &Base,
) -> Result<Vec<String>, Error> {
    let text = facts_text(&read(image)?, c.image, commit, c.platform, base)?;
    let name = c.image.facts_name(commit, c.platform);
    match mode {
        Mode::Add(out) => {
            let (sums_path, mut lines) = open_sums(out, c.platform)?;
            if lines.iter().any(|l| l.name == name) {
                return Err(Error::Failed(format!(
                    "{} already lists {name}",
                    sums_path.display()
                )));
            }
            write_new(&out.join(&name), text.as_bytes())?;
            let sha256 = Sha256::of(text.as_bytes());
            lines.push(sums::Line {
                sha256,
                name: name.clone(),
            });
            write_sums(&sums_path, &lines)?;
            Ok(vec![
                format!("recorded {name}:"),
                format!("{sha256}  {name}"),
            ])
        }
        Mode::VerifyOnly => {
            let beside = image.with_file_name(&name);
            let held = match std::fs::read(&beside) {
                Ok(held) => held,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Error::Refused(vec![format!(
                        "{}: the image has no facts file: {name} is not beside it",
                        image.display()
                    )]));
                }
                Err(e) => return Err(Error::spawn(format!("read {}", beside.display()))(e)),
            };
            if held != text.as_bytes() {
                return Err(Error::Refused(vec![format!(
                    "{}: these are not the facts of the image {}",
                    beside.display(),
                    image.display()
                )]));
            }
            Ok(vec![format!("{name}: the facts of the image")])
        }
    }
}

#[cfg(test)]
mod tests;
