//! A job's completion callback as the fake keeps it: the service's checks at submit, the key
//! never answered, and the delivery status a read shows.
//!
//! The fake sends no call: a test that wants one sends it itself, signed with the key the job
//! was submitted with ([`FakeJobServer::submitted`](super::FakeJobServer::submitted) keeps it).

use js_proto::v1::{CallbackState, CallbackStatus, JobSpec};
use tonic::Status;

/// The shortest signing key the service takes, in bytes.
pub(super) const MIN_KEY: usize = 32;
/// The longest signing key the service takes, in bytes.
pub(super) const MAX_KEY: usize = 64;

/// The service's checks of `spec`'s callback that the fake keeps: an `https` URL and a key of
/// [`MIN_KEY`] to [`MAX_KEY`] bytes, each refused `INVALID_ARGUMENT` with the service's text,
/// which repeats neither the URL nor the key. A spec with no callback passes.
pub(super) fn check(spec: &JobSpec) -> Result<(), Status> {
    let Some(callback) = &spec.callback else {
        return Ok(());
    };
    let https = callback
        .url
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
    if !https {
        return Err(Status::invalid_argument(
            "field: spec.callback.url: not-https",
        ));
    }
    if !(MIN_KEY..=MAX_KEY).contains(&callback.signing_key.len()) {
        return Err(Status::invalid_argument(
            "field: spec.callback.signingKey: bad-length",
        ));
    }
    Ok(())
}

/// The callback of `spec` as a read answers it: its key emptied in `spec`, and the status of
/// its delivery, `ARMED` while the job is `live` and `PENDING` once it has ended (the fake
/// delivers nothing). `None`, and `spec` unchanged, for a job with no callback.
pub(super) fn shown(spec: &mut JobSpec, live: bool) -> Option<CallbackStatus> {
    let callback = spec.callback.as_mut()?;
    callback.signing_key.clear();
    let state = if live {
        CallbackState::Armed
    } else {
        CallbackState::Pending
    };
    Some(CallbackStatus {
        state: state.into(),
        ..CallbackStatus::default()
    })
}

#[cfg(test)]
#[path = "callback_tests.rs"]
mod tests;
