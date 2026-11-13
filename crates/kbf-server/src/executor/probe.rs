//! The job service as `/readyz` asks about it (a frontend with no storage node, whose only other
//! dependency is the state store).

use std::time::Duration;

use futures::future::BoxFuture;
use kbf_job_client::Client;

use super::api::{Api, answered};
use crate::health::Probe;

/// The job id the probe reads. No job has it, so the service answers "no such job" from its own
/// index and the call writes nothing.
const PROBE_JOB: &str = "kbf-readyz-probe";

/// One read of [`PROBE_JOB`]: the check passes when the job service answers about it, whatever
/// it says (see [`answered`]), and fails when the call does not reach the service, is not
/// answered within the limit, or the service will not serve this client at all (a revoked
/// token).
pub struct ServiceProbe {
    client: Client,
    limit: Duration,
}

impl ServiceProbe {
    pub(super) const fn new(client: Client, limit: Duration) -> Self {
        Self { client, limit }
    }
}

impl Probe for ServiceProbe {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let api = Api {
                client: &self.client,
                limit: self.limit,
            };
            let answer = api.get_job(PROBE_JOB).await;
            match answered(&answer) {
                true => Ok(()),
                false => Err(format!(
                    "the job service did not answer a read of a job: {}",
                    answer.err().map_or_else(String::new, |e| e.to_string())
                )),
            }
        })
    }
}
