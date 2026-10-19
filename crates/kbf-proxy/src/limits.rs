//! The h2 builders of both sides, and the flood limits, in one place (design note sections
//! 10 and 18).
//!
//! The flow-control numbers are the same everywhere: one [`STREAM_WINDOW_BYTES`] window for
//! every stream, and a connection window of a thousand of them, so held bodies cannot use up
//! a connection's credit. Adaptive BDP is off: it would grow windows the memory budget
//! (`Limits::worst_case_bytes`) assumes are fixed.
//!
//! The flood limits are derived from the 1000-stream cap and not left at h2's defaults (about
//! 20 reset streams that the application has not yet accepted). A Buck2 build cancel resets
//! hundreds of streams of one connection in one burst, and tripping a limit sends the whole
//! connection a GOAWAY, with the streams of a pre-flip child on it. The limits that guard
//! against CONTINUATION floods are derived inside h2 from the header-list cap and the frame
//! size set here (h2 0.4.20 has no setter of their own). h2's cap on resets the proxy itself
//! caused (`max_local_error_reset_streams`, 1024) stays at its default: a burst of client
//! cancels does not reach it.
//!
//! Pre-stream timeouts and the connection caps belong to the accept path, not to a builder.

use hyper::server::conn::http2;
use hyper_util::rt::TokioExecutor;

use crate::config::Limits;
use crate::hold::STREAM_WINDOW_BYTES;
use crate::keepalive;

/// The streams one downstream connection may have open at once. The connection window is this
/// many stream windows (design note section 4).
pub(crate) const DOWNSTREAM_CONNECTION_STREAMS: u32 = 1000;

/// The connection window of an upstream connection, in streams' worth of window.
pub(crate) const UPSTREAM_CONNECTION_STREAMS: u32 = 1000;

/// The most streams of one connection that may be reset by the client before the proxy has
/// accepted them. A client that opens a full connection and cancels every stream in one
/// burst, twice over, is within it; a client that keeps resetting beyond it is the rapid
/// reset attack and gets a GOAWAY on its own connection.
pub(crate) const PENDING_RESET_STREAMS: usize = 2 * DOWNSTREAM_CONNECTION_STREAMS as usize;

// h2 trips on the reset after the limit, so the limit must be above the stream cap.
const _: () = assert!(PENDING_RESET_STREAMS > DOWNSTREAM_CONNECTION_STREAMS as usize);

/// The h2 frame size the proxy accepts: the protocol's minimum, which is also h2's default. It
/// is set so that h2's CONTINUATION cap, derived from it and the header-list cap, does not
/// change with the library's default.
pub(crate) const MAX_FRAME_BYTES: u32 = 16 * 1024;

/// The downstream h2 server's settings: windows, concurrent streams, header and frame caps,
/// the flood limits and the keepalive.
pub(crate) fn h2_server_builder(limits: &Limits) -> http2::Builder<TokioExecutor> {
    let mut http = http2::Builder::new(TokioExecutor::new());
    http.initial_stream_window_size(STREAM_WINDOW_BYTES)
        .initial_connection_window_size(
            STREAM_WINDOW_BYTES.saturating_mul(DOWNSTREAM_CONNECTION_STREAMS),
        )
        .adaptive_window(false)
        .max_concurrent_streams(DOWNSTREAM_CONNECTION_STREAMS)
        .max_header_list_size(limits.max_header_list_bytes)
        .max_frame_size(MAX_FRAME_BYTES)
        .max_pending_accept_reset_streams(PENDING_RESET_STREAMS);
    keepalive::downstream(&mut http, limits);
    http
}

/// The upstream h2 client's settings: the same windows, and no server push (the children
/// never push, and a pushed stream would have no route).
pub(crate) fn h2_client_builder() -> h2::client::Builder {
    let mut builder = h2::client::Builder::new();
    builder
        .initial_window_size(STREAM_WINDOW_BYTES)
        .initial_connection_window_size(
            STREAM_WINDOW_BYTES.saturating_mul(UPSTREAM_CONNECTION_STREAMS),
        )
        .enable_push(false);
    builder
}
