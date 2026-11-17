//! What one job is told: the put directory it commits into, and the command line of the task
//! tool.
//!
//! The job holds no url, no token and no other secret: it has no network, and reads its inputs
//! from its tree.

use std::fmt;
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;

/// The build id of a farm lease, in a pack key and in the task's `--lease`: `<term>-<seq>`.
/// The farm's own spelling, `<term>.<seq>`, holds a dot, which a build id may not
/// (`kbf_pack::check_build` allows `0-9`, `a-z`, `A-Z`, `-` and `_`): a pack that names it is
/// refused by the ingest, and a key built from it is refused by the mint.
#[must_use]
pub fn build_id(lease: LeaseId) -> String {
    format!("{}-{}", lease.term, lease.seq)
}

/// The put directory a task commits its objects into, `cas/b/<build>/<nonce>`: issued by the
/// frontend's ingest ([`Ingest::mint`](crate::executor::Ingest::mint)) for one farm lease of one
/// action and verified by the same ingest when the run is read. The executor does not look
/// inside it.
///
/// A farm lease is one job: its id is made from the lease ([`job_id`](crate::executor::job_id)),
/// a submit whose answer was lost is sent again with the same id and makes no second job, and
/// the farm runs an action again only as a new farm lease. So the directory of a lease is never
/// handed to a second job.
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

/// The flags of the task tool for the run of `action` on `lease` that commits into a put
/// directory: the inputs are the tree at `/` (`--tree`), the outputs are committed into
/// [`OUT_TARGET`](crate::executor::OUT_TARGET) (`--stage-and-commit`, `--out-dir`), and the action
/// may run for `deadline`. The task is also told to write one object (`--object-format one-pass`),
/// the only format it writes: a task of a release that still has two formats needs the words, and
/// one that has only this one accepts them. No url, no grant and no secret: the job has no
/// network.
#[must_use]
pub fn put_dir_argv(action: &Digest, lease: LeaseId, deadline: Duration) -> Vec<String> {
    vec![
        "--tree".to_owned(),
        "--stage-and-commit".to_owned(),
        "--out-dir".to_owned(),
        crate::executor::OUT_TARGET.to_owned(),
        "--action".to_owned(),
        format!("{}/{}", action.hash, action.size_bytes),
        "--lease".to_owned(),
        build_id(lease),
        "--deadline-ms".to_owned(),
        deadline.as_millis().to_string(),
        "--object-format".to_owned(),
        "one-pass".to_owned(),
    ]
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
