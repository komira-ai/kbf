//! `image setup --xcode --sim-runtime`: staging Xcode and the iOS simulator runtimes into a
//! guest's Data volume while the guest is off.
//!
//! The work has two halves. [`plan`] reads the host side only (Xcode.app and the runtime
//! disk images the operator names), refuses what must not be copied, and totals the bytes.
//! [`execute`] writes a plan into a mounted Data volume: it checks the free space first,
//! checks that every directory it writes through is a real directory of the volume (a
//! symbolic link planted by an earlier guest is not followed), builds Xcode.app beside its
//! final name and renames it into place, and copies the runtimes while hashing them. Both
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
//!   setuid, setgid and sticky bits are dropped.
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
/// The most runtimes one setup stages.
pub const MAX_RUNTIMES: usize = 8;
/// Free space that must remain on the volume after the copy: the guest writes to its
/// Data volume while it boots, and Xcode's first launch expands what it installs.
pub const HEADROOM_BYTES: u64 = 4 << 30;
/// The name Xcode.app is built under inside `Applications` before it is renamed.
const STAGING_NAME: &str = ".Xcode.app.kbf-staging";

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
    #[error("{0}: more than {MAX_ENTRIES} entries")]
    TooMany(PathBuf),
    #[error("{0}: nested deeper than {MAX_DEPTH} directories")]
    TooDeep(PathBuf),
    #[error("{0} is not a regular file")]
    NotRegular(PathBuf),
    #[error(
        "{0}: a runtime must be named <name>.dmg with name made of letters, digits, '.', '_' or '-'"
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

/// Xcode.app as it will be copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XcodePlan {
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
    pub xcode: Option<XcodePlan>,
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

/// Whether the link target `target`, found at `depth` directories below the app's root,
/// stays inside the app. Only leading `..` components count, and no more than `depth` of
/// them: a `..` after a name could climb out through a link that name goes through.
pub(crate) fn stays_inside(target: &Path, depth: usize) -> bool {
    if target.has_root() {
        return target.starts_with(simpaths::XCODE_APP)
            && !target.components().any(|c| c == Component::ParentDir);
    }
    let mut up = 0usize;
    let mut named = false;
    for c in target.components() {
        match c {
            Component::ParentDir if named => return false,
            Component::ParentDir => up += 1,
            Component::Normal(_) => named = true,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    up <= depth && target.components().next().is_some()
}

struct Walk {
    root: PathBuf,
    allow_external: bool,
    nodes: Vec<Node>,
    bytes: u64,
    external: Vec<(PathBuf, PathBuf)>,
}

impl Walk {
    fn dir(&mut self, rel: &Path, depth: usize) -> Result<(), StageError> {
        if depth > MAX_DEPTH {
            return Err(StageError::TooDeep(self.root.join(rel)));
        }
        let here = self.root.join(rel);
        let mut names: Vec<_> = std::fs::read_dir(&here)
            .map_err(|e| io_err("read", &here, &e))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(|e| io_err("read", &here, &e))?;
        // A fixed order, so two plans of one tree are equal.
        names.sort();
        for name in names {
            if self.nodes.len() >= MAX_ENTRIES {
                return Err(StageError::TooMany(self.root.clone()));
            }
            let (child_rel, path) = (rel.join(&name), here.join(&name));
            let meta = std::fs::symlink_metadata(&path).map_err(|e| io_err("stat", &path, &e))?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                let target =
                    std::fs::read_link(&path).map_err(|e| io_err("readlink", &path, &e))?;
                if !stays_inside(&target, depth) {
                    if !self.allow_external {
                        return Err(StageError::EscapingLink { path, target });
                    }
                    self.external.push((child_rel.clone(), target.clone()));
                }
                self.nodes.push(Node::Link {
                    rel: child_rel,
                    target,
                });
            } else if ft.is_dir() {
                self.nodes.push(Node::Dir {
                    rel: child_rel.clone(),
                    mode: meta.permissions().mode() & 0o777,
                });
                self.dir(&child_rel, depth + 1)?;
            } else if ft.is_file() {
                self.bytes += meta.len();
                self.nodes.push(Node::File {
                    rel: child_rel,
                    src: path,
                    mode: meta.permissions().mode() & 0o777,
                    bytes: meta.len(),
                });
            } else {
                return Err(StageError::Special {
                    path,
                    kind: kind_of(&ft),
                });
            }
        }
        Ok(())
    }
}

fn kind_of(ft: &std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_fifo() {
        "named pipe"
    } else if ft.is_socket() {
        "socket"
    } else if ft.is_char_device() || ft.is_block_device() {
        "device"
    } else {
        "special file"
    }
}

/// Reads Xcode.app at `app` into a plan.
///
/// # Errors
/// `app` is a link or not an Xcode.app, or something in it is refused (see the module
/// documentation), or it is too large to walk.
pub fn plan_xcode(app: &Path, allow_external_links: bool) -> Result<XcodePlan, StageError> {
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
        allow_external: allow_external_links,
        nodes: Vec::new(),
        bytes: 0,
        external: Vec::new(),
    };
    walk.dir(Path::new(""), 0)?;
    Ok(XcodePlan {
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

/// Makes sure every directory on the way to `dir` (under `volume`) is a real directory,
/// creating the missing ones (root, 0755). A link or a file in the way is an error.
fn ensure_dir(volume: &Path, dir: &Path, env: &Env<'_>) -> Result<(), String> {
    let rel = dir
        .strip_prefix(volume)
        .map_err(|_| format!("{} is not under the volume", dir.display()))?;
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
                std::fs::create_dir(&at).map_err(|e| format!("create {}: {e}", at.display()))?;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("chmod {}: {e}", at.display()))?;
                (env.chown_root)(&at).map_err(|e| format!("chown {}: {e}", at.display()))?;
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

/// Builds the plan's Xcode.app at `staging` (which does not exist): a tree clone when the
/// host can, else file by file. Returns the method used.
fn build_xcode(x: &XcodePlan, staging: &Path, env: &Env<'_>) -> Result<&'static str, String> {
    if (env.clone_tree)(&x.source, staging).is_ok() {
        return Ok("clone");
    }
    // A failed clone may leave part of the tree: start from nothing.
    remove_leftover(staging);
    std::fs::create_dir(staging).map_err(|e| format!("create {}: {e}", staging.display()))?;
    let mut dir_modes = vec![(staging.to_owned(), 0o755)];
    for node in &x.nodes {
        let dst = staging.join(node.rel());
        match node {
            Node::Dir { mode, .. } => {
                std::fs::create_dir(&dst).map_err(|e| format!("create {}: {e}", dst.display()))?;
                dir_modes.push((dst, *mode));
            }
            Node::File {
                src, mode, bytes, ..
            } => {
                let copied = std::fs::copy(src, &dst)
                    .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
                if copied != *bytes {
                    return Err(format!(
                        "{} changed while it was copied ({bytes} bytes planned, {copied} copied)",
                        src.display()
                    ));
                }
                std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(*mode))
                    .map_err(|e| format!("chmod {}: {e}", dst.display()))?;
            }
            Node::Link { target, .. } => {
                std::os::unix::fs::symlink(target, &dst)
                    .map_err(|e| format!("link {}: {e}", dst.display()))?;
            }
        }
    }
    // Directory modes last: a read-only directory would refuse its own contents.
    for (dir, mode) in dir_modes.into_iter().rev() {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {}: {e}", dir.display()))?;
    }
    Ok("copy")
}

fn stage_xcode(volume: &Path, x: &XcodePlan, env: &Env<'_>) -> Result<Value, String> {
    let dest = on_volume(volume, simpaths::XCODE_APP);
    let parent = dest.parent().ok_or("Xcode.app has no parent")?;
    ensure_dir(volume, parent, env)?;
    let staging = parent.join(STAGING_NAME);
    remove_leftover(&staging);
    let method = build_xcode(x, &staging, env)?;
    (env.chown_root)(&staging).map_err(|e| format!("chown {}: {e}", staging.display()))?;
    for node in &x.nodes {
        let p = staging.join(node.rel());
        (env.chown_root)(&p).map_err(|e| format!("chown {}: {e}", p.display()))?;
    }
    // The finished copy replaces a previous one in a single rename.
    remove_leftover(&dest);
    std::fs::rename(&staging, &dest)
        .map_err(|e| format!("rename {} -> {}: {e}", staging.display(), dest.display()))?;
    let count = |f: fn(&Node) -> bool| x.nodes.iter().filter(|n| f(n)).count();
    Ok(json!({
        "dest": simpaths::XCODE_APP,
        "method": method,
        "bytes": x.bytes,
        "files": count(|n| matches!(n, Node::File { .. })),
        "dirs": count(|n| matches!(n, Node::Dir { .. })),
        "symlinks": count(|n| matches!(n, Node::Link { .. })),
        "external_links": x.external_links.iter()
            .map(|(p, t)| json!({"path": p, "target": t}))
            .collect::<Vec<_>>(),
    }))
}

fn stage_runtimes(volume: &Path, runtimes: &[RuntimePlan], env: &Env<'_>) -> Result<Value, String> {
    let dir = on_volume(volume, simpaths::RUNTIME_DIR);
    ensure_dir(volume, &dir, env)?;
    // The directory holds exactly this setup's runtimes: the guest installs what it finds.
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
        if !runtimes
            .iter()
            .any(|r| entry.file_name() == r.name.as_str())
        {
            remove_leftover(&entry.path());
        }
    }
    let mut staged = Vec::new();
    for r in runtimes {
        let dst = dir.join(&r.name);
        remove_leftover(&dst);
        let sha256 = image::stage(&r.src, &dst).map_err(|e| e.to_string())?;
        let size = std::fs::metadata(&dst)
            .map_err(|e| format!("stat {}: {e}", dst.display()))?
            .size();
        if size != r.bytes {
            return Err(format!(
                "{} changed while it was copied ({} bytes planned, {size} copied)",
                r.src.display(),
                r.bytes
            ));
        }
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("chmod {}: {e}", dst.display()))?;
        (env.chown_root)(&dst).map_err(|e| format!("chown {}: {e}", dst.display()))?;
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
/// rolled back; Xcode.app is renamed into place only once complete.
pub fn execute(volume: &Path, plan: &Plan, env: &Env<'_>) -> Result<Value, String> {
    let (free, need) = check_space(volume, plan, env)?;
    let xcode = plan
        .xcode
        .as_ref()
        .map(|x| stage_xcode(volume, x, env))
        .transpose()?;
    let runtimes = if plan.runtimes.is_empty() {
        None
    } else {
        Some(stage_runtimes(volume, &plan.runtimes, env)?)
    };
    let manifest =
        json!({"xcode": xcode, "runtimes": runtimes.clone().unwrap_or_else(|| json!([]))});
    let path = on_volume(volume, simpaths::MANIFEST);
    let parent = path.parent().ok_or("the manifest has no parent")?;
    ensure_dir(volume, parent, env)?;
    remove_leftover(&path);
    std::fs::write(&path, format!("{manifest:#}\n"))
        .and_then(|()| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    (env.chown_root)(&path).map_err(|e| format!("chown {}: {e}", path.display()))?;
    Ok(json!({
        "free_bytes_before": free,
        "needed_bytes": need,
        "xcode": xcode,
        "runtimes": runtimes,
        "manifest": simpaths::MANIFEST,
    }))
}
