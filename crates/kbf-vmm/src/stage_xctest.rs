//! `image setup --sim-tests`: staging the `Build/Products` tree of an `xcodebuild
//! build-for-testing` (one `*.xctestrun` and the built apps beside it) into the guest at
//! [`simpaths::TESTS_DIR`], for the simulator probe's UI tests.
//!
//! The tree is walked and refused by the rules of the app bundle (nothing followed; regular
//! files, directories and links that stay inside it, or leave it only with
//! `--allow-external-links`; a fifo, socket or device refused), and must hold exactly one
//! `*.xctestrun` at its top, a regular file with a name the guest can carry. It is copied
//! file by file with each file hashed while it is copied (modes kept but group and world
//! write, setuid, setgid and sticky dropped; owners root:wheel), built under a staging name
//! and renamed into place, so the guest never sees a partial tree. The directory ends
//! holding exactly this tree: a setup that names no tests removes the earlier ones, so a
//! test run never pairs an old build with a new Xcode or app.

use std::path::{Path, PathBuf};

use kbf_guest::{sim_xctest, simpaths};
use serde_json::{Value, json};

use crate::stage::{
    Env, Limits, Node, StageError, TreePlan, apply_modes, chown, copy_file_hashed, discard,
    ensure_dir, link, mkdir, on_volume, plan_tree, remove_existing, rename,
};
use crate::xattr;

const MAX_NAME_BYTES: usize = kbf_guest::sim::MAX_STAGED_NAME_BYTES;

/// Why a tests tree was refused (beyond what the walk refuses).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TestsError {
    #[error("{0} is not a directory (--sim-tests takes the Build/Products directory)")]
    NotDirectory(PathBuf),
    #[error("{0} holds no *.xctestrun file (was it built with `xcodebuild build-for-testing`?)")]
    NoXctestrun(PathBuf),
    #[error("{dir} holds two *.xctestrun files ({a} and {b}); it must hold exactly one")]
    TwoXctestruns { dir: PathBuf, a: String, b: String },
    #[error("{0} is not a regular file")]
    NotRegular(PathBuf),
    #[error(
        "{0}: the .xctestrun name must be made of letters, digits, '.', '_' or '-' (no '..'), end in .xctestrun and be at most {MAX_NAME_BYTES} bytes"
    )]
    BadName(PathBuf),
}

/// The tests as they will be copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestsPlan {
    /// The `.xctestrun` file name, also its name in the guest.
    pub xctestrun: String,
    pub tree: TreePlan,
}

/// Reads the products directory `dir` into a plan.
///
/// # Errors
/// `dir` is a link or not a directory, something in it is refused, or it does not hold
/// exactly one `*.xctestrun` (a regular file with a name the guest can carry).
pub fn plan_tests(dir: &Path, allow_external_links: bool) -> Result<TestsPlan, StageError> {
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| StageError::Io(format!("stat {}: {e}", dir.display())))?;
    if meta.file_type().is_symlink() {
        return Err(StageError::Symlink(dir.to_owned()));
    }
    if !meta.is_dir() {
        return Err(TestsError::NotDirectory(dir.to_owned()).into());
    }
    let tree = plan_tree(
        dir,
        simpaths::TESTS_DIR,
        allow_external_links,
        &Limits::DEFAULT,
    )?;
    let mut runs = tree.nodes.iter().filter(|n| {
        let rel = n.rel();
        rel.components().count() == 1
            && rel
                .to_str()
                .is_some_and(|s| s.ends_with(sim_xctest::XCTESTRUN_SUFFIX))
    });
    let first = runs
        .next()
        .ok_or_else(|| TestsError::NoXctestrun(dir.to_owned()))?;
    if let Some(second) = runs.next() {
        return Err(TestsError::TwoXctestruns {
            dir: dir.to_owned(),
            a: first.rel().display().to_string(),
            b: second.rel().display().to_string(),
        }
        .into());
    }
    let at = dir.join(first.rel());
    if !matches!(first, Node::File { .. }) {
        return Err(TestsError::NotRegular(at).into());
    }
    let xctestrun = first
        .rel()
        .to_str()
        .filter(|n| kbf_guest::sim::name_ok(n, sim_xctest::XCTESTRUN_SUFFIX))
        .ok_or(TestsError::BadName(at))?
        .to_owned();
    Ok(TestsPlan { xctestrun, tree })
}

/// Builds the plan's tree at `staging` (which does not exist), hashing each file as it is
/// copied, and hands it to root. Returns the manifest, one entry per file.
fn build(t: &TestsPlan, staging: &Path, env: &Env<'_>) -> Result<Vec<Value>, String> {
    let tree = &t.tree;
    mkdir(staging, 0o700)?;
    let mut dirs = vec![(staging.to_owned(), 0o755)];
    let mut files = Vec::new();
    for node in &tree.nodes {
        let at = staging.join(node.rel());
        match node {
            Node::Dir { mode, .. } => {
                mkdir(&at, 0o700)?;
                dirs.push((at, *mode));
            }
            Node::File {
                rel,
                src,
                mode,
                bytes,
            } => {
                let sha256 = copy_file_hashed(src, &at, *mode, *bytes)?;
                files.push(json!({"path": rel, "bytes": bytes, "sha256": sha256}));
            }
            Node::Link { target, .. } => link(target, &at)?,
        }
    }
    // The attributes of the tree, of each directory and of each link (a file's were copied
    // with its bytes), once everything exists.
    std::iter::once((tree.source.clone(), staging.to_owned()))
        .chain(
            tree.nodes
                .iter()
                .filter(|n| !matches!(n, Node::File { .. }))
                .map(|n| (tree.source.join(n.rel()), staging.join(n.rel()))),
        )
        .try_for_each(|(from, to)| xattr::copy_nofollow(&from, &to))?;
    apply_modes(dirs)?;
    chown(env, staging)?;
    tree.nodes
        .iter()
        .try_for_each(|n| chown(env, &staging.join(n.rel())))?;
    Ok(files)
}

/// Makes the guest's tests directory hold exactly `tests` (none: remove it). Returns the
/// entry of the report and of `staged.json`, `null` for none.
///
/// # Errors
/// As the other stages: a link or file where a directory must be, what cannot be removed, a
/// source that changed since the plan, an I/O error. A tree that failed part way is removed.
pub fn stage_tests(
    volume: &Path,
    tests: Option<&TestsPlan>,
    env: &Env<'_>,
) -> Result<Value, String> {
    let rel = Path::new(simpaths::TESTS_DIR.trim_start_matches('/'));
    let parent = rel.parent().unwrap_or(Path::new(""));
    ensure_dir(volume, parent, env)?;
    let dest = on_volume(volume, simpaths::TESTS_DIR);
    let staging = volume.join(parent).join(".tests.kbf-staging");
    remove_existing(env, &staging)?;
    let Some(t) = tests else {
        remove_existing(env, &dest)?;
        return Ok(Value::Null);
    };
    let files = build(t, &staging, env).map_err(|e| discard(env, &staging, e))?;
    // The finished tree replaces the earlier one.
    remove_existing(env, &dest)?;
    rename(&staging, &dest)?;
    let count = |f: fn(&Node) -> bool| t.tree.nodes.iter().filter(|n| f(n)).count();
    Ok(json!({
        "dest": simpaths::TESTS_DIR,
        "xctestrun": t.xctestrun,
        "bytes": t.tree.bytes,
        "files": files,
        "dirs": count(|n| matches!(n, Node::Dir { .. })),
        "symlinks": count(|n| matches!(n, Node::Link { .. })),
        "external_links": t.tree.external_links.iter()
            .map(|(p, t)| json!({"path": p, "target": t}))
            .collect::<Vec<_>>(),
    }))
}

impl From<TestsError> for StageError {
    fn from(e: TestsError) -> Self {
        Self::Tests(e)
    }
}
