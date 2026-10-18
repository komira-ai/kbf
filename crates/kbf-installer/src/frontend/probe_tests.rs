use std::time::Duration;

use super::*;
use crate::fixtures::{FakeRunner, ran};
use kbf_host::testkit::FakeExec;

fn url() -> HttpUrl {
    HttpUrl::new("http://127.0.0.1:18081/readyz").expect("url")
}

/// Catches readiness decided by anything but a 2xx answer: 200 and 204 are ready; a 503
/// (up and not ready), a redirect, and a connection that fails are not.
///
/// MUTANT-TODO: `status != 503` (a refused connection, or a 404, read as ready).
#[test]
fn only_a_2xx_answer_is_ready() {
    let mut exec = FakeExec::new(0);
    for (answer, want) in [
        (Ok(200), true),
        (Ok(204), true),
        (Ok(503), false),
        (Ok(404), false),
        (Ok(302), false),
        (Err(std::io::ErrorKind::ConnectionRefused), false),
    ] {
        exec.http = answer;
        assert_eq!(ready(&exec, &url()), want, "{answer:?}");
    }
}

fn probe(
    answer: impl Fn(&Spawn) -> std::io::Result<crate::run::Ran> + 'static,
) -> RunnerProbe<FakeRunner> {
    RunnerProbe {
        runner: FakeRunner::new(answer),
        platform: Platform::Linux,
    }
}

const JSON: &str = "{\"commit\":\"0123456789ab\",\"handover_protocol\":1,\"min_protocol\":1,\
                    \"profile\":\"release\",\"protocol\":1,\"state_schema\":1,\"version\":\"0.1.0\"}\n";

/// Catches the binary being run as root or with the caller's environment, or without its
/// time bound: it is `--version --json` by absolute path, as the unprivileged user, in `/`,
/// with an empty environment and ten seconds; and what it reports is read as the protocol.
#[test]
fn the_binary_is_run_unprivileged_with_an_empty_environment_and_names_its_protocol() {
    let p = probe(|_| Ok(ran(0, JSON, "")));
    assert_eq!(p.handover_protocol(Path::new("/r/kbf")), Ok(1));
    let calls = p.runner.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.program, Path::new("/r/kbf"));
    assert_eq!(call.args, ["--version", "--json"]);
    assert!(call.env.is_empty());
    assert_eq!(call.cwd, Path::new("/"));
    assert_eq!(call.run_as, Some(nobody(Platform::Linux)));
    assert_eq!(call.timeout, Duration::from_secs(10));
}

/// Catches a binary that cannot say being taken for one that speaks protocol 0 or 1: one
/// that does not start, exits non-zero, times out, prints something else, or prints an
/// identity with no `handover_protocol` (a release from before it was reported) is an error,
/// so the update is refused before anything starts.
///
/// MUTANT-TODO: a missing field defaulted to the current protocol.
#[test]
fn a_binary_that_cannot_name_its_protocol_is_an_error() {
    let old = JSON.replace("\"handover_protocol\":1,", "");
    for (name, answer) in [
        ("does not run", Err(std::io::Error::other("no such file"))),
        ("exits 1", Ok(ran(1, JSON, ""))),
        (
            "times out",
            Ok(crate::run::Ran {
                code: None,
                stdout: JSON.as_bytes().to_vec(),
                stderr: Vec::new(),
                timed_out: true,
            }),
        ),
        ("prints text", Ok(ran(0, "kbf 0.1.0\n", ""))),
        ("has no field", Ok(ran(0, &old, ""))),
    ] {
        let answer = std::cell::RefCell::new(Some(answer));
        let p = probe(move |_| answer.borrow_mut().take().expect("once"));
        assert!(p.handover_protocol(Path::new("/r/kbf")).is_err(), "{name}");
    }
}

/// Catches a readiness probe that could be pointed off the host: only a numeric loopback
/// address is taken (IPv4 or IPv6), and a name, another address, a missing port or `https`
/// is refused with the text it was given.
///
/// MUTANT-TODO: the loopback test dropped.
#[test]
fn a_readiness_url_must_be_numeric_loopback() {
    for ok in [
        "http://127.0.0.1:8081/readyz",
        "http://[::1]:8081/readyz",
        "http://127.0.0.2:1/",
    ] {
        assert!(loopback(ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://localhost:8081/readyz",
        "http://192.0.2.1:8081/readyz",
        "http://example.com/readyz",
        "https://127.0.0.1:8081/readyz",
        "http://127.0.0.1:0/readyz",
        "127.0.0.1:8081",
    ] {
        assert!(loopback(bad).is_err(), "{bad}");
    }
}
