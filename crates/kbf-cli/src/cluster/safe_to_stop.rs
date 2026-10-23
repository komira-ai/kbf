//! `kbf cluster safe-to-stop <id>`: the answer of `GET /v1/members/<id>/safe-to-stop`
//! (`docs/api.md`) as an exit status a script can branch on, with the reasons printed.
//!
//! Exit 0: safe to stop. Exit [`NOT_SAFE`]: not safe, the reasons on stdout. Exit 1: the
//! call failed or the answer is no verdict (an unknown id is 404, which is a failed call, so
//! a script never reads a typo as "not safe, try later"). Exit 2: the command line.

use std::io::Write;

use kbf_server::api::Verdict;
use kbf_server::api_client::Reply;

/// The exit status of a verdict that says the stop is not safe. Not 1: that is a call that
/// failed, which a script must not take for an answer.
pub const NOT_SAFE: u8 = 3;

/// Prints the verdict in `reply`, the 2xx answer for `id`, to `out`, and returns the exit
/// status it means. An answer that is no verdict is said on `err` and is exit 1.
pub fn report(id: &str, reply: &Reply, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let Ok(verdict) = serde_json::from_str::<Verdict>(&reply.body) else {
        let _ = writeln!(
            err,
            "kbf cluster: the answer is not a safe-to-stop verdict: {}",
            reply.body.trim_end()
        );
        return 1;
    };
    let word = if verdict.safe { "safe" } else { "not safe" };
    let _ = writeln!(out, "{word} to stop {id}");
    for reason in &verdict.reasons {
        let _ = writeln!(out, "- {reason}");
    }
    if verdict.planned_down {
        let _ = writeln!(out, "(the member is already planned down)");
    }
    if verdict.safe { 0 } else { NOT_SAFE }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAID: &str = "not a safe-to-stop verdict";

    fn reply(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.to_owned(),
        }
    }

    /// The exit status, stdout and stderr of `report` for `body`.
    fn reported(body: &str) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::<u8>::new(), Vec::<u8>::new());
        let code = report("7", &reply(body), &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("UTF-8"),
            String::from_utf8(err).expect("UTF-8"),
        )
    }

    /// Catches: a safe verdict that exits non-zero (an updater would never stop the host),
    /// or one that hides its reason line. Catches: the exit status inverted.
    #[test]
    fn a_safe_verdict_exits_0() {
        let (code, out, err) = reported(r#"{"safe":true,"reasons":[],"planned_down":false}"#);
        assert_eq!(code, 0);
        assert_eq!(out, "safe to stop 7\n");
        assert!(err.is_empty(), "{err}");
        let daemon = r#"{"safe":true,"reasons":["a daemon"],"planned_down":false}"#;
        assert_eq!(reported(daemon).1, "safe to stop 7\n- a daemon\n");
    }

    /// Catches: an unsafe verdict that exits 0 (a host stopped with a blob on one copy), one
    /// that exits 1 (taken for a failed call, not an answer), and reasons that are not all
    /// printed, in order, or a planned-down member that is not said. Catches: the
    /// exit status inverted; the reasons loop dropped.
    #[test]
    fn an_unsafe_verdict_exits_3_and_prints_every_reason() {
        let owing = "member m2 owes 5 blobs".to_owned();
        let down = "m3 is Down".to_owned();
        let verdict = Verdict {
            safe: false,
            reasons: vec![owing, down],
            planned_down: true,
        };
        let body = serde_json::to_string(&verdict).expect("JSON");
        let (code, out, err) = reported(&body);
        assert_eq!(code, NOT_SAFE);
        assert_eq!(NOT_SAFE, 3);
        let want = "not safe to stop 7\n- member m2 owes 5 blobs\n- m3 is Down\n\
                    (the member is already planned down)\n";
        assert_eq!(out, want);
        assert!(err.is_empty(), "{err}");
    }

    /// Catches: an answer that is no verdict taken for one (a proxy's page would read as
    /// safe): it exits 1, prints nothing on stdout, and says what it got on stderr.
    #[test]
    fn an_answer_that_is_no_verdict_exits_1() {
        for body in ["not json\n", r#"{"safe":true}"#, "{}", ""] {
            let (code, out, err) = reported(body);
            assert_eq!(code, 1, "{body:?}");
            assert!(out.is_empty(), "{body:?}: {out}");
            assert!(err.contains(SAID), "{body:?}: {err}");
        }
    }
}
