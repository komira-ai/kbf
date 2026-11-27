//! The one INFO line each job this process submits writes, `the job service took the job`
//! ([`log_submitted`]): the frontend's half of a job's start, per job, for joining with the
//! host's own line for the same job id.

use std::time::{SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use super::Op;
use crate::executor::task::RunId;

/// Writes the one line of a submitted job: its operation, run and job id; when the Execute came
/// to the front (`reapi_unix_ms`), when the executor took it (`execute_unix_ms`) and when the job
/// service answered the submit with the job, as Unix milliseconds; and, in milliseconds, the time
/// from the front to the executor (`presubmit_ms`: the Execute's authorization, its action and
/// command reads, its input walk and its operation id), from the executor to that answer
/// (`since_execute_ms`, every attempt before this one included) and from the front to it
/// (`since_reapi_ms`, their sum), and this run's `prepare` and `start` phases
/// ([`super::super::phases`]). `at` holds the run's `Seen`, `Prepared` and `Posted` stamps.
///
/// `presubmit_ms` is split as the front measured it ([`kbf_front::PresubmitSplit`]), so its
/// median can be told apart from the log alone:
/// - `auth_ms`, the call's authorization;
/// - `lookup_ms`, the action-cache lookup beside the Action's read;
/// - `inputs_ms`, the Command's read beside the input walk, of which `walk_ms` is the walk, and
///   of that `dirs_ms` its Directory phase (reads, `dir_reads` of them, and Directories taken
///   from the content cache, `dir_cache_hits`) and `files_ms` its presence checks (`file_asks`
///   FindMissing calls about `files_checked` files and `dirs_present_checked` cached
///   Directories); `dirs_skipped` counts the subtrees an earlier walk verified, which cost none
///   of these;
/// - `rest_ms`, what is left of `presubmit_ms`: the decoding after the walk, the dispatcher's
///   submit up to the operation's opening (its id included) and the small steps between the
///   stages above.
///
/// The submit time is the Execute's wall-clock time plus the executor's own measure of the time
/// since, so the two times differ by exactly `since_execute_ms` even if the wall clock steps.
/// For an operation taken up from the state store, the Execute's time is when this process took
/// it up ([`Op::new`]), and no front saw it: `reapi_unix_ms`, `presubmit_ms` and its split are 0.
pub(super) fn log_submitted(op: &Op, run: RunId, job: &str, at: [Instant; 3]) {
    let [seen, ready, posted] = at;
    let since_execute_ms = posted.saturating_duration_since(op.created).as_millis();
    let unix_ms = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let execute_unix_ms = unix_ms(op.arrived);
    let (reapi_unix_ms, presubmit) = op.reapi.map_or((0, None), |reapi| {
        let presubmit = op.created.saturating_duration_since(reapi.at);
        (unix_ms(reapi.unix), Some(presubmit))
    });
    let presubmit = presubmit.unwrap_or_default();
    let split = &op.presubmit;
    let rest = presubmit
        .saturating_sub(split.auth)
        .saturating_sub(split.lookup)
        .saturating_sub(split.inputs);
    let presubmit_ms = presubmit.as_millis();
    tracing::info!(
        operation = %op.id,
        %run,
        %job,
        attempt = run.attempt,
        execute_unix_ms,
        submitted_unix_ms = execute_unix_ms + since_execute_ms,
        since_execute_ms,
        reapi_unix_ms,
        presubmit_ms,
        since_reapi_ms = since_execute_ms + presubmit_ms,
        prepare_ms = ready.saturating_duration_since(seen).as_millis(),
        start_ms = posted.saturating_duration_since(ready).as_millis(),
        auth_ms = split.auth.as_millis(),
        lookup_ms = split.lookup.as_millis(),
        inputs_ms = split.inputs.as_millis(),
        walk_ms = split.walk.as_millis(),
        dirs_ms = split.walk_dirs.as_millis(),
        files_ms = split.walk_files.as_millis(),
        rest_ms = rest.as_millis(),
        dir_reads = split.dir_reads,
        dirs_skipped = split.dirs_skipped,
        dir_cache_hits = split.dir_cache_hits,
        dirs_present_checked = split.dirs_present_checked,
        files_checked = split.files_checked,
        file_asks = split.file_asks,
        "the job service took the job"
    );
}
