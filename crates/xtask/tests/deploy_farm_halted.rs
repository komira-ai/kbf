//! `deploy-farm` against a coordinator that answers `halted`: the wait ends at once, as a
//! failure that names the host, the check and the version the host failed on. The cases run
//! through an in-memory transport (`deploy_with`): no socket, no TLS. The socket suites of
//! `deploy_farm.rs` cover the real transport.

use std::cell::RefCell;

use serde_json::{Value, json};
use xtask::deploy_farm::{Answer, Call, Error, TokenService, Transport, deploy_with};

#[path = "support/deploy_data.rs"]
mod deploy_data;
use deploy_data::{Time, farm, plan_for};

const BASE: &str = "https://coordinator.invalid";
const TOKENS: &str = "https://tokens.invalid";

/// A transport in memory: the token service answers a token, the coordinator answers the
/// scripted convergence reads (the first is the baseline, the last repeats) and accepts the
/// POST, and every call is recorded as (method, url).
struct Memory {
    reads: Vec<Value>,
    /// The coordinator's answers have a status but a body that cannot be read.
    unreadable: bool,
    seen: RefCell<Vec<(String, String)>>,
}

impl Memory {
    fn new(reads: Vec<Value>) -> Self {
        Self {
            reads,
            unreadable: false,
            seen: RefCell::new(Vec::new()),
        }
    }

    fn requests(&self, path: &str) -> usize {
        self.seen.borrow().iter().filter(|r| r.1 == path).count()
    }
}

impl Transport for Memory {
    fn call(&self, call: Call<'_>) -> Result<Answer, String> {
        let path = call.url.strip_prefix(BASE).unwrap_or(call.url).to_owned();
        let reads_so_far = self
            .seen
            .borrow()
            .iter()
            .filter(|r| r.0 == "GET" && r.1 == "/v1/convergence")
            .count();
        self.seen
            .borrow_mut()
            .push((call.method.to_owned(), path.clone()));
        let body = if path.starts_with(TOKENS) {
            json!({"value": "tok"})
        } else if call.method == "POST" {
            json!({})
        } else {
            self.reads[reads_so_far.min(self.reads.len() - 1)].clone()
        };
        if self.unreadable && !path.starts_with(TOKENS) {
            return Ok(Answer {
                status: 200,
                body: Err("the answer is too long".to_owned()),
            });
        }
        Ok(Answer {
            status: 200,
            body: Ok(body.to_string()),
        })
    }
}

fn run(memory: &Memory) -> (Result<(), Error>, String) {
    let service = TokenService {
        url: format!("{TOKENS}/token?api-version=2.0"),
        key: "runner-key".to_owned(),
    };
    let mut out = Vec::new();
    let result = deploy_with(
        &plan_for(BASE),
        &service,
        memory,
        &Time::default().clock(),
        &mut out,
    );
    (result, String::from_utf8(out).expect("utf8"))
}

const VERSION: &str = "2.0.0+g000000000002";

/// A farm whose coordinator says `halted`, with `halt` when there is one.
fn halted(halt: Option<Value>) -> Value {
    let mut body = farm("halted", 0, 1);
    if let Some(halt) = halt {
        body["halt"] = halt;
    }
    body
}

/// The requests of a run that ended at the halted read: the baseline read, the POST of the
/// desired version, one more read, and nothing after.
fn assert_stopped_at_the_halted_read(fake: &Memory) {
    let seen = fake.seen.borrow();
    let calls: Vec<(&str, &str)> = seen
        .iter()
        .filter(|r| !r.1.starts_with(TOKENS))
        .map(|r| (r.0.as_str(), r.1.as_str()))
        .collect();
    assert_eq!(
        calls,
        [
            ("GET", "/v1/convergence"),
            ("POST", "/v1/desired"),
            ("GET", "/v1/convergence"),
        ]
    );
}

/// Proves: `halted` ends the wait at the read that says it, as a failure that names the host,
/// the check and the version, after the table was printed; no read follows. Catches: an
/// unknown state that would only have stalled until the stall limit.
#[test]
fn a_halted_farm_fails_at_once_naming_the_host_and_the_check() {
    let fake = Memory::new(vec![
        farm("converged", 0, 0),
        halted(Some(
            json!({"node_id": "s1", "check": "disk_tainted", "target": VERSION}),
        )),
    ]);
    let (result, out) = run(&fake);
    let failure = result.expect_err("a halted farm is a failure").to_string();
    assert!(failure.starts_with("halted"), "{failure}");
    assert!(failure.contains("s1"), "{failure}");
    assert!(failure.contains("disk_tainted"), "{failure}");
    assert!(failure.contains(VERSION), "{failure}");
    assert_stopped_at_the_halted_read(&fake);
    assert_eq!(fake.requests("/v1/desired"), 1);
    assert!(
        out.contains(&format!("farm: halted, desired {VERSION}")),
        "{out}"
    );
}

/// Proves: a coordinator that says `halted` with no `halt` object is still a failure, and the
/// message says the host, the check and the version are not known instead of naming a blank.
/// Catches: a halted answer without the halt object that is read as a missing field and stalls
/// or panics.
#[test]
fn a_halted_answer_without_the_halt_still_fails() {
    let fake = Memory::new(vec![farm("converged", 0, 0), halted(None)]);
    let (result, _) = run(&fake);
    let failure = result.expect_err("a halted farm is a failure").to_string();
    assert!(
        failure.starts_with(
            "halted: unknown reported a failure no version can repair (unknown) while updating to unknown;"
        ),
        "{failure}"
    );
    assert_stopped_at_the_halted_read(&fake);
}

/// Proves: the version the failure names as the one the host failed on is the halt's own
/// `target`, even when the desired version has since become the deploy's (so the run is not
/// superseded). Catches: the deploy's version named as the one the host failed on, which sends
/// an operator looking at a release that never ran on that host.
#[test]
fn a_halt_names_the_version_the_host_failed_on_not_the_deploys() {
    const OLDER: &str = "1.0.0+g000000000001";
    let fake = Memory::new(vec![
        farm("converged", 0, 0),
        halted(Some(
            json!({"node_id": "s1", "check": "disk_tainted", "target": OLDER}),
        )),
    ]);
    let (result, _) = run(&fake);
    let failure = result.expect_err("a halted farm is a failure").to_string();
    assert!(failure.starts_with("halted: s1"), "{failure}");
    assert!(
        failure.contains(&format!("while updating to {OLDER}")),
        "{failure}"
    );
    assert!(
        !failure.contains(&format!("while updating to {VERSION}")),
        "{failure}"
    );
    assert_stopped_at_the_halted_read(&fake);
}

/// Proves: a baseline answer that has roles but no `state` is refused as an unreadable answer
/// that names the missing field, and nothing is set.
#[test]
fn an_answer_without_a_state_is_refused_before_anything_is_set() {
    let fake = Memory::new(vec![json!({"roles": []})]);
    let (result, _) = run(&fake);
    let failure = result.expect_err("an unreadable answer").to_string();
    assert!(failure.contains("no state"), "{failure}");
    assert_eq!(fake.requests("/v1/desired"), 0);
}

/// Proves: a coordinator answer whose body cannot be read ends the baseline read as a call
/// error that names the reason, and nothing is set.
#[test]
fn an_unreadable_answer_body_is_a_call_error() {
    let mut fake = Memory::new(vec![farm("converged", 0, 0)]);
    fake.unreadable = true;
    let (result, _) = run(&fake);
    let failure = result.expect_err("an unreadable body").to_string();
    assert!(failure.contains("the answer is too long"), "{failure}");
    assert_eq!(fake.requests("/v1/desired"), 0);
}
