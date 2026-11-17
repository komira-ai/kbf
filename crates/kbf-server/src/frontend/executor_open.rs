//! The executor `--executor js-agent` describes, built over the frontend's packs and blob seams
//! ([`executor_of`]). `frontend.rs` has the whole picture.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Backend, Packs, Recorder};

use super::FrontendFlags;
use crate::executor::{Executor, Metrics as ExecutorMetrics, RunSource};

/// A built executor, and the counts it keeps.
pub(super) struct ExecutorParts {
    pub(super) executor: Executor,
    pub(super) metrics: Arc<ExecutorMetrics>,
    /// The job service as `/readyz` asks about it, for a frontend with no storage node.
    pub(super) probe: Arc<dyn crate::health::Probe>,
}

/// The executor `--executor js-agent` describes, with the metrics it counts into; none when
/// `--executor` is left out, which builds nothing and contacts no job service. The seams are
/// the frontend's own: the ingest of the packs, the CAS `backend` over the same packs, and the
/// `source` of the put directories. The rows of the operations are kept in `store`, the
/// frontend's state store, if it has one; the operations' events go to `recorder`, and a
/// finished operation is answered by name from memory for `--finished-retention-secs`.
///
/// # Errors
/// `--executor js-agent` without the packs or without a `source` (the frontend makes one from
/// the run preparation `--task-secret-file` opens, which the parse of the flags requires), or
/// as [`super::JsAgentFlags::build`].
pub(super) fn executor_of(
    flags: &FrontendFlags,
    packs: &Option<Packs>,
    backend: &Backend,
    source: Option<RunSource>,
    timeout: Duration,
    store: Option<Arc<dyn kbf_state::Store>>,
    recorder: Recorder,
) -> Result<Option<ExecutorParts>, Box<dyn Error>> {
    let finished_retention = Duration::from_secs(flags.server.finished_retention_secs);
    let Some(js) = &flags.executor.js else {
        return Ok(None);
    };
    let packs = packs
        .as_ref()
        .ok_or("--executor js-agent needs --pack-store-endpoint")?;
    let source = source.ok_or(
        "--executor js-agent: this frontend was started without the preparation of task runs \
         (the key mint and the put directory of each run): give --task-secret-file",
    )?;
    let front = js.front(packs.clone(), backend.clone(), source);
    let metrics = Arc::new(ExecutorMetrics::default());
    let executor = js.build(
        Arc::new(front),
        Arc::clone(&metrics),
        store,
        recorder,
        finished_retention,
    )?;
    let probe: Arc<dyn crate::health::Probe> = Arc::new(executor.probe(timeout));
    Ok(Some(ExecutorParts {
        executor,
        metrics,
        probe,
    }))
}
