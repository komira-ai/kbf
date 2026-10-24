//! The container images of the frontend and the update coordinator as release assets
//! (docs/artifacts.md, docs/deploy/containers.md), and the checks that tie each to the
//! bytes the attestation covers.
//!
//! An image is the output of `docker save`: a tar holding `manifest.json`, the image's
//! configuration and its layers, which `docker load` and `ctr images import` both read (the
//! operator imports it with `k3s ctr -n k8s.io images import`; there is no registry). It is
//! not an OCI archive made by a builder plug-in, so the runner needs nothing but Docker, and
//! the name the image is known by (`kbf-frontend:<commit>`) travels in the file.
//!
//! - `image-build --image frontend|coordinator --dockerfile-dir <dir> --binary <file> --commit
//!   <sha> --out <dir> [--docker <program>]` ([`build`]): the build context is the directory's
//!   `Dockerfile` and the binary, nothing else; `docker build` it as `<repository>:<commit>` with
//!   the commit as a build argument (the image says it in its label), `docker save` it to
//!   `<out>/<repository>.tar` and remove it from the daemon again, so that `image-test` loads
//!   the archive and not a copy the daemon already had.
//! - `image-asset --image frontend|coordinator --archive <tar> --binary <file> --sbom <file>
//!   --platform <platform> --commit <sha> --out <dir>` ([`asset`]): refuses the archive
//!   unless ([`problems`]) it holds one image, tagged `<repository>:<commit>`, for the
//!   platform's architecture; the image runs as a user that is not root, has the binary as its
//!   entrypoint and no environment variable but `PATH`; says it was built from the commit
//!   (`org.opencontainers.image.revision`); holds `usr/bin/<binary>` byte for byte the file
//!   `--binary`, which `build-commit check` has shown names the commit; and bakes no file into
//!   the directories the orchestrator mounts the settings, keys and state in
//!   ([`MOUNTED`]). Then it copies the archive to `<out>/<repository>-image-<commit>-<platform>.tar`
//!   and `--sbom` (the CycloneDX document of the binary) beside it as `.cdx.json`, and
//!   appends both lines to `<out>/SHA256SUMS.<platform>` ([`crate::sums`]).
//! - `image-test --image frontend|coordinator --archive <tar> --commit <sha> [--docker
//!   <program>]` ([`run`]): loads the archive, runs the binary of the image
//!   with `--version` (and, for `kbf`, `--version --json`) and requires it to name the commit
//!   and, for the JSON, a release build ([`crate::build_commit`]), runs the image's own user id,
//!   and runs the image with nothing mounted, which must fail: an image that starts without its
//!   settings has them from somewhere. The image is removed again.
//!
//! The SBOM is the binary's: the crates that went into it. The Ubuntu packages of the base
//! layer are not in it; the base is named by its digest in the Dockerfile.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use crate::build_commit::{check_identity, check_version, is_commit, names};
use crate::package::{Platform, open_sums, read, write_new, write_sums};
use crate::pkg_asset::{self, Mode};
use crate::pkg_install::Docker;
use crate::{Error, fetch::Sha256, sums};

/// The label an image says its commit in.
pub const REVISION_LABEL: &str = "org.opencontainers.image.revision";

/// The user id the images run as.
pub const UID: u32 = 10001;

/// The directories the orchestrator mounts settings, keys and state into: a regular file the
/// image itself holds in one of them is a setting or a key baked in.
pub const MOUNTED: [&str; 4] = [
    "etc/kbf",
    "etc/kbf-coordinator",
    "var/lib/kbf-server",
    "var/lib/kbf-coordinator",
];

/// The only environment variable an image sets: the base image's search path.
const ENV_ALLOWED: [&str; 1] = ["PATH"];

/// An image the release carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Image {
    /// `kbf --mode=frontend`.
    Frontend,
    /// `kbf-coordinator`.
    Coordinator,
}

impl Image {
    /// Every image, in the order docs/artifacts.md lists them.
    pub const ALL: [Image; 2] = [Image::Frontend, Image::Coordinator];

    /// The repository the image is tagged in (`<repository>:<commit>`).
    pub fn repository(self) -> &'static str {
        match self {
            Image::Frontend => "kbf-frontend",
            Image::Coordinator => "kbf-coordinator",
        }
    }

    /// The binary the image runs.
    pub fn binary(self) -> &'static str {
        match self {
            Image::Frontend => "kbf",
            Image::Coordinator => "kbf-coordinator",
        }
    }

    /// What names the image's assets before `-<commit>-<platform>`.
    pub fn asset_prefix(self) -> String {
        format!("{}-image", self.repository())
    }

    /// The file name of the image asset.
    pub fn file_name(self, commit: &str, platform: Platform) -> String {
        format!("{}-{commit}-{}.tar", self.asset_prefix(), platform.name())
    }

    /// The file name of the image's SBOM.
    pub fn sbom_name(self, commit: &str, platform: Platform) -> String {
        format!(
            "{}-{commit}-{}.cdx.json",
            self.asset_prefix(),
            platform.name()
        )
    }

    /// The tag the image is saved under.
    pub fn tag(self, commit: &str) -> String {
        format!("{}:{commit}", self.repository())
    }

    /// The path of the binary inside the image.
    fn binary_path(self) -> String {
        format!("usr/bin/{}", self.binary())
    }

    /// Whether the binary says what it is as JSON (`kbf --version --json`).
    fn says_json(self) -> bool {
        self == Image::Frontend
    }
}

/// The inputs of the checks of [`problems`].
pub struct Check<'a> {
    pub image: Image,
    pub platform: Platform,
    pub commit: &'a str,
    /// The binary the image was built from.
    pub binary: &'a Path,
}

/// The regular files of the tar `bytes`, by path.
fn outer_files(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut files = BTreeMap::new();
    let mut archive = tar::Archive::new(bytes);
    // A fresh archive lists its entries.
    for entry in archive.entries().expect("a fresh archive") {
        let mut entry = entry.map_err(|e| format!("not a tar: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = trimmed(&String::from_utf8_lossy(&entry.path_bytes()));
        let held = body(&mut entry, &path)?;
        files.insert(path, held);
    }
    Ok(files)
}

/// The bytes of the tar entry `path`, which `entry` reads.
fn body(entry: &mut impl Read, path: &str) -> Result<Vec<u8>, String> {
    let mut held = Vec::new();
    entry
        .read_to_end(&mut held)
        .map_err(|e| format!("{path}: {e}"))?;
    Ok(held)
}

/// `path` without a leading `./` and a trailing `/`.
fn trimmed(path: &str) -> String {
    path.trim_start_matches("./")
        .trim_end_matches('/')
        .to_owned()
}

/// A regular file of an image's filesystem: its mode, and its bytes when asked for.
type Held = (u32, Option<Vec<u8>>);

/// Applies the layer `layer` (a tar, gzip-compressed or not) to `files`, the regular files
/// of the filesystem so far: whiteouts delete, a path written again replaces what it was.
/// The bytes are kept of `keep` only.
fn apply_layer(files: &mut BTreeMap<String, Held>, layer: &[u8], keep: &str) -> Result<(), String> {
    let mut plain = Vec::new();
    let bytes = if layer.starts_with(&[0x1f, 0x8b]) {
        flate2::read::GzDecoder::new(layer)
            .read_to_end(&mut plain)
            .map_err(|e| format!("a layer is not a gzip stream: {e}"))?;
        &plain[..]
    } else {
        layer
    };
    let mut archive = tar::Archive::new(bytes);
    // A fresh archive lists its entries.
    for entry in archive.entries().expect("a fresh archive") {
        let mut entry = entry.map_err(|e| format!("a layer is not a tar: {e}"))?;
        let path = trimmed(&String::from_utf8_lossy(&entry.path_bytes()));
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        // Without a directory (a path at the root), the slash is trimmed away again.
        let below = |prefix: &str| format!("{prefix}/").trim_start_matches('/').to_owned();
        if name == ".wh..wh..opq" {
            let prefix = below(dir);
            files.retain(|k, _| !k.starts_with(&prefix));
        } else if let Some(target) = name.strip_prefix(".wh.") {
            let gone = format!("{dir}/{target}").trim_start_matches('/').to_owned();
            let inside = below(&gone);
            files.retain(|k, _| *k != gone && !k.starts_with(&inside));
        } else {
            files.remove(&path);
            if entry.header().entry_type().is_file() {
                // A mode that is not a number is no mode: it fails the check on the mode.
                let mode = entry.header().mode().unwrap_or(0);
                let held = if path == keep {
                    Some(body(&mut entry, &path)?)
                } else {
                    None
                };
                files.insert(path, (mode, held));
            }
        }
    }
    Ok(())
}

/// The text values of the JSON array `value`, or none when it is not an array of text.
fn texts(value: Option<&serde_json::Value>) -> Option<Vec<&str>> {
    value?.as_array()?.iter().map(|v| v.as_str()).collect()
}

/// What is wrong with `archive` (the bytes of a `docker save` tar) as the image `c` names:
/// one line per problem, none when it meets every check above.
///
/// # Errors
/// The binary `c` names cannot be read.
pub fn problems(archive: &[u8], c: &Check) -> Result<Vec<String>, Error> {
    let want = read(c.binary)?;
    let mut problems = Vec::new();
    let outer = match outer_files(archive) {
        Ok(outer) => outer,
        Err(e) => return Ok(vec![e]),
    };
    let manifest: Option<serde_json::Value> = outer
        .get("manifest.json")
        .and_then(|m| serde_json::from_slice(m).ok());
    let images = manifest.as_ref().and_then(|m| m.as_array());
    let [image] = images.map_or(&[][..], Vec::as_slice) else {
        return Ok(vec![format!(
            "manifest.json lists {} images, not exactly one",
            images.map_or(0, Vec::len)
        )]);
    };
    let tags = texts(image.get("RepoTags")).unwrap_or_default();
    let tag = c.image.tag(c.commit);
    if tags != [tag.as_str()] {
        problems.push(format!("is tagged {tags:?}, not only {tag:?}"));
    }
    let config: Option<serde_json::Value> = image
        .get("Config")
        .and_then(|p| p.as_str())
        .and_then(|p| outer.get(&trimmed(p)))
        .and_then(|c| serde_json::from_slice(c).ok());
    let Some(config) = config else {
        problems.push("has no image configuration that manifest.json names".to_owned());
        return Ok(problems);
    };
    config_problems(&config, c, &mut problems);
    let layers = texts(image.get("Layers")).unwrap_or_default();
    let mut files = BTreeMap::new();
    let binary_path = c.image.binary_path();
    for layer in &layers {
        let Some(bytes) = outer.get(&trimmed(layer)) else {
            problems.push(format!("layer {layer} is not in the archive"));
            continue;
        };
        if let Err(e) = apply_layer(&mut files, bytes, &binary_path) {
            problems.push(e);
        }
    }
    match files.get(&binary_path) {
        None => problems.push(format!("holds no {binary_path}")),
        Some((mode, held)) => {
            if mode & 0o111 != 0o111 || mode & 0o022 != 0 {
                problems.push(format!(
                    "{binary_path} has mode {mode:04o}, not executable by all and writable by its owner only"
                ));
            }
            if held.as_deref() != Some(&want[..]) {
                problems.push(format!(
                    "{binary_path} is not the bytes of {}, the binary the build identified",
                    c.binary.display()
                ));
            }
        }
    }
    for path in files.keys() {
        if MOUNTED
            .iter()
            .any(|dir| path.starts_with(&format!("{dir}/")))
        {
            problems.push(format!(
                "holds the file {path}, in a directory the orchestrator mounts: a setting, key or state baked into the image"
            ));
        }
    }
    Ok(problems)
}

/// The problems with the image configuration `config`.
fn config_problems(config: &serde_json::Value, c: &Check, problems: &mut Vec<String>) {
    fn text(v: Option<&serde_json::Value>) -> &str {
        v.and_then(|v| v.as_str()).unwrap_or("(none)")
    }
    let arch = if c.platform == Platform::LinuxX86_64 {
        "amd64"
    } else {
        "arm64"
    };
    for (what, got, want) in [
        ("os", text(config.get("os")), "linux"),
        ("architecture", text(config.get("architecture")), arch),
    ] {
        if got != want {
            problems.push(format!("says {what} {got:?}, not {want:?}"));
        }
    }
    let inner = config.get("config");
    let user = text(inner.and_then(|i| i.get("User")));
    let uid = user.split(':').next().and_then(|u| u.parse::<u32>().ok());
    if uid != Some(UID) {
        problems.push(format!("runs as user {user:?}, not the uid {UID}"));
    }
    let entrypoint = texts(inner.and_then(|i| i.get("Entrypoint")));
    let binary = format!("/{}", c.image.binary_path());
    if entrypoint != Some(vec![binary.as_str()]) {
        problems.push(format!(
            "has the entrypoint {entrypoint:?}, not [{binary:?}]"
        ));
    }
    for var in texts(inner.and_then(|i| i.get("Env"))).unwrap_or_default() {
        let name = var.split('=').next().unwrap_or_default();
        if !ENV_ALLOWED.contains(&name) {
            problems.push(format!(
                "sets the environment variable {name}: settings are files, flags and mounts"
            ));
        }
    }
    let label = text(
        inner
            .and_then(|i| i.get("Labels"))
            .and_then(|l| l.get(REVISION_LABEL)),
    );
    if label != c.commit {
        problems.push(format!(
            "says {REVISION_LABEL} {label:?}, not the commit {:?}",
            c.commit
        ));
    }
}

/// What is wrong with `sbom` as the SBOM of an image: empty, or not a CycloneDX document.
fn sbom_problem(sbom: &[u8]) -> Option<String> {
    let value: Option<serde_json::Value> = serde_json::from_slice(sbom).ok();
    let format = value
        .as_ref()
        .and_then(|v| v.get("bomFormat"))
        .and_then(|f| f.as_str());
    (format != Some("CycloneDX")).then(|| "is not a CycloneDX JSON document".to_owned())
}

/// The `image-asset` verb: the lines to print.
///
/// # Errors
/// The commit is not one, the platform is not Linux, the archive is refused, or a file
/// cannot be read or written.
pub fn asset(archive: &Path, sbom: &Path, out: &Path, c: &Check) -> Result<Vec<String>, Error> {
    let sbom_bytes = read(sbom)?;
    if let Some(why) = sbom_problem(&sbom_bytes) {
        return Err(Error::Failed(format!("the SBOM {} {why}", sbom.display())));
    }
    let name = c.image.file_name(c.commit, c.platform);
    let sbom_name = c.image.sbom_name(c.commit, c.platform);
    let said = format!(
        "the {} image of the {} in {}",
        c.platform.name(),
        c.image.binary(),
        c.binary.display()
    );
    let mut printed = pkg_asset::publish(
        archive,
        (c.commit, c.platform),
        &name,
        &Mode::Add(out),
        |bytes| problems(bytes, c),
        &said,
    )?;
    let (path, mut lines) = open_sums(out, c.platform)?;
    if lines.iter().any(|l| l.name == sbom_name) {
        return Err(Error::Failed(format!(
            "{} already lists {sbom_name}",
            path.display()
        )));
    }
    write_new(&out.join(&sbom_name), &sbom_bytes)?;
    lines.push(sums::Line {
        sha256: Sha256::of(&sbom_bytes),
        name: sbom_name,
    });
    let text = write_sums(&path, &lines)?;
    printed.extend(text.lines().map(str::to_owned));
    Ok(printed)
}

/// The inputs of [`build`].
pub struct Build<'a> {
    pub image: Image,
    /// The directory holding the image's `Dockerfile`.
    pub dockerfile_dir: &'a Path,
    /// The binary the image carries.
    pub binary: &'a Path,
    pub commit: &'a str,
    /// The directory the build context and the archive go in.
    pub out: &'a Path,
}

/// The file name of the archive `image-build` saves, in its `--out` directory.
pub fn archive_name(image: Image) -> String {
    format!("{}.tar", image.repository())
}

/// Copies `from` to `to`, with the paths in the error.
fn copy(from: &Path, to: &Path) -> Result<(), Error> {
    std::fs::copy(from, to)
        .map(drop)
        .map_err(Error::spawn(format!(
            "copy {} to {}",
            from.display(),
            to.display()
        )))
}

/// The `image-build` verb: the lines to print.
///
/// # Errors
/// The commit is not one, a file cannot be copied, the context already exists, or docker
/// fails.
pub fn build(docker: &mut Docker, b: &Build) -> Result<Vec<String>, Error> {
    if !is_commit(b.commit) {
        return Err(Error::NotACommit(b.commit.to_owned()));
    }
    let context = b.out.join(format!("{}-context", b.image.repository()));
    std::fs::create_dir_all(b.out)
        .and_then(|()| std::fs::create_dir(&context))
        .map_err(Error::spawn(format!("create {}", context.display())))?;
    copy(
        &b.dockerfile_dir.join("Dockerfile"),
        &context.join("Dockerfile"),
    )?;
    copy(b.binary, &context.join(b.image.binary()))?;
    let tag = b.image.tag(b.commit);
    let argument = format!("COMMIT={}", b.commit);
    must(
        docker,
        &[
            "build",
            "--build-arg",
            &argument,
            "--tag",
            &tag,
            &context.to_string_lossy(),
        ],
    )?;
    let archive = b.out.join(archive_name(b.image));
    let saved = must(
        docker,
        &["save", "--output", &archive.to_string_lossy(), &tag],
    );
    // The daemon keeps no copy: `image-test` loads the archive.
    let _ = docker(&["rmi", "--force", &tag]);
    saved?;
    Ok(vec![format!("built {tag} into {}", archive.display())])
}

/// What `image-test` runs.
pub struct Test<'a> {
    pub image: Image,
    pub archive: &'a Path,
    pub commit: &'a str,
}

/// `docker <args>` and its output, which must have succeeded.
fn must(docker: &mut Docker, args: &[&str]) -> Result<String, Error> {
    let out = docker(args)?;
    if out.ok {
        return Ok(out.stdout);
    }
    Err(Error::Failed(format!(
        "docker {}: {}",
        args.join(" "),
        out.stderr.trim_end()
    )))
}

/// `<binary> <args>` in a container of `tag`, without a network: what it printed.
fn in_image(docker: &mut Docker, tag: &str, program: &str, args: &[&str]) -> Result<String, Error> {
    let mut argv = vec![
        "run",
        "--rm",
        "--network",
        "none",
        "--entrypoint",
        program,
        tag,
    ];
    argv.extend(args);
    must(docker, &argv).map(|out| out.trim_end().to_owned())
}

/// The checks of `image-test` on the image already loaded as `tag`.
fn checks(docker: &mut Docker, t: &Test, tag: &str) -> Result<Vec<String>, Error> {
    let binary = t.image.binary();
    let program = format!("/{}", t.image.binary_path());
    let mut printed = Vec::new();
    let version = in_image(docker, tag, &program, &["--version"])?;
    check_version(&version, binary, t.commit, names)?;
    printed.push(format!("ok: {version}"));
    if t.image.says_json() {
        let json = in_image(docker, tag, &program, &["--version", "--json"])?;
        printed.push(format!("ok: {}", check_identity(&json, t.commit)?));
    }
    let uid = in_image(docker, tag, "/usr/bin/id", &["-u"])?;
    if uid != UID.to_string() {
        return Err(Error::Failed(format!(
            "the image runs as the user id {uid:?}, not {UID}"
        )));
    }
    printed.push(format!("ok: runs as the user id {uid}"));
    let bare = docker(&["run", "--rm", "--network", "none", tag])?;
    if bare.ok {
        return Err(Error::Failed(
            "the image started and exited 0 with nothing mounted: its settings come from somewhere but the mounts".to_owned(),
        ));
    }
    printed.push("ok: with nothing mounted it exits non-zero".to_owned());
    Ok(printed)
}

/// The `image-test` verb: the lines to print.
///
/// # Errors
/// The commit is not one, docker fails, or the image is not what the checks say.
pub fn run(docker: &mut Docker, t: &Test) -> Result<Vec<String>, Error> {
    if !is_commit(t.commit) {
        return Err(Error::NotACommit(t.commit.to_owned()));
    }
    let tag = t.image.tag(t.commit);
    let archive = t.archive.to_string_lossy();
    let loaded = must(docker, &["load", "--input", &archive])?;
    let result = if loaded.contains(&tag) {
        checks(docker, t, &tag)
    } else {
        Err(Error::Failed(format!(
            "docker load did not load {tag}: {}",
            loaded.trim_end()
        )))
    };
    // The image is removed whatever the result.
    let _ = docker(&["rmi", "--force", &tag]);
    let mut printed = vec![format!("loaded {tag} from {archive}")];
    printed.extend(result?);
    Ok(printed)
}

#[cfg(test)]
mod tests;
