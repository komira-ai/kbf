//! `image setup --xcode --sim-runtime`: staging Xcode and the iOS simulator runtimes into a
//! guest's Data volume while the guest is off.
//!
//! The work has two halves. [`plan`] reads the host side only (Xcode.app and the runtime
//! disk images the operator names), refuses what must not be copied, and totals the bytes.
//! [`execute`] writes a plan into a mounted Data volume: it checks the free space first,
//! checks that every directory it writes through is a real directory of the volume (a
//! symbolic link planted by an earlier guest is not followed), builds Xcode.app beside its
//! final name and renames it into place (the previous copy is removed first), and replaces
//! the runtimes while hashing them (the runtime directory ends holding exactly the plan's
//! runtimes, none when the plan has none). Both
//! are plain Rust over directories, so the tests run them on a fake volume; the attach and
//! detach around [`execute`] are `offline::stage` (macOS only).
//!
//! What is copied and what is not:
//! - Xcode.app is copied as regular files, directories and symbolic links, never following
//!   a link. A link is recreated only when its target stays inside the app (relative, with
//!   the `..` components first and no more of them than the link's depth) or is an absolute
//!   path into the staged copy itself; any other link is refused unless the operator passed
//!   `--allow-external-links`, and then it is listed in the report. Hard links become
//!   separate files. A fifo, socket or device is refused. Owners become root:wheel; the
//!   setuid, setgid and sticky bits are dropped. This holds for both ways of building the
//!   copy. The file-by-file copy writes the plan's modes and opens each source without
//!   following a link, refusing one that is no longer a regular file. A tree clone copies
//!   the live tree, so it is walked again afterwards, refused unless it holds exactly the
//!   plan's entries (kinds, link targets, file sizes), and given the plan's modes. Extended
//!   attributes are not copied by the file-by-file path.
//! - A runtime is one regular file named `<name>.dmg`, copied byte for byte while its
//!   SHA-256 is computed.

use std::collections::BTreeSet;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use kbf_guest::simpaths;
use serde_json::{Value, json};

use crate::guest::remove_leftover;
use crate::image;

/// The most entries (files, directories, links) one Xcode.app may hold.
pub const MAX_ENTRIES: usize = 2_000_000;
/// The deepest directory nesting the walk follows.
pub const MAX_DEPTH: usize = 64;
/// The longest runtime file name: the guest derives the names of its result frames from it,
/// and a frame name is limited to 100 bytes.
pub const MAX_RUNTIME_NAME_BYTES: usize = 64;
/// The most runtimes one setup stages.
pub const MAX_RUNTIMES: usize = 8;
/// Free space that must remain on the volume after the copy: the guest writes to its
/// Data volume while it boots, and Xcode's first launch expands what it installs.
pub const HEADROOM_BYTES: u64 = 4 << 30;

/// The caps of the walk of a tree; tests lower them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: usize,
    pub max_depth: usize,
}

impl Limits {
    pub const DEFAULT: Self = Self {
        max_entries: MAX_ENTRIES,
        max_depth: MAX_DEPTH,
    };
}

/// Why a plan could not be made or a stage could not be done.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StageError {
    #[error("{0} is not an Xcode.app directory (it needs Contents/Developer/usr/bin/xcodebuild)")]
    NotXcode(PathBuf),
    #[error("{0} is a symbolic link; name the real path")]
    Symlink(PathBuf),
    #[error(
        "{path}: a symbolic link to {target}, which leaves the app (pass --allow-external-links to copy it as a link)"
    )]
    EscapingLink { path: PathBuf, target: PathBuf },
    #[error("{path}: a {kind}, which is not copied")]
    Special { path: PathBuf, kind: &'static str },
    #[error("{0}: more than {1} entries")]
    TooMany(PathBuf, usize),
    #[error("{0}: nested deeper than {1} directories")]
    TooDeep(PathBuf, usize),
    #[error("{0} is not a regular file")]
    NotRegular(PathBuf),
    #[error(
        "{0}: a runtime must be named <name>.dmg, at most {MAX_RUNTIME_NAME_BYTES} bytes, with name made of letters, digits, '.', '_' or '-' (no '..')"
    )]
    BadRuntimeName(PathBuf),
    #[error("two runtimes are named {0}; each --sim-runtime needs its own file name")]
    DuplicateRuntime(String),
    #[error("more than {MAX_RUNTIMES} --sim-runtime")]
    TooManyRuntimes,
    #[error("{0}")]
    Io(String),
}

/// One thing to create under Xcode.app, in an order where a directory comes before what
/// it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir {
        rel: PathBuf,
        mode: u32,
    },
    File {
        rel: PathBuf,
        src: PathBuf,
        mode: u32,
        bytes: u64,
    },
    Link {
        rel: PathBuf,
        target: PathBuf,
    },
}

impl Node {
    fn rel(&self) -> &Path {
        match self {
            Self::Dir { rel, .. } | Self::File { rel, .. } | Self::Link { rel, .. } => rel,
        }
    }
}

/// A directory tree (Xcode.app) as it will be copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreePlan {
    pub source: PathBuf,
    pub nodes: Vec<Node>,
    /// The sum of the file sizes.
    pub bytes: u64,
    /// Links that leave the app, accepted because of `--allow-external-links`.
    pub external_links: Vec<(PathBuf, PathBuf)>,
}

/// One runtime disk image as it will be copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePlan {
    /// The file name, which is also the name in the guest.
    pub name: String,
    pub src: PathBuf,
    pub bytes: u64,
}

/// Everything one `image setup` stages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub xcode: Option<TreePlan>,
    pub runtimes: Vec<RuntimePlan>,
}

impl Plan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.xcode.is_none() && self.runtimes.is_empty()
    }

    /// The bytes the copy writes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.xcode.as_ref().map_or(0, |x| x.bytes)
            + self.runtimes.iter().map(|r| r.bytes).sum::<u64>()
    }
}

fn io_err(what: &str, path: &Path, e: &io::Error) -> StageError {
    StageError::Io(format!("{what} {}: {e}", path.display()))
}

/// Whether the link target `target`, found at `depth` directories below the tree's root,
/// stays inside the tree (whose own path in the guest is `guest_root`, which an absolute
/// target may name). Only leading `..` components count, and no more than `depth` of them:
/// a `..` after a name could climb out through a link that name goes through.
pub(crate) fn stays_inside(target: &Path, depth: usize, guest_root: &str) -> bool {
    if target.has_root() {
        return target.starts_with(guest_root)
            && !target.components().any(|c| c == Component::ParentDir);
    }
    let mut up = 0usize;
    let mut named = false;
    for c in target.components() {
        match c {
            Component::ParentDir if named => return false,
            Component::ParentDir => up += 1,
            Component::Normal(_) => named = true,
            // `.`; a relative path has no root or prefix.
            _ => {}
        }
    }
    up <= depth && target.components().next().is_some()
}

/// What a directory entry is, read in one step with everything the walk needs, so that a
/// failure to read any of it is one error.
struct Entry {
    name: std::ffi::OsString,
    meta: std::fs::Metadata,
    /// Where a symbolic link points.
    target: Option<PathBuf>,
}

/// The entries of `dir`, sorted by name (so two plans of one tree are equal); nothing is
/// followed.
fn entries(dir: &Path) -> io::Result<Vec<Entry>> {
    std::fs::read_dir(dir)?
        .map(|e| {
            let e = e?;
            let meta = e.metadata()?;
            let target = if meta.file_type().is_symlink() {
                Some(std::fs::read_link(e.path())?)
            } else {
                None
            };
            Ok(Entry {
                name: e.file_name(),
                meta,
                target,
            })
        })
        .collect::<io::Result<Vec<_>>>()
        .map(|mut all| {
            all.sort_by(|a, b| a.name.cmp(&b.name));
            all
        })
}

struct Walk {
    root: PathBuf,
    /// Where the tree will be in the guest.
    guest_root: String,
    limits: Limits,
    allow_external: bool,
    nodes: Vec<Node>,
    bytes: u64,
    external: Vec<(PathBuf, PathBuf)>,
}

impl Walk {
    fn dir(&mut self, rel: &Path, depth: usize) -> Result<(), StageError> {
        if depth > self.limits.max_depth {
            return Err(StageError::TooDeep(
                self.root.join(rel),
                self.limits.max_depth,
            ));
        }
        let here = self.root.join(rel);
        for entry in entries(&here).map_err(|e| io_err("read", &here, &e))? {
            if self.nodes.len() >= self.limits.max_entries {
                return Err(StageError::TooMany(
                    self.root.clone(),
                    self.limits.max_entries,
                ));
            }
            let (child_rel, path) = (rel.join(&entry.name), here.join(&entry.name));
            let meta = &entry.meta;
            let mode = meta.permissions().mode() & 0o777;
            if let Some(target) = entry.target {
                if !stays_inside(&target, depth, &self.guest_root) {
                    if !self.allow_external {
                        return Err(StageError::EscapingLink { path, target });
                    }
                    self.external.push((child_rel.clone(), target.clone()));
                }
                self.nodes.push(Node::Link {
                    rel: child_rel,
                    target,
                });
            } else if meta.is_dir() {
                self.nodes.push(Node::Dir {
                    rel: child_rel.clone(),
                    mode,
                });
                self.dir(&child_rel, depth + 1)?;
            } else if meta.is_file() {
                self.bytes += meta.len();
                self.nodes.push(Node::File {
                    rel: child_rel,
                    src: path,
                    mode,
                    bytes: meta.len(),
                });
            } else {
                return Err(StageError::Special {
                    path,
                    kind: kind_of(meta.mode()),
                });
            }
        }
        Ok(())
    }
}

/// What kind of file the `st_mode` `mode` describes, for a refusal. The file type constants
/// are 16 bits wide on macOS and 32 on Linux, hence the conversions.
#[allow(clippy::useless_conversion)]
pub(crate) fn kind_of(mode: u32) -> &'static str {
    match mode & u32::from(libc::S_IFMT) {
        m if m == u32::from(libc::S_IFIFO) => "named pipe",
        m if m == u32::from(libc::S_IFSOCK) => "socket",
        m if m == u32::from(libc::S_IFCHR) || m == u32::from(libc::S_IFBLK) => "device",
        _ => "special file",
    }
}

/// Reads Xcode.app at `app` into a plan.
///
/// # Errors
/// `app` is a link or not an Xcode.app, or something in it is refused (see the module
/// documentation), or it is too large to walk.
pub fn plan_xcode(app: &Path, allow_external_links: bool) -> Result<TreePlan, StageError> {
    plan_xcode_within(app, allow_external_links, &Limits::DEFAULT)
}

/// [`plan_xcode`] with other caps on the walk.
///
/// # Errors
/// As [`plan_xcode`].
pub fn plan_xcode_within(
    app: &Path,
    allow_external_links: bool,
    limits: &Limits,
) -> Result<TreePlan, StageError> {
    let meta = std::fs::symlink_metadata(app).map_err(|e| io_err("stat", app, &e))?;
    if meta.file_type().is_symlink() {
        return Err(StageError::Symlink(app.to_owned()));
    }
    let tool = app.join("Contents/Developer/usr/bin/xcodebuild");
    let is_tool = std::fs::symlink_metadata(&tool).is_ok_and(|m| m.is_file());
    if !meta.is_dir() || !is_tool {
        return Err(StageError::NotXcode(app.to_owned()));
    }
    let mut walk = Walk {
        root: app.to_owned(),
        guest_root: simpaths::XCODE_APP.to_owned(),
        limits: *limits,
        allow_external: allow_external_links,
        nodes: Vec::new(),
        bytes: 0,
        external: Vec::new(),
    };
    walk.dir(Path::new(""), 0)?;
    Ok(TreePlan {
        source: app.to_owned(),
        nodes: walk.nodes,
        bytes: walk.bytes,
        external_links: walk.external,
    })
}

/// A runtime's file name when it is `<name>.dmg` with a name of letters, digits, `.`, `_`
/// and `-` only (the name becomes a path in the guest).
fn runtime_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".dmg")?;
    let ok = !stem.is_empty()
        && !stem.starts_with('.')
        && !name.contains("..")
        && name.len() <= MAX_RUNTIME_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    ok.then(|| name.to_owned())
}

/// Reads the runtime disk images at `paths` into plans.
///
/// # Errors
/// Too many, a path that is a link or not a regular file, a name the guest path cannot
/// carry, or two of one name.
pub fn plan_runtimes(paths: &[PathBuf]) -> Result<Vec<RuntimePlan>, StageError> {
    if paths.len() > MAX_RUNTIMES {
        return Err(StageError::TooManyRuntimes);
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for p in paths {
        let name = runtime_name(p).ok_or_else(|| StageError::BadRuntimeName(p.clone()))?;
        let meta = std::fs::symlink_metadata(p).map_err(|e| io_err("stat", p, &e))?;
        if meta.file_type().is_symlink() {
            return Err(StageError::Symlink(p.clone()));
        }
        if !meta.is_file() {
            return Err(StageError::NotRegular(p.clone()));
        }
        if !seen.insert(name.clone()) {
            return Err(StageError::DuplicateRuntime(name));
        }
        out.push(RuntimePlan {
            name,
            src: p.clone(),
            bytes: meta.len(),
        });
    }
    Ok(out)
}

/// What the operator asked to stage.
///
/// # Errors
/// As [`plan_xcode`] and [`plan_runtimes`].
pub fn plan(
    xcode: Option<&Path>,
    runtimes: &[PathBuf],
    allow_external_links: bool,
) -> Result<Plan, StageError> {
    Ok(Plan {
        xcode: xcode
            .map(|a| plan_xcode(a, allow_external_links))
            .transpose()?,
        runtimes: plan_runtimes(runtimes)?,
    })
}

/// What [`execute`] needs from the host that a test replaces.
pub struct Env<'a> {
    /// The bytes available to a writer on the volume at the path.
    pub free_bytes: &'a dyn Fn(&Path) -> io::Result<u64>,
    /// A copy-on-write copy of a whole tree to a path that does not exist yet.
    pub clone_tree: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
    /// Makes the path (a link itself, never what it points to) owned by root:wheel.
    pub chown_root: &'a dyn Fn(&Path) -> io::Result<()>,
}

/// The guest-absolute path `abs` as a path under the volume root.
fn on_volume(volume: &Path, abs: &str) -> PathBuf {
    volume.join(abs.trim_start_matches('/'))
}

/// `chmod` of `path` to `mode`.
pub(crate) fn chmod(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

/// Hands `path` to root through the environment.
pub(crate) fn chown(env: &Env<'_>, path: &Path) -> Result<(), String> {
    (env.chown_root)(path).map_err(|e| format!("chown {}: {e}", path.display()))
}

/// Makes the new directory `path` with `mode` (as far as the umask allows).
pub(crate) fn mkdir(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(mode)
        .create(path)
        .map_err(|e| format!("create {}: {e}", path.display()))
}

/// Writes `text` to `path` (any file there is replaced) with `mode`.
pub(crate) fn write_text(path: &Path, text: &str, mode: u32) -> Result<(), String> {
    remove_leftover(path);
    std::fs::write(path, text)
        .and_then(|()| std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)))
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// The size of the file at `path`.
pub(crate) fn file_len(path: &Path) -> Result<u64, String> {
    std::fs::metadata(path)
        .map(|m| m.size())
        .map_err(|e| format!("stat {}: {e}", path.display()))
}

/// Makes sure every directory on the way to `rel` (a path under `volume`) is a real
/// directory, creating the missing ones (root, 0755). A link or a file in the way is an
/// error.
pub(crate) fn ensure_dir(volume: &Path, rel: &Path, env: &Env<'_>) -> Result<(), String> {
    let mut at = volume.to_owned();
    for c in rel.components() {
        at.push(c);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "{} on the guest volume is not a plain directory (a link or a file); refusing to write through it",
                    at.display()
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                mkdir(&at, 0o755)?;
                chown(env, &at)?;
            }
            Err(e) => return Err(format!("stat {}: {e}", at.display())),
        }
    }
    Ok(())
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// Refuses when the copy plus [`HEADROOM_BYTES`] does not fit on the volume.
fn check_space(volume: &Path, plan: &Plan, env: &Env<'_>) -> Result<(u64, u64), String> {
    let free = (env.free_bytes)(volume)
        .map_err(|e| format!("read the free space of {}: {e}", volume.display()))?;
    let need = plan.bytes();
    if free < need.saturating_add(HEADROOM_BYTES) {
        return Err(format!(
            "not enough room on the guest's Data volume: staging needs {} (Xcode {}, runtimes {}) plus {} headroom, and {} is free; rebuild the base with a larger --disk-gib",
            gib(need),
            gib(plan.xcode.as_ref().map_or(0, |x| x.bytes)),
            gib(plan.runtimes.iter().map(|r| r.bytes).sum()),
            gib(HEADROOM_BYTES),
            gib(free)
        ));
    }
    Ok((free, need))
}

/// Copies the file `src` to the new file `dst` with `mode`, and checks that `bytes` were
/// copied (the file is the one that was planned). `src` is opened without following a link
/// (and without blocking on a pipe) and the open file is checked to be a regular file, so a
/// file swapped for a link or another kind after the plan is refused, not read as root.
pub(crate) fn copy_file(src: &Path, dst: &Path, mode: u32, bytes: u64) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let fail =
        |e: &dyn std::fmt::Display| format!("copy {} -> {}: {e}", src.display(), dst.display());
    let mut from = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)
        .map_err(|e| fail(&e))?;
    if !from.metadata().map_err(|e| fail(&e))?.is_file() {
        return Err(format!(
            "{} is no longer the regular file that was planned",
            src.display()
        ));
    }
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)
        .map_err(|e| fail(&e))?;
    let copied = io::copy(&mut from, &mut to)
        .and_then(|n| {
            std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode)).map(|()| n)
        })
        .map_err(|e| fail(&e))?;
    if copied == bytes {
        Ok(())
    } else {
        Err(format!(
            "{} changed while it was copied ({bytes} bytes planned, {copied} copied)",
            src.display()
        ))
    }
}

/// Makes `dst` a symbolic link to `target`.
pub(crate) fn link(target: &Path, dst: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, dst).map_err(|e| format!("link {}: {e}", dst.display()))
}

/// Sets each directory's final mode, innermost first: a read-only directory would have
/// refused its own contents.
pub(crate) fn apply_modes(dirs: Vec<(PathBuf, u32)>) -> Result<(), String> {
    dirs.into_iter()
        .rev()
        .try_for_each(|(dir, mode)| chmod(&dir, mode))
}

/// What a node is, without where its bytes come from: for comparing a tree on the volume
/// with the plan.
fn shape(n: &Node) -> (&Path, u8, Option<&Path>, u64) {
    match n {
        Node::Dir { rel, .. } => (rel, 0, None, 0),
        Node::File { rel, bytes, .. } => (rel, 1, None, *bytes),
        Node::Link { rel, target } => (rel, 2, Some(target), 0),
    }
}

/// Holds a cloned tree to the plan. The clone copies the live host tree, not the plan, so
/// the tree at `staging` is walked again (nothing followed): it must hold exactly the plan's
/// entries (same kinds, link targets and file sizes; so no link, special file or entry the
/// plan did not accept, and nothing past the plan's entry count), and every mode is then set
/// to the plan's, which drops the setuid, setgid and sticky bits.
fn verify_clone(x: &TreePlan, staging: &Path) -> Result<(), String> {
    let mismatch = |why: String| {
        format!(
            "the cloned tree does not match the plan (the app changed since it was planned): {why}"
        )
    };
    let mut walk = Walk {
        root: staging.to_owned(),
        guest_root: simpaths::XCODE_APP.to_owned(),
        limits: Limits {
            max_entries: x.nodes.len().saturating_add(1),
            max_depth: MAX_DEPTH,
        },
        // A link the plan accepted is in the plan; any other is a difference below.
        allow_external: true,
        nodes: Vec::new(),
        bytes: 0,
        external: Vec::new(),
    };
    walk.dir(Path::new(""), 0)
        .map_err(|e| mismatch(e.to_string()))?;
    let differs = walk.nodes.len() != x.nodes.len()
        || walk
            .nodes
            .iter()
            .zip(&x.nodes)
            .any(|(a, b)| shape(a) != shape(b));
    if differs {
        let at = walk
            .nodes
            .iter()
            .zip(&x.nodes)
            .find(|(a, b)| shape(a) != shape(b))
            .map_or(Path::new(""), |(a, _)| a.rel());
        return Err(mismatch(format!(
            "{} entries cloned, {} planned, first difference at {}",
            walk.nodes.len(),
            x.nodes.len(),
            at.display()
        )));
    }
    let mut dir_modes = vec![(staging.to_owned(), 0o755)];
    for node in &x.nodes {
        let at = staging.join(node.rel());
        match node {
            Node::Dir { mode, .. } => dir_modes.push((at, *mode)),
            Node::File { mode, .. } => chmod(&at, *mode)?,
            Node::Link { .. } => {}
        }
    }
    apply_modes(dir_modes)
}

/// Builds the plan's tree at `staging` (which does not exist): a tree clone when the host
/// can (checked against the plan afterwards, see [`verify_clone`]), else file by file.
/// Returns the method used.
fn build_tree(x: &TreePlan, staging: &Path, env: &Env<'_>) -> Result<&'static str, String> {
    if (env.clone_tree)(&x.source, staging).is_ok() {
        verify_clone(x, staging)?;
        return Ok("clone");
    }
    // A failed clone may leave part of the tree: start from nothing.
    remove_leftover(staging);
    mkdir(staging, 0o700)?;
    let mut dir_modes = vec![(staging.to_owned(), 0o755)];
    for node in &x.nodes {
        let dst = staging.join(node.rel());
        match node {
            Node::Dir { mode, .. } => {
                mkdir(&dst, 0o700)?;
                dir_modes.push((dst, *mode));
            }
            Node::File {
                src, mode, bytes, ..
            } => copy_file(src, &dst, *mode, *bytes)?,
            Node::Link { target, .. } => link(target, &dst)?,
        }
    }
    apply_modes(dir_modes)?;
    Ok("copy")
}

/// `rename` of `from` to `to`.
pub(crate) fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to)
        .map_err(|e| format!("rename {} -> {}: {e}", from.display(), to.display()))
}

/// Builds `tree` under a staging name beside `dest_abs` (its guest path) and renames it into
/// place.
fn stage_tree(
    volume: &Path,
    tree: &TreePlan,
    dest_abs: &str,
    env: &Env<'_>,
) -> Result<Value, String> {
    let dest = on_volume(volume, dest_abs);
    // `dest_abs` has a parent and a name: it is a path in the guest.
    let (parent, name) = (
        Path::new(dest_abs.trim_start_matches('/'))
            .parent()
            .unwrap_or(Path::new("")),
        dest.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    );
    ensure_dir(volume, parent, env)?;
    let staging = volume.join(parent).join(format!(".{name}.kbf-staging"));
    remove_leftover(&staging);
    let method = build_tree(tree, &staging, env)?;
    chown(env, &staging)?;
    for node in &tree.nodes {
        chown(env, &staging.join(node.rel()))?;
    }
    // The finished copy replaces a previous one: the old tree is removed, then the new one
    // is renamed into place (a directory cannot be renamed over a full one). A run that dies
    // between the two leaves no Xcode.app, never a half one.
    remove_leftover(&dest);
    rename(&staging, &dest)?;
    let count = |f: fn(&Node) -> bool| tree.nodes.iter().filter(|n| f(n)).count();
    Ok(json!({
        "dest": dest_abs,
        "method": method,
        "bytes": tree.bytes,
        "files": count(|n| matches!(n, Node::File { .. })),
        "dirs": count(|n| matches!(n, Node::Dir { .. })),
        "symlinks": count(|n| matches!(n, Node::Link { .. })),
        "external_links": tree.external_links.iter()
            .map(|(p, t)| json!({"path": p, "target": t}))
            .collect::<Vec<_>>(),
    }))
}

fn stage_runtimes(volume: &Path, runtimes: &[RuntimePlan], env: &Env<'_>) -> Result<Value, String> {
    let rel = Path::new(simpaths::RUNTIME_DIR.trim_start_matches('/'));
    let dir = volume.join(rel);
    ensure_dir(volume, rel, env)?;
    // The directory holds exactly this setup's runtimes: the guest installs what it finds.
    let present = std::fs::read_dir(&dir)
        .and_then(|rd| {
            rd.map(|e| e.map(|e| e.path()))
                .collect::<io::Result<Vec<_>>>()
        })
        .map_err(|e| format!("read {}: {e}", dir.display()))?;
    for path in present {
        if !runtimes.iter().any(|r| path.ends_with(&r.name)) {
            remove_leftover(&path);
        }
    }
    let mut staged = Vec::new();
    for r in runtimes {
        let dst = dir.join(&r.name);
        remove_leftover(&dst);
        let sha256 = image::stage(&r.src, &dst).map_err(|e| e.to_string())?;
        let size = file_len(&dst)?;
        if size != r.bytes {
            return Err(format!(
                "{} changed while it was copied ({} bytes planned, {size} copied)",
                r.src.display(),
                r.bytes
            ));
        }
        chmod(&dst, 0o644)?;
        chown(env, &dst)?;
        staged.push(json!({
            "name": r.name,
            "bytes": size,
            "sha256": sha256,
            "dest": format!("{}/{}", simpaths::RUNTIME_DIR, r.name),
        }));
    }
    Ok(Value::Array(staged))
}

/// Writes `plan` into the Data volume mounted at `volume`, and returns what was staged
/// (also written to the volume as the manifest the guest reads).
///
/// # Errors
/// Not enough free space (named, with the sizes), a link or file where a directory must
/// be, a source that changed since the plan, or an I/O error. Nothing already staged is
/// rolled back; Xcode.app is renamed into place only once complete. A rerun holds the
/// previous Xcode.app and the new one side by side until the rename, so the free-space check
/// (which counts the previous copy as used) asks for room for both.
pub fn execute(volume: &Path, plan: &Plan, env: &Env<'_>) -> Result<Value, String> {
    let (free, need) = check_space(volume, plan, env)?;
    let xcode = plan
        .xcode
        .as_ref()
        .map(|x| stage_tree(volume, x, simpaths::XCODE_APP, env))
        .transpose()?;
    // Always: a setup that names no runtime clears the earlier ones.
    let runtimes = stage_runtimes(volume, &plan.runtimes, env)?;
    let manifest = json!({"xcode": xcode, "runtimes": runtimes});
    let rel = Path::new(simpaths::MANIFEST.trim_start_matches('/'));
    ensure_dir(volume, rel.parent().unwrap_or(Path::new("")), env)?;
    let path = volume.join(rel);
    write_text(&path, &format!("{manifest:#}\n"), 0o644)?;
    chown(env, &path)?;
    Ok(json!({
        "free_bytes_before": free,
        "needed_bytes": need,
        "xcode": xcode,
        "runtimes": runtimes,
        "manifest": simpaths::MANIFEST,
    }))
}
