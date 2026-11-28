//! `--executor-callback-origin`: the origin each job's completion call is sent to.

use super::{js_with, refused};

/// Asserts that a js-agent executor given no `--executor-callback-origin` asks for no call,
/// and that one given an https origin keeps it as written, its scheme in lower case. Catches a
/// default that sends a callback to a job service that may refuse it, and an origin lost
/// between the flag and the executor's settings.
#[test]
fn the_callback_origin_is_unset_by_default_and_kept_when_given() {
    let flags = js_with(&[]).expect("flags");
    let config = flags.executor.js.expect("js-agent").config;
    assert_eq!(config.callback_origin, None);

    let flags = js_with(&["--executor-callback-origin=HTTPS://kbf.example:8443"]).expect("flags");
    let config = flags.executor.js.expect("js-agent").config;
    let origin = config.callback_origin.map(|origin| origin.to_string());
    assert_eq!(origin.as_deref(), Some("https://kbf.example:8443"));
}

/// Asserts that an origin the job service would refuse at every submit is refused at start,
/// naming the flag and the rule: plain http, and a path after the host. Catches a frontend
/// that starts with an origin every job's submit would then fail on.
#[test]
fn a_callback_origin_that_is_not_an_https_origin_is_refused_at_start() {
    for (given, rule) in [
        (
            "--executor-callback-origin=http://kbf.example",
            "--executor-callback-origin: the callback origin must start with https://",
        ),
        (
            "--executor-callback-origin=https://kbf.example/js-done",
            "--executor-callback-origin: the callback origin is https://HOST or \
             https://HOST:PORT (a port of 1 to 65535), with no user, path, query or fragment",
        ),
    ] {
        let why = refused(&[given]);
        assert!(why.contains(rule), "{given}: {why}");
    }
}
