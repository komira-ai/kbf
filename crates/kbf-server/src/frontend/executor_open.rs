//! The executor `--executor js-agent` describes, built over the frontend's farm and blob seams
//! ([`executor_of`]). `frontend.rs` has the whole picture.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Backend, Packs};

use super::FrontendFlags;
use crate::executor::{Executor, Metrics as ExecutorMetrics, RunSource};
use crate::farm::Farm;

/// A built executor, and the counts it keeps.
pub(super) struct ExecutorParts {
    pub(super) executor: Executor,
    pub(super) metrics: Arc<ExecutorMetrics>,
    /// The job service as `/readyz` asks about it, for a frontend with no storage node.
    pub(super) probe: Arc<dyn crate::health::Probe>,
}

/// The executor `--executor js-agent` describes, over `farm`, with the metrics it counts into;
/// none for `kbf-daemon`, which builds nothing and contacts no job service. The seams are the
/// frontend's own: the ingest of the packs, the CAS `backend` over the same packs, and the
/// `source` of the keys, writes and grants.
///
/// # Errors
/// `--executor js-agent` without the packs or without a `source` (the frontend makes one from
/// the run preparation `--task-secret-file` opens, which the parse of the flags requires), or
/// as [`super::JsAgentFlags::build`].
pub(super) fn executor_of(
    flags: &FrontendFlags,
    farm: &Arc<Farm>,
    packs: Option<&Packs>,
    backend: &Backend,
    source: Option<RunSource>,
    timeout: Duration,
) -> Result<Option<ExecutorParts>, Box<dyn Error>> {
    let Some(js) = &flags.executor.js else {
        return Ok(None);
    };
    let packs = packs.ok_or("--executor js-agent needs --pack-store-endpoint")?;
    let source = source.ok_or(
        "--executor js-agent: this frontend was started without the preparation of task runs \
         (the key mint, the presigned write and the read grant): give --task-secret-file",
    )?;
    let front = js.front(packs.clone(), backend.clone(), source);
    let metrics = Arc::new(ExecutorMetrics::default());
    let executor = js.build(Arc::clone(farm), Arc::new(front), Arc::clone(&metrics))?;
    let probe: Arc<dyn crate::health::Probe> = Arc::new(executor.probe(timeout));
    Ok(Some(ExecutorParts {
        executor,
        metrics,
        probe,
    }))
}
