//! [`TransferConfig`]: the engine's limits, one value per `--transfer-*` flag
//! (`docs/design/worker-cache.md#45-knobs-flags-not-environment-variables`).

use crate::cas::WRITE_CHUNK_BYTES;

/// Requests in flight for the whole process by default (`--transfer-in-flight`).
/// Unmeasured: Buildbarn's examples use 10, NativeLink 64.
pub const DEFAULT_IN_FLIGHT: usize = 64;

/// The engine's limits. Every field is a flag of `kbf-daemon` (crate `kbf-node`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferConfig {
    /// The most requests in flight at once for the whole process: ByteStream reads and
    /// writes and FindMissing calls (`--transfer-in-flight`).
    pub in_flight: usize,
    /// The most bytes of blobs read into memory in flight at once
    /// (`--transfer-bytes-in-flight`). A blob larger than this is fetched alone.
    pub bytes_in_flight: usize,
    /// The largest blob [`crate::Cas::read_to`] fetches through memory, where requests
    /// for it from any lease share one fetch (`--transfer-memory-blob-bytes`). A larger
    /// one is streamed into the file.
    pub memory_blob_bytes: u64,
    /// The most bytes one ByteStream `WriteRequest` carries (`--transfer-chunk-bytes`).
    pub chunk_bytes: usize,
    /// The most digests one FindMissing call asks about (`--transfer-find-missing-chunk`).
    pub find_missing_chunk: usize,
    /// The HTTP/2 window of each stream (`--transfer-stream-window-bytes`); the
    /// connection's is eight times this.
    pub stream_window_bytes: u32,
    /// How many times a request is retried after an UNAVAILABLE or a broken
    /// connection (`--transfer-retries`).
    pub retries: u32,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            in_flight: DEFAULT_IN_FLIGHT,
            bytes_in_flight: 256 << 20,
            memory_blob_bytes: 1 << 20,
            chunk_bytes: WRITE_CHUNK_BYTES,
            find_missing_chunk: 1000,
            stream_window_bytes: 8 << 20,
            retries: 3,
        }
    }
}
