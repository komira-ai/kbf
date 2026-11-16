//! Where a task's outputs go ([`TaskOut`], `--task-out-mode`): presigned writes to the tier
//! store's proxy, or a put directory on the job's host (docs/design/jobs-on-jobscheduler-and-tierstore.md,
//! sections 2 and 3).
//!
//! In a put directory the task commits its objects as files, and the node's tier store makes
//! each an object of the store when its rename returns. The job is then submitted with:
//!
//! - **One layer**, the read-only action tree `<mp>/trees/<digest>` ([`PutDirRoots::layer`]):
//!   the job's root, with `work` (`/work`), `.kbf` (`/.kbf`) and the base tree's
//!   `/.kbf-tools/kbf-task` ([`TASK_IN_TREE`]), the program the job runs. No image.
//! - **One mount**, the put directory `<mp>/put/cas/b/<lease>/<nonce>` at [`OUT_TARGET`],
//!   read-write and `no_exec` ([`PutDirRoots::mount`]).
//!
//! `<mp>/put` is `--task-out-root`, and `<mp>/trees` the directory beside it: the tier store
//! mounts both under one mount point, and the host's agent lists them as its mount roots
//! (`--mount-root put=<mp>/put`, `--mount-root-ro trees=<mp>/trees`).

use js_proto::v1::{Layer, Mount, MountMode, MountPropagation, layer, mount};

/// Where the put directory is mounted in a job: `kbf-task --out-dir`'s default.
pub const OUT_TARGET: &str = "/out";

/// The program a put-directory job runs, in the base tree every action tree is written over.
pub const TASK_IN_TREE: &str = "/.kbf-tools/kbf-task";

/// The longest `--task-out-root`: the job service takes a host path of at most 4096 bytes, and
/// a put directory adds at most 176 to the root (`/cas/b/`, a build id of 128 and `/` and a
/// nonce of 40), an action tree 71 to the trees beside it.
pub const MAX_ROOT_BYTES: usize = 4096 - 176;

/// Where a task's outputs go.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TaskOut {
    /// A presigned `PUT` of each object to the tier store's proxy, and a read grant for the
    /// inputs: the job reaches both over its network.
    #[default]
    Presigned,
    /// A put directory on the job's host, under these roots, and the inputs in a read-only
    /// tree: the job has no network.
    PutDir(PutDirRoots),
}

/// Why `--task-out-root` is not one.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RootError {
    /// Not an absolute path.
    #[error("{0:?} is not an absolute path")]
    NotAbsolute(String),
    /// `/` alone, or a path with an empty, `.` or `..` part, a `/` at its end, or a control
    /// character.
    #[error(
        "{0:?} is not a path in normal form below /: no empty, . or .. part, no / at its end, no control character"
    )]
    NotNormal(String),
    /// Longer than [`MAX_ROOT_BYTES`].
    #[error("{0:?} is longer than {MAX_ROOT_BYTES} bytes")]
    TooLong(String),
}

/// The host directories of a put-directory job: the put root and the trees beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutDirRoots {
    put: String,
    trees: String,
}

impl PutDirRoots {
    /// The roots of `put`, the tier store's put directory on every host (`<mp>/put`); the
    /// trees are `<mp>/trees`.
    ///
    /// # Errors
    /// [`RootError`]: `put` is not an absolute path in normal form below `/`, or is too long.
    pub fn new(put: &str) -> Result<Self, RootError> {
        let Some(below) = put.strip_prefix('/') else {
            return Err(RootError::NotAbsolute(put.to_owned()));
        };
        let normal = below
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
            && !put.chars().any(char::is_control);
        if !normal {
            return Err(RootError::NotNormal(put.to_owned()));
        }
        if put.len() > MAX_ROOT_BYTES {
            return Err(RootError::TooLong(put.to_owned()));
        }
        // `put` has a part below `/`, so it has a parent: `/` itself, or a directory.
        let parent = put.rsplit_once('/').map_or("", |(parent, _)| parent);
        Ok(Self {
            put: put.to_owned(),
            trees: format!("{parent}/trees"),
        })
    }

    /// The put root, `<mp>/put`.
    #[must_use]
    pub fn put(&self) -> &str {
        &self.put
    }

    /// The trees beside it, `<mp>/trees`.
    #[must_use]
    pub fn trees(&self) -> &str {
        &self.trees
    }

    /// The job's one layer: the action tree `tree` (its 64 hex digits) under the trees.
    #[must_use]
    pub fn layer(&self, tree: &str) -> Layer {
        Layer {
            source: Some(layer::Source::HostPath(format!("{}/{tree}", self.trees))),
        }
    }

    /// The job's one mount: the put directory `dir` (`cas/b/<lease>/<nonce>`) under the put
    /// root, at [`OUT_TARGET`], read-write, no file in it executable.
    #[must_use]
    pub fn mount(&self, dir: &str) -> Mount {
        Mount {
            source: Some(mount::Source::HostPath(format!("{}/{dir}", self.put))),
            target: OUT_TARGET.to_owned(),
            mode: MountMode::ReadWrite.into(),
            sub_path: String::new(),
            no_exec: true,
            propagation: MountPropagation::Private.into(),
        }
    }
}

#[cfg(test)]
#[path = "out_tests.rs"]
mod tests;
