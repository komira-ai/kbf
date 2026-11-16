//! What `--executor js-agent` needs of the flags beside it, checked when the flags are parsed.
//!
//! The executor prepares each run through the frontend's [`kbf_front::prepare_run::PutDirPrep`]
//! (`--task-*`), which makes the put directory the task commits its object into and the closure
//! its action tree is written from, and ingests the object from the frontend's packs
//! (`--pack-store-*`). Every disagreement between those and the executor's own settings would
//! otherwise show up later, on the first run, as a failure of that run: a pack that is never
//! found, a run longer than its keys are good for. Each is refused here, naming the flags.
//!
//! A task's job has no network and reads no grant, so no listener and no store endpoint is
//! needed: the job-facing read listener (`--job-*`) is independent of the executor.

use std::time::Duration;

use kbf_front::prepare_run::MAX_EXPIRY;

use super::executor_flags::JsAgentFlags;
use super::pack_flags::PackFlags;
use super::task_flags::PutDirFlags;
use crate::executor::{Config, TaskOut};

/// The settings `--executor js-agent` rests on, as the flags beside it give them.
pub(super) struct Beside<'a> {
    /// `--task-secret-file` and its companions.
    pub put_dir: Option<&'a PutDirFlags>,
    /// `--pack-store-endpoint` and its companions.
    pub packs: Option<&'a PackFlags>,
}

/// Checks `js` against the flags beside it, and sets what follows from them: the roots of the put
/// directories, the limits of an object, and the ceiling the executor holds a run's wall to (the
/// 4 hours a directory's keys may be good for, less the slack the preparation adds to every run's
/// wall time).
///
/// # Errors
/// A flag the executor cannot run without is missing (the packs, `--task-secret-file`), or the
/// longest run's wall time does not fit the ceiling.
pub(super) fn check(js: &mut JsAgentFlags, beside: &Beside<'_>) -> Result<(), String> {
    let needs = |what: &str| format!("--executor js-agent needs {what}");
    if beside.packs.is_none() {
        return Err(needs(
            "--pack-store-endpoint: the packs tasks write are read and checked through them",
        ));
    }
    let put_dir = beside.put_dir.ok_or_else(|| {
        needs(
            "--task-secret-file (and --task-out-root): the put directory of each run is made              from them",
        )
    })?;
    js.put_retry = put_dir.put_retry;
    let config = &mut js.config;
    config.task_out = TaskOut::PutDir(put_dir.roots.clone());
    config.max_object_entries = put_dir.max_object_entries;
    config.max_object_manifest_bytes = put_dir.max_object_manifest_bytes;
    config.object_bytes_per_entry = put_dir.object_bytes_per_entry;
    fit_wall(config, put_dir.url_slack)
}

/// Sets the ceiling the executor holds a run's wall to (the 4 hours a directory's keys may be
/// good for, less `slack`), and checks the longest run fits it and the settings agree
/// ([`Config::validate`]).
fn fit_wall(config: &mut Config, slack: Duration) -> Result<(), String> {
    let refuse = |why: &str| format!("--executor js-agent: {why}");
    config.presign_ceiling = MAX_EXPIRY.saturating_sub(slack);
    let longest = config.max_run + config.run_margin;
    if longest > config.presign_ceiling {
        return Err(refuse(&format!(
            "--executor-max-run-secs plus --executor-run-margin-secs ({} s) must be at most \
             14400 s less --task-url-slack-secs ({} s), the longest the keys of a put directory \
             may be good for once the slack is added",
            longest.as_secs(),
            slack.as_secs(),
        )));
    }
    config.validate().map_err(|e| refuse(&e.to_string()))
}

#[cfg(test)]
#[path = "executor_bind_tests.rs"]
mod tests;
