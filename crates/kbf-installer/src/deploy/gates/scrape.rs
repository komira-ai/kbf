//! One scrape of `/metrics` through the operator API client.

use kbf_gate::Metrics;

use crate::deploy::Run;
use crate::deploy::api::Request;

/// GET `/metrics` and parse it. A status other than 200, a body that is not text, a line
/// that is not a sample: each is an error, which the gates read as every series absent.
pub(super) fn read(run: &Run) -> Result<Metrics, String> {
    let reply = run
        .observe_send(&Request::get("/metrics"))
        .map_err(|e| format!("scrape failed: GET /metrics: {e}"))?;
    if reply.status != 200 {
        return Err(format!(
            "scrape failed: GET /metrics: status {}",
            reply.status
        ));
    }
    let text =
        std::str::from_utf8(&reply.body).map_err(|_| "scrape failed: not text".to_owned())?;
    Metrics::parse(text).map_err(|e| format!("scrape failed: {e}"))
}
