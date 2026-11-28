use js_proto::v1::Callback;
use tonic::Code;

use super::*;

fn with(url: &str, key: usize) -> JobSpec {
    JobSpec {
        callback: Some(Callback {
            url: url.to_owned(),
            signing_key: vec![1; key],
            deadline_seconds: None,
        }),
        ..JobSpec::default()
    }
}

/// Asserts the bounds of the checks, below the client: a spec with no callback passes, a key
/// of [`MIN_KEY`] and [`MAX_KEY`] bytes passes and one byte either side does not, and the URL
/// check runs first, so a plain `http` URL with a bad key is refused for its URL. Catches a
/// bound off by one and a check that names the key when the URL is wrong.
#[test]
fn the_checks_hold_at_their_bounds_and_the_url_is_checked_first() {
    assert!(check(&JobSpec::default()).is_ok());
    assert!(check(&with("https://h/r", MIN_KEY)).is_ok());
    assert!(check(&with("https://h/r", MAX_KEY)).is_ok());
    for key in [MIN_KEY - 1, MAX_KEY + 1] {
        let refused = check(&with("https://h/r", key)).unwrap_err();
        assert_eq!(refused.code(), Code::InvalidArgument);
        assert_eq!(
            refused.message(),
            "field: spec.callback.signingKey: bad-length"
        );
    }
    let refused = check(&with("http://h/r", 0)).unwrap_err();
    assert_eq!(refused.message(), "field: spec.callback.url: not-https");
}

/// Asserts what a read shows of a callback: the key emptied and the URL and deadline kept,
/// `ARMED` for a live job and `PENDING` for an ended one, and nothing for a spec with no
/// callback, which is left as it was. Catches a status that ignores the job's state, and a
/// key left in the answer.
#[test]
fn a_read_shows_the_callback_without_its_key() {
    for (live, state) in [
        (true, CallbackState::Armed),
        (false, CallbackState::Pending),
    ] {
        let mut spec = with("https://h/r", MIN_KEY);
        let status = shown(&mut spec, live).unwrap();
        assert_eq!(status.state(), state);
        assert_eq!((status.tries, status.last_failure.as_str()), (0, ""));
        let callback = spec.callback.unwrap();
        assert_eq!(callback.url, "https://h/r");
        assert!(callback.signing_key.is_empty());
    }
    let mut plain = JobSpec::default();
    assert_eq!(shown(&mut plain, true), None);
    assert_eq!(plain, JobSpec::default());
}
