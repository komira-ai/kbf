//! What one job is told: the put directory it commits into, and the command line of the task
//! tool.
//!
//! The job holds no url, no token and no other secret: it has no network, and reads its inputs
//! from its tree.

use std::fmt;
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_types::OperationId;

/// One run of an operation: the operation's id and the attempt its job is, counted from 0. An
/// operation is run again only as its next attempt, so a run is one job ([`job_id`]) and one
/// pack key, and a zombie of an earlier attempt never writes under a later one's key.
///
/// [`job_id`]: crate::executor::job_id
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId {
    /// The operation (`operations/<id>`).
    pub operation: OperationId,
    /// Which run of it, from 0.
    pub attempt: u32,
}

impl RunId {
    /// The run `attempt` of `operation`.
    #[must_use]
    pub const fn new(operation: OperationId, attempt: u32) -> Self {
        Self { operation, attempt }
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.operation.0, self.attempt)
    }
}

/// The build id of a run, in a pack key and in the task's `--lease`: `<operation>-<attempt>`,
/// which `kbf_pack::check_build` takes (`0-9`, `a-z`, `A-Z`, `-` and `_`).
#[must_use]
pub fn build_id(run: RunId) -> String {
    run.to_string()
}

/// The put directory a task commits its objects into, `cas/b/<build>/<nonce>`: issued by the
/// frontend's ingest ([`Ingest::mint`](crate::executor::Ingest::mint)) for one run of one
/// action and verified by the same ingest when the run is read. The executor does not look
/// inside it.
///
/// A run is one job: its id is made from the run ([`job_id`](crate::executor::job_id)), a
/// submit whose answer was lost is sent again with the same id and makes no second job, and an
/// operation is run again only as its next attempt. So the directory of a run is never handed
/// to a second job, and a zombie of an earlier job never meets the create-only refusal of a key
/// it did not write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackKey(String);

impl PackKey {
    /// The object key `key`.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The object key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The flags of the task tool for `run` of `action` that commits into a put
/// directory: the inputs are the tree at `/` (`--tree`), the outputs are committed into
/// [`OUT_TARGET`](crate::executor::OUT_TARGET) (`--stage-and-commit`, `--out-dir`), and the action
/// may run for `deadline`. The task is also told to write one object (`--object-format one-pass`),
/// the only format it writes: a task of a release that still has two formats needs the words, and
/// one that has only this one accepts them. No url, no grant and no secret: the job has no
/// network.
#[must_use]
pub fn put_dir_argv(action: &Digest, run: RunId, deadline: Duration) -> Vec<String> {
    vec![
        "--tree".to_owned(),
        "--stage-and-commit".to_owned(),
        "--out-dir".to_owned(),
        crate::executor::OUT_TARGET.to_owned(),
        "--action".to_owned(),
        format!("{}/{}", action.hash, action.size_bytes),
        "--lease".to_owned(),
        build_id(run),
        "--deadline-ms".to_owned(),
        deadline.as_millis().to_string(),
        "--object-format".to_owned(),
        "one-pass".to_owned(),
    ]
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
