//! `base-tree --image <oci.tar> --platform <platform> --commit <sha> --binary <kbf-task> --out
//! <dir>`: the kbf base tree of a release as the asset
//! `kbf-base-tree-<commit>-<platform>.tree.tar` (docs/design/actions-as-jobs.md, 3.3;
//! docs/artifacts.md, "The kbf base tree bundle").
//!
//! The task image is opened like `pkg-image-asset` opens it ([`crate::pkg_image_asset`]) and
//! refused unless its layers begin with those of the pinned base, in order ([`Base`]). Those
//! layers, each checked against its digest, are applied in order onto one tree ([`layers`]: the
//! whiteouts, hard links as separate entries, the refusals), the packages and paths of
//! [`removed`] are left out as dpkg would leave them out, a set-uid or set-gid file still there
//! is refused, `--binary`
//! is added as the executable `/.kbf-tools/kbf-task` and nothing else is in `/.kbf-tools`, the
//! tree is written as tierstore manifests ([`kbf_tree_bundle::Dir::build`]), and the bundle is
//! written to
//! `<out>/<file name>` ([`kbf_tree_bundle::bundle`]): the facts, every file's bytes in the
//! order of the tree, then the manifests, the root's last. The layers above the base (the
//! image's own `kbf-task` layer) are not read: the tree takes `--binary` instead.
//!
//! A tree has one owner and the modes 0644 and 0755 only: a file becomes 0755 if any execute bit
//! is set and else 0644, a directory 0755, and every path whose mode was another is listed in
//! the facts, with the mode it had and the one it has. An image that holds what a tree cannot
//! hold, or what the kbf tree makes itself, is refused by path and kind and nothing is written;
//! so is one whose packages cannot all be left out consistently, and one with a set-uid or
//! set-gid file left once they are. The lines printed end with each package and path left out,
//! and why.
//!
//! Once written, the bundle is read back ([`kbf_tree_bundle::read`]: each object is checked
//! against its name) and held to what was written, and only then listed in
//! `<out>/SHA256SUMS.<platform>` ([`crate::sums`]); a bundle that is already listed is refused.
//! The release attests it with the rest of `SHA256SUMS`.

mod layers;
mod removed;

use removed::{EXCLUDED, Excluded, REMOVED, Removed};

use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use kbf_tree_bundle::{Built, BundleError, Facts, Item, Normalised, Writer, file_name};

use crate::fetch::Sha256;
use crate::package::{Platform, open_sums, read, write_sums};
use crate::pkg_image_asset::{self, Base, Image};
use crate::{Error, build_commit, sums};

/// The flags of the verb.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// The task image `docker buildx build --output type=docker,oci-mediatypes=true` wrote.
    #[arg(long)]
    pub image: PathBuf,
    /// The platform it was built for (`linux-x86_64`, `linux-arm64`).
    #[arg(long, value_enum)]
    pub platform: Platform,
    /// The commit of the release (`$GITHUB_SHA`), in full.
    #[arg(long)]
    pub commit: String,
    /// The built static `kbf-task`, which the tree holds as `/.kbf-tools/kbf-task`.
    #[arg(long)]
    pub binary: PathBuf,
    /// The directory the bundle and its line of `SHA256SUMS.<platform>` go in.
    #[arg(long)]
    pub out: PathBuf,
}

/// The commit of the release the file name `name` is a base tree bundle of
/// (`kbf-base-tree-<commit>-<platform>.tree.tar`, for a Linux platform).
fn bundle_commit(name: &str) -> Option<&str> {
    let stem = name
        .strip_prefix(kbf_tree_bundle::bundle::NAME)?
        .strip_prefix('-')?
        .strip_suffix(kbf_tree_bundle::bundle::SUFFIX)?;
    let (commit, platform) = stem.split_once('-')?;
    let linux = matches!(
        Platform::named(platform),
        Some(Platform::LinuxX86_64 | Platform::LinuxArm64)
    );
    (linux && build_commit::is_commit(commit)).then_some(commit)
}

/// Whether `name` is the file name of a base tree bundle: `kbf-base-tree-<commit>-<platform>.tree.tar`
/// for a Linux platform.
pub fn is_bundle(name: &str) -> bool {
    bundle_commit(name).is_some()
}

/// Whether `name` is the file name of the base tree bundle of `commit`.
pub fn is_bundle_of(name: &str, commit: &str) -> bool {
    bundle_commit(name) == Some(commit)
}

/// The base tree made from an image, not yet written.
struct Tree {
    built: Built,
    files: Vec<Rc<[u8]>>,
    normalised: Vec<Normalised>,
}

/// The `base-tree` verb: the lines to print.
///
/// # Errors
/// The platform has no base image, the commit is not a full one, a file cannot be read, the
/// image is refused ([`tree_of`]) or the bundle cannot be written, checked and listed.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let Some(base) = Image::Task.base(args.platform)? else {
        return Err(Error::Refused(vec![format!(
            "{}: the kbf base tree is built for the Linux platforms only",
            args.platform.name()
        )]));
    };
    run_with(args, &base, REMOVED, EXCLUDED)
}

/// [`run`] on the base `base`, with the packages `removed` and the paths `excluded` left out;
/// the lines printed end with one for each package and path left out, with its reason.
fn run_with(
    args: &Args,
    base: &Base,
    removed: &[Removed],
    excluded: &[Excluded],
) -> Result<Vec<String>, Error> {
    build_commit::full_commit("--commit", &args.commit)?;
    let image = read(&args.image)?;
    let binary = read(&args.binary)?;
    let packages: Vec<&str> = removed.iter().map(|r| r.package).collect();
    let paths: Vec<&str> = excluded.iter().map(|e| e.path).collect();
    let tree = tree_of(&image, base, &packages, &paths, binary)?;
    let mut lines = write(&args.out, &args.commit, args.platform, &tree)?;
    lines.extend(
        removed
            .iter()
            .map(|r| format!("left out {}: {}", r.package, r.why)),
    );
    lines.extend(
        excluded
            .iter()
            .map(|e| format!("left out {}: {}", e.path, e.why)),
    );
    Ok(lines)
}

/// What is wrong with the layers `layers` of an image as the start of a base whose layers are
/// `base`: fewer than the base's, or one that is not the base's at that place.
fn layer_problems(layers: &[String], base: &[String]) -> Vec<String> {
    if layers.len() < base.len() {
        return vec![format!(
            "the image has {} layers, fewer than the {} of its base",
            layers.len(),
            base.len()
        )];
    }
    base.iter()
        .zip(layers)
        .enumerate()
        .filter(|(_, (b, l))| b != l)
        .map(|(i, (b, l))| format!("layer {} is {l}, the base's is {b}", i + 1))
        .collect()
}

/// The tree of the first layers of `image` (the bytes of its tar), which must be those of
/// `base`, with the packages `removed` and the paths `excluded` of kept packages left out and
/// `binary` added as `/.kbf-tools/kbf-task`.
///
/// # Errors
/// [`Error::Refused`], one line per problem: the image is not an OCI layout, its layers do not
/// begin with the base's, a layer is not a tar, a layer holds what a tree cannot hold
/// ([`layers`]), a package cannot be left out consistently, a set-uid or set-gid file is left,
/// or the tree build refuses a name or a limit.
fn tree_of(
    image: &[u8],
    base: &Base,
    removed: &[&str],
    excluded: &[&str],
    binary: Vec<u8>,
) -> Result<Tree, Error> {
    let refused = |why: String| Error::Refused(vec![why]);
    let layout = pkg_image_asset::layout(image).map_err(|e| refused(format!("the image {e}")))?;
    let held = pkg_image_asset::layer_digests(&layout.manifest);
    let wrong = layer_problems(&held, &base.layers);
    if !wrong.is_empty() {
        return Err(Error::Refused(wrong));
    }
    let mut rootfs = layers::Rootfs::new();
    let mut problems = Vec::new();
    for digest in &base.layers {
        let blob = pkg_image_asset::blob(&layout.blobs, digest, "layer")
            .map_err(|e| refused(format!("the image {e}")))?;
        pkg_image_asset::plain_tar(blob)
            .and_then(|plain| rootfs.apply(&plain, &mut problems))
            .map_err(|e| refused(format!("layer {digest}: {e}")))?;
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    rootfs.remove_packages(removed, excluded, &mut problems);
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let set_id = rootfs.set_id_files();
    if !set_id.is_empty() {
        return Err(Error::Refused(set_id));
    }
    rootfs.add_tool(binary);
    let flat = rootfs.flatten();
    let built = flat
        .dir
        .build()
        .map_err(|e| refused(format!("the tree is refused: {e}")))?;
    Ok(Tree {
        built,
        files: flat.files,
        normalised: flat.normalised,
    })
}

/// The facts of `tree`.
fn facts_of(commit: &str, platform: Platform, tree: &Tree) -> Facts {
    Facts {
        commit: commit.to_owned(),
        platform: platform.name().to_owned(),
        root: tree.built.root.clone(),
        entries: tree.built.entries,
        depth: tree.built.depth as u64,
        normalised: tree.normalised.clone(),
    }
}

/// Writes the bundle of `tree` to a new file in `out`, reads it back, and lists it.
fn write(out: &Path, commit: &str, platform: Platform, tree: &Tree) -> Result<Vec<String>, Error> {
    let name = file_name(commit, platform.name());
    let (sums_path, mut lines) = open_sums(out, platform)?;
    if lines.iter().any(|l| l.name == name) {
        return Err(Error::Failed(format!(
            "{} already lists {name}",
            sums_path.display()
        )));
    }
    let facts = facts_of(commit, platform, tree);
    let path = out.join(&name);
    // A file already there is refused and left as it is.
    let file = std::fs::File::create_new(&path)
        .map_err(Error::spawn(format!("create {}", path.display())))?;
    commit_bundle(file, &path, &facts, tree)?;
    let sha256 = Sha256::of(&read(&path)?);
    lines.push(sums::Line {
        sha256,
        name: name.clone(),
    });
    write_sums(&sums_path, &lines)?;
    Ok(vec![
        format!("wrote {name}: the base tree {}", facts.root),
        format!(
            "{name}: {} entries, depth {}, {} paths with a mode normalised",
            facts.entries,
            facts.depth,
            facts.normalised.len()
        ),
        format!("{sha256}  {name}"),
    ])
}

/// Writes the bundle of `tree` with `facts` to `file`, the new file at `path`, and reads it
/// back; a bundle that is not what was meant is removed, not left to be summed.
fn commit_bundle(
    file: std::fs::File,
    path: &Path,
    facts: &Facts,
    tree: &Tree,
) -> Result<(), Error> {
    let checked =
        emit(file, path, facts, tree).and_then(|()| read_back(path, facts, &objects_of(tree)));
    if checked.is_err() {
        let _ = std::fs::remove_file(path);
    }
    checked
}

/// The digests of the objects the bundle of `tree` holds, in its order: each file's, in the
/// order of the tree, then each manifest's, the root's last; each once.
fn objects_of(tree: &Tree) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut digests = Vec::new();
    let files = tree.files.iter().map(|f| Sha256::of(f).to_string());
    let manifests = tree.built.manifests.iter().map(|(hex, _)| hex.clone());
    for digest in files.chain(manifests) {
        if seen.insert(digest.clone()) {
            digests.push(digest);
        }
    }
    digests
}

/// Writes the bundle of `tree` with `facts` to `file`, the new file at `path`.
fn emit(file: std::fs::File, path: &Path, facts: &Facts, tree: &Tree) -> Result<(), Error> {
    let failed = |e: BundleError| Error::Failed(format!("{}: {e}", path.display()));
    let mut writer = Writer::new(BufWriter::new(file), facts).map_err(failed)?;
    for bytes in &tree.files {
        writer.object(bytes).map_err(failed)?;
    }
    for (_, bytes) in &tree.built.manifests {
        writer.object(bytes).map_err(failed)?;
    }
    let mut out = writer.finish().map_err(failed)?;
    out.flush()
        .map_err(Error::spawn(format!("write {}", path.display())))
}

/// Reads the bundle at `path` as the frontend will, and holds it to `facts` and to `objects`,
/// the digests of what it was written with, in order.
fn read_back(path: &Path, facts: &Facts, objects: &[String]) -> Result<(), Error> {
    let bad = |why: String| Error::Failed(format!("{}: read back: {why}", path.display()));
    let file =
        std::fs::File::open(path).map_err(Error::spawn(format!("open {}", path.display())))?;
    let mut got = Vec::new();
    kbf_tree_bundle::read(BufReader::new(file), |item| match item {
        Item::Facts(read) if read != *facts => Err("the facts are not the ones written".to_owned()),
        Item::Facts(_) => Ok(()),
        Item::Object { sha256, .. } => {
            got.push(sha256);
            Ok(())
        }
    })
    .map_err(|e| bad(e.to_string()))?;
    if got != objects {
        return Err(bad(format!(
            "it holds {} objects, not the {} written, or in another order",
            got.len(),
            objects.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
