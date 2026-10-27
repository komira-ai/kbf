//! The simulator runtimes of `image setup --sim-runtime`: the plan of each disk image and
//! exported bundle, and writing them into the guest's runtime directory (see
//! [`crate::stage`] for the rules both share).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use kbf_guest::simpaths;
use serde_json::{Value, json};

use crate::image;
use crate::stage::{
    Env, Limits, MAX_RUNTIMES, Node, StageError, apply_modes, chmod, chown, clear_except,
    copy_file_hashed, discard, ensure_dir, io_err, link, mkdir, plan_tree, remove_existing, rename,
};

/// What a staged runtime is, from its name's suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    /// A disk image, one file (`<name>.dmg`).
    Dmg,
    /// An exported platform, a directory (`<name>.exportedBundle`).
    Bundle,
}

impl RuntimeKind {
    /// The word the report and `staged.json` use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dmg => "dmg",
            Self::Bundle => "bundle",
        }
    }
}

/// One runtime (a disk image or an exported bundle) as it will be copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePlan {
    /// The file or directory name, which is also the name in the guest.
    pub name: String,
    pub kind: RuntimeKind,
    pub src: PathBuf,
    /// The size of the image, or the sum of the file sizes of the bundle.
    pub bytes: u64,
    /// What a bundle holds, in the order of [`TreePlan`]; empty for an image.
    pub nodes: Vec<Node>,
}

/// A runtime's name and kind when it is `<name>.dmg` or `<name>.exportedBundle` with a name
/// of letters, digits, `.`, `_` and `-` only (the name becomes a path in the guest).
fn runtime_name(path: &Path) -> Option<(String, RuntimeKind)> {
    let name = path.file_name()?.to_str()?;
    let kind = if kbf_guest::sim::name_ok(name, ".dmg") {
        RuntimeKind::Dmg
    } else if kbf_guest::sim::name_ok(name, kbf_guest::sim::BUNDLE_SUFFIX) {
        RuntimeKind::Bundle
    } else {
        return None;
    };
    Some((name.to_owned(), kind))
}

/// Reads the runtimes at `paths` (disk images and exported bundles) into plans.
///
/// # Errors
/// Too many, a path that is a link, a `.dmg` that is not a regular file, a bundle that is not
/// a directory or holds something the Xcode.app walk refuses, a name the guest path cannot
/// carry, or two of one name.
pub fn plan_runtimes(paths: &[PathBuf]) -> Result<Vec<RuntimePlan>, StageError> {
    if paths.len() > MAX_RUNTIMES {
        return Err(StageError::TooManyRuntimes);
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for p in paths {
        let (name, kind) = runtime_name(p).ok_or_else(|| StageError::BadRuntimeName(p.clone()))?;
        let meta = std::fs::symlink_metadata(p).map_err(|e| io_err("stat", p, &e))?;
        if meta.file_type().is_symlink() {
            return Err(StageError::Symlink(p.clone()));
        }
        let (bytes, nodes) = match kind {
            RuntimeKind::Dmg if !meta.is_file() => {
                return Err(StageError::NotRegular(p.clone()));
            }
            RuntimeKind::Dmg => (meta.len(), Vec::new()),
            RuntimeKind::Bundle if !meta.is_dir() => {
                return Err(StageError::NotDirectory(p.clone()));
            }
            RuntimeKind::Bundle => {
                let tree = plan_tree(
                    p,
                    &format!("{}/{name}", simpaths::RUNTIME_DIR),
                    false,
                    &Limits::DEFAULT,
                )?;
                (tree.bytes, tree.nodes)
            }
        };
        if !seen.insert(name.clone()) {
            return Err(StageError::DuplicateRuntime(name));
        }
        out.push(RuntimePlan {
            name,
            kind,
            src: p.clone(),
            bytes,
            nodes,
        });
    }
    Ok(out)
}

/// Builds the bundle `r` at `dst` (which does not exist): directories 0755, files 0644,
/// each file hashed while it is copied, everything owned by root. Returns the manifest, one
/// entry per file.
fn stage_bundle(r: &RuntimePlan, dst: &Path, env: &Env<'_>) -> Result<Vec<Value>, String> {
    mkdir(dst, 0o700)?;
    let mut dirs = vec![(dst.to_owned(), 0o755)];
    let mut files = Vec::new();
    for node in &r.nodes {
        let at = dst.join(node.rel());
        match node {
            Node::Dir { .. } => {
                mkdir(&at, 0o700)?;
                dirs.push((at, 0o755));
            }
            Node::File {
                rel, src, bytes, ..
            } => {
                let sha256 = copy_file_hashed(src, &at, 0o644, *bytes)?;
                files.push(json!({"path": rel, "bytes": bytes, "sha256": sha256}));
            }
            Node::Link { target, .. } => link(target, &at)?,
        }
    }
    apply_modes(dirs)?;
    chown(env, dst)?;
    r.nodes
        .iter()
        .try_for_each(|n| chown(env, &dst.join(n.rel())))?;
    Ok(files)
}

/// Builds the runtime `r` at `dst` (which does not exist) and returns its report entry.
fn stage_runtime(r: &RuntimePlan, dst: &Path, env: &Env<'_>) -> Result<Value, String> {
    let dest = format!("{}/{}", simpaths::RUNTIME_DIR, r.name);
    if r.kind == RuntimeKind::Bundle {
        let files = stage_bundle(r, dst, env)?;
        return Ok(json!({
            "name": r.name,
            "kind": r.kind.as_str(),
            "bytes": r.bytes,
            "files": files,
            "dest": dest,
        }));
    }
    let sha256 = image::stage_exact(&r.src, dst, r.bytes).map_err(|e| e.to_string())?;
    chmod(dst, 0o644)?;
    chown(env, dst)?;
    Ok(json!({
        "name": r.name,
        "kind": r.kind.as_str(),
        "bytes": r.bytes,
        "sha256": sha256,
        "dest": dest,
    }))
}

/// Builds each runtime in `staging` (beside the runtime directory, never inside it) and
/// renames it into `dir` once complete, so a name in `dir` is always a whole runtime.
fn place_runtimes(
    dir: &Path,
    staging: &Path,
    runtimes: &[RuntimePlan],
    env: &Env<'_>,
) -> Result<Value, String> {
    // The directory holds exactly this setup's runtimes: the guest installs what it finds.
    clear_except(env, dir, &|p| runtimes.iter().any(|r| p.ends_with(&r.name)))?;
    remove_existing(env, staging)?;
    mkdir(staging, 0o700)?;
    let mut staged = Vec::new();
    for r in runtimes {
        let built = staging.join(&r.name);
        staged.push(stage_runtime(r, &built, env)?);
        let dst = dir.join(&r.name);
        remove_existing(env, &dst)?;
        rename(&built, &dst)?;
    }
    remove_existing(env, staging)?;
    Ok(Value::Array(staged))
}

pub(crate) fn stage_runtimes(
    volume: &Path,
    runtimes: &[RuntimePlan],
    env: &Env<'_>,
) -> Result<Value, String> {
    let rel = Path::new(simpaths::RUNTIME_DIR.trim_start_matches('/'));
    let dir = volume.join(rel);
    ensure_dir(volume, rel, env)?;
    let staging = volume
        .join(rel.parent().unwrap_or(Path::new("")))
        .join(".runtimes.kbf-staging");
    // Whatever fails, the runtime being built is removed: the guest never sees a partial one.
    place_runtimes(&dir, &staging, runtimes, env).map_err(|e| discard(env, &staging, e))
}
