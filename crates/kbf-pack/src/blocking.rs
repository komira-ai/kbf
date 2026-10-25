//! Calls that block, run off the async worker threads.

/// Runs `work`, which blocks (a call of the pack index, a read or write of a cache file), on
/// the runtime's blocking pool and waits for it.
///
/// # Panics
/// If `work` panics, or the runtime shuts down under it: neither is an error to hand back.
pub(crate) async fn offload<T, F>(work: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(work).await;
    joined.expect("a blocking pack call panicked")
}
