//! The farm's metrics endpoint, read with a plain blocking GET. The scrape needs no
//! credential: the endpoint is open and read-only.

use std::time::Duration;

/// GET `url` and return the body, or why it could not be read (a connection error, a status
/// that is not a success, a body that is not text).
pub fn http_get(url: &str, timeout: Duration) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into();
    let mut response = agent.get(url).call().map_err(|e| e.to_string())?;
    response
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}
