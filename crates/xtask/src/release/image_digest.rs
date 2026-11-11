//! `release image-digest`: what a deploy of the published frontend image is given, read from
//! the release itself.
//!
//! A Docker Swarm deploy of the frontend (the `deploy` job of `.github/workflows/release.yml`)
//! names the image by its manifest digest and the release's download addresses of the image
//! and of its provenance bundle; the deployer downloads both, verifies the bundle against this
//! repository's `artifacts.yml` on `main`, and refuses an image whose digest is not the one
//! named.
//!
//! `release image-digest --repo <owner/repo> --tag <tag> --commit <sha> [--output <file>]
//! [--gh <path>]`:
//!
//! 1. `gh release download <tag>` fetches `SHA256SUMS`, the linux-x86_64 frontend image
//!    `kbf-frontend-image-<commit>-linux-x86_64.oci.tar` and its bundle
//!    `<image>.sigstore.json` into a directory of this process alone (removed again), so a
//!    release that lacks either file is refused here, not by the deployer;
//! 2. the image must be the file `SHA256SUMS` lists, byte for byte;
//! 3. the digest is the one manifest the image's `index.json` names, whose blob (and its
//!    config's) hashes to the digest that names it ([`crate::pkg_image_asset::manifest_digest`]).
//!
//! Only then it prints `digest=sha256:<hex>`, `image-url=` and `bundle-url=` (the
//! `https://github.com/<repo>/releases/download/<tag>/<file>` of each) and appends those lines
//! to `--output` (on Actions, `GITHUB_OUTPUT`). Any refusal prints nothing and writes nothing.
//! It runs no downloaded file. The repository and the tag are written into the addresses, so
//! each must be letters, digits, `.`, `_` and `-` only (the repository one `/` between two such
//! names).

use std::path::{Path, PathBuf};

use super::farm_version::read_capped;
use super::{append_output, gh_run};
use crate::Error;
use crate::build_commit::full_commit;
use crate::fetch::Sha256;
use crate::package::Platform;
use crate::pkg_image_asset::{Image, manifest_digest};
use crate::{bundles, sums};

/// The platform of the image deployed: the one the deploy front's machine runs.
const PLATFORM: Platform = Platform::LinuxX86_64;

/// The largest image read (the frontend image is tens of MiB).
const IMAGE_CAP: usize = 1 << 30;

/// The largest bundle read (a Sigstore bundle is a few KiB).
const BUNDLE_CAP: usize = 1 << 20;

/// The arguments of `release image-digest`.
#[derive(clap::Args)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The release's tag.
    #[arg(long)]
    pub tag: String,
    /// The commit the release was built from, a full SHA.
    #[arg(long)]
    pub commit: String,
    /// Append the three lines to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// Whether `name` is a non-empty run of ASCII letters, digits, `.`, `_` and `-`, and not `.`
/// or `..`: a path segment of a download address that needs no escaping.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Refuses a repository that is not `<owner>/<name>` of plain names, and a tag that is not
/// plain, before anything is downloaded.
fn check_names(repo: &str, tag: &str) -> Result<(), Error> {
    let repo_ok = repo
        .split_once('/')
        .is_some_and(|(owner, name)| plain(owner) && plain(name));
    if !repo_ok {
        return Err(Error::Usage(format!(
            "--repo {repo:?} is not <owner>/<name> of letters, digits, '.', '_' and '-'"
        )));
    }
    if !plain(tag) {
        return Err(Error::Usage(format!(
            "--tag {tag:?} is not letters, digits, '.', '_' and '-'"
        )));
    }
    Ok(())
}

/// The manifest digest of the image `bytes`, which `sums_text` must list for `name`.
///
/// # Errors
/// [`Error::Failed`] when the sums do not list these bytes exactly once, or the bytes are not
/// an image layout with one manifest whose blobs match their digests.
pub fn derive(bytes: &[u8], name: &str, sums_text: &str) -> Result<String, Error> {
    let lines = sums::parse(sums_text)
        .map_err(|p| Error::Failed(format!("SHA256SUMS: {}", p.join("; "))))?;
    let listed: Vec<&sums::Line> = lines.iter().filter(|l| l.name == name).collect();
    if listed.len() != 1 || listed[0].sha256 != Sha256::of(bytes) {
        return Err(Error::Failed(format!(
            "{name} is not the file SHA256SUMS lists (listed {} time(s))",
            listed.len()
        )));
    }
    manifest_digest(bytes).map_err(|e| Error::Failed(format!("{name} {e}")))
}

/// The `release image-digest` verb: the lines to print.
pub fn run(a: &Args) -> Result<Vec<String>, Error> {
    full_commit("--commit", &a.commit)?;
    check_names(&a.repo, &a.tag)?;
    let image = Image::Frontend.file_name(&a.commit, PLATFORM);
    let bundle = bundles::name_of(&image);
    let dir = std::env::temp_dir().join(format!("xtask-image-digest-{}", std::process::id()));
    std::fs::create_dir(&dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    let found = download_and_derive(a, &image, &bundle, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    let base = format!("https://github.com/{}/releases/download/{}", a.repo, a.tag);
    let lines = vec![
        format!("digest={}", found?),
        format!("image-url={base}/{image}"),
        format!("bundle-url={base}/{bundle}"),
    ];
    if let Some(output) = &a.output {
        append_output(output, &lines)?;
    }
    Ok(lines)
}

fn download_and_derive(a: &Args, image: &str, bundle: &str, dir: &Path) -> Result<String, Error> {
    let dir_text = dir.to_string_lossy();
    gh_run(
        &a.gh,
        &[
            "release",
            "download",
            &a.tag,
            "--repo",
            &a.repo,
            "--dir",
            &dir_text,
            "--pattern",
            sums::SUMS,
            "--pattern",
            image,
            "--pattern",
            bundle,
        ],
    )?;
    let listed = read_capped(&dir.join(sums::SUMS), 1 << 20)?;
    let listed = String::from_utf8(listed)
        .map_err(|_| Error::Failed("SHA256SUMS is not UTF-8".to_owned()))?;
    let bytes = read_capped(&dir.join(image), IMAGE_CAP)?;
    if read_capped(&dir.join(bundle), BUNDLE_CAP)?.is_empty() {
        return Err(Error::Failed(format!("{bundle} is empty")));
    }
    derive(&bytes, image, &listed)
}
