//! What `--executor js-agent` needs of the flags beside it, checked when the flags are parsed.
//!
//! The executor prepares each run through the frontend's [`kbf_front::prepare_run::RunPrep`]
//! (`--task-*`), which signs a write to the tier store and issues a read grant for the
//! job-facing listener (`--job-*`), and ingests the pack from the frontend's packs
//! (`--pack-store-*`). Every disagreement between those and the executor's own settings would
//! otherwise show up later, on the first run, as a failure of that run: a refused url, a read
//! that connects to nothing, a pack that is never found. Each is refused here, naming the
//! flags.
//!
//! The job network has no DNS, so a task dials the read listener and the tier store by IP
//! address, and both urls must say one.

use std::net::IpAddr;
use std::str::FromStr as _;
use std::time::Duration;

use axum::http::Uri;
use kbf_front::prepare_run::MAX_EXPIRY;

use super::executor_flags::JsAgentFlags;
use super::job_read::JobReadFlags;
use super::pack_flags::PackFlags;
use super::task_flags::TaskFlags;

/// The most a lease server grants a lease for unless it is started with another `--max-ttl`;
/// it clamps a longer ask to it.
const SERVER_MAX_TTL: Duration = Duration::from_secs(3600);

/// The settings `--executor js-agent` rests on, as the flags beside it give them.
pub(super) struct Beside<'a> {
    /// `--task-secret-file` and its companions.
    pub task: Option<&'a TaskFlags>,
    /// `--job-listen` and its companions.
    pub job_read: Option<&'a JobReadFlags>,
    /// `--pack-store-endpoint` and its companions.
    pub packs: Option<&'a PackFlags>,
}

/// Checks `js` against the flags beside it, and sets what follows from them: the presign
/// ceiling is the 4 hours a write may be good for, less the slack the preparation adds to every
/// run's wall time. Returns what is worth a warning, and not a refusal.
///
/// # Errors
/// A flag the executor cannot run without is missing (the packs, `--task-secret-file`,
/// `--job-listen`), the longest run's wall time does not fit the presign ceiling, the tier
/// store is `http://` and the executor was not told the plain write is accepted, the read url
/// or the tier store is not an IP address, the read url's scheme is not the one the job
/// listener serves, or the tier store's bucket is not the packs'.
pub(super) fn check(js: &mut JsAgentFlags, beside: &Beside<'_>) -> Result<Vec<String>, String> {
    let refuse = |why: &str| format!("--executor js-agent: {why}");
    let needs = |what: &str| format!("--executor js-agent needs {what}");
    let packs = beside.packs.ok_or_else(|| {
        needs("--pack-store-endpoint: the packs tasks write are read and checked through them")
    })?;
    let task = beside.task.ok_or_else(|| {
        needs(
            "--task-secret-file (and --task-put-endpoint, --task-put-bucket, \
             --task-put-credentials-file, --task-read-url): the key, the write and the read \
             grant of each run are made from them",
        )
    })?;
    let Some(job) = beside.job_read else {
        return Err(needs(
            "--job-listen (and --job-tls-cert and --job-tls-key, or --job-plain-http, and \
             --job-grant-secret-file): without the listener every read grant points at a port \
             nobody serves",
        ));
    };
    // The task dials the scheme of the read url, and the listener speaks the other one.
    match (job.tls.is_some(), task.read_url.starts_with("http://")) {
        (true, true) => {
            return Err(refuse(
                "--task-read-url is http:// but --job-listen serves TLS: a task would send \
                 plain HTTP to a TLS port; use https://, or serve --job-listen with \
                 --job-plain-http instead of --job-tls-cert",
            ));
        }
        (false, false) => {
            return Err(refuse(
                "--task-read-url is https:// but --job-plain-http serves no TLS: a task would \
                 start a handshake the listener cannot answer; use http:// (with \
                 --executor-allow-plain-http-grant), or serve --job-listen over TLS",
            ));
        }
        _ => {}
    }
    let mut warnings = Vec::new();

    let config = &mut js.config;
    config.presign_ceiling = MAX_EXPIRY.saturating_sub(task.url_slack);
    let longest = config.max_run + config.run_margin;
    if longest > config.presign_ceiling {
        return Err(refuse(&format!(
            "--executor-max-run-secs plus --executor-run-margin-secs ({} s) must be at most \
             14400 s less --task-url-slack-secs ({} s), the longest a presigned write may \
             be good for once the slack is added",
            longest.as_secs(),
            task.url_slack.as_secs(),
        )));
    }
    config.validate().map_err(|e| refuse(&e.to_string()))?;

    let put_https = task.put_endpoint.starts_with("https://");
    let put_http = task.put_endpoint.starts_with("http://");
    if !put_https && !put_http {
        return Err(refuse(
            "--task-put-endpoint must start with https:// (or http://, with \
             --executor-allow-plain-http-put)",
        ));
    }
    match (put_https, config.allow_plain_http_put) {
        (false, false) => {
            return Err(refuse(
                "--task-put-endpoint is http://, which a task refuses to write to: serve the \
                 tier store over TLS (https://), or give --executor-allow-plain-http-put",
            ));
        }
        (true, true) => warnings.push(
            "--executor-allow-plain-http-put is set and --task-put-endpoint is https://: the \
             flag is not needed"
                .to_owned(),
        ),
        _ => {}
    }
    if config.allow_plain_http_grant && task.read_url.starts_with("https://") {
        warnings.push(
            "--executor-allow-plain-http-grant is set and --task-read-url is https://: the \
             flag is not needed"
                .to_owned(),
        );
    }
    for (flag, url) in [
        ("--task-read-url", &task.read_url),
        ("--task-put-endpoint", &task.put_endpoint),
    ] {
        if host_ip(url).is_none() {
            return Err(refuse(&format!(
                "{flag} {url:?} is not an IP address: the job network has no DNS, so a task \
                 cannot look a name up"
            )));
        }
    }
    if task.put_bucket != packs.bucket {
        return Err(refuse(&format!(
            "--task-put-bucket {:?} is not --pack-store-bucket {:?}: a task writes its pack to \
             the first and the frontend reads it from the second",
            task.put_bucket, packs.bucket
        )));
    }
    if config.lease_ttl > SERVER_MAX_TTL {
        warnings.push(format!(
            "--executor-lease-ttl-secs {} is more than the {} s a lease server grants unless \
             it is started with a longer --max-ttl; it clamps the ask, and a lease may then \
             end sooner than the executor expects",
            config.lease_ttl.as_secs(),
            SERVER_MAX_TTL.as_secs()
        ));
    }
    Ok(warnings)
}

/// The IP address `url` names as its host, if it names one (`https://192.0.2.1:9`,
/// `https://[::1]:9`).
fn host_ip(url: &str) -> Option<IpAddr> {
    let host = Uri::from_str(url).ok()?.host()?.to_owned();
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
    IpAddr::from_str(bare.unwrap_or(&host)).ok()
}

#[cfg(test)]
#[path = "executor_bind_tests.rs"]
mod tests;
