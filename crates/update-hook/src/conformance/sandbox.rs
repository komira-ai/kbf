//! The directories one run of the harness works in, and comparisons of their contents.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What a path holds, for comparing two moments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Dir,
    File(Vec<u8>),
    Link(PathBuf),
}

/// A directory's contents by path relative to it.
pub type Tree = BTreeMap<PathBuf, Entry>;

/// Reads the whole tree under `root`. Links are recorded, not followed.
pub fn snapshot(root: &Path) -> io::Result<Tree> {
    let mut tree = Tree::new();
    walk(root, root, &mut tree)?;
    Ok(tree)
}

fn walk(base: &Path, dir: &Path, tree: &mut Tree) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            tree.insert(relative, Entry::Link(fs::read_link(&path)?));
        } else if kind.is_dir() {
            tree.insert(relative, Entry::Dir);
            walk(base, &path, tree)?;
        } else {
            tree.insert(relative, Entry::File(fs::read(&path)?));
        }
    }
    Ok(())
}

/// The paths added, removed or changed between two snapshots, one line each.
pub fn changes(before: &Tree, after: &Tree) -> Vec<String> {
    let mut out = Vec::new();
    for (path, was) in before {
        match after.get(path) {
            None => out.push(format!("removed {}", path.display())),
            Some(now) if now != was => out.push(format!("changed {}", path.display())),
            Some(_) => {}
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) {
            out.push(format!("added {}", path.display()));
        }
    }
    out
}

/// The five directories of one run:
///
/// - `artifact/`: the staged release. Read-only to the hook.
/// - `record/`: the hook's record directory (`state_dir`). The only place the hook writes
///   besides the world.
/// - `world/`: the fake service's world, which the hook changes through the fake. Its contents
///   are the observable state of the service.
/// - `outside/`: everything else the declaration grants nothing in: the service's own data
///   directory, the old location of a state file. A hook that writes here has gone outside
///   its record.
/// - `control/`: the harness's own files (the params file, the kill-point count).
#[derive(Clone, Debug)]
pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    const DIRS: [&'static str; 6] = [
        "artifact", "previous", "record", "world", "outside", "control",
    ];

    /// Makes the five directories under `root` (which may be new).
    pub fn create(root: PathBuf) -> io::Result<Sandbox> {
        for dir in Sandbox::DIRS {
            fs::create_dir_all(root.join(dir))?;
        }
        Ok(Sandbox { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn artifact(&self) -> PathBuf {
        self.root.join("artifact")
    }

    pub fn record(&self) -> PathBuf {
        self.root.join("record")
    }

    /// The previous release, staged by the harness for an update; unused on a first install.
    pub fn previous(&self) -> PathBuf {
        self.root.join("previous")
    }

    pub fn world(&self) -> PathBuf {
        self.root.join("world")
    }

    pub fn outside(&self) -> PathBuf {
        self.root.join("outside")
    }

    pub fn control(&self) -> PathBuf {
        self.root.join("control")
    }

    /// What no step may change: everything but the record, the world and the harness's own
    /// kill-point count. So the staged and the previous release, `outside/`, the params file and
    /// the files of the sandbox root.
    pub(crate) fn guarded(&self) -> io::Result<Tree> {
        let mut tree = snapshot(&self.root)?;
        tree.retain(|path, _| {
            !path.starts_with("record")
                && !path.starts_with("world")
                && *path != Path::new("control").join(super::gate::COUNT_FILE)
        });
        Ok(tree)
    }
}
