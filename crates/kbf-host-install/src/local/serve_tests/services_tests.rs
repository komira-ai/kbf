//! The socket with services other than kbf behind it: which requests reach them, what is
//! audited, and that kbf's lines are as they were.

use std::cell::RefCell;

use serde_json::{Value, json};

use super::{VB, audit_lines, with_services, with_serving};
use crate::local::answer::{Answer, Code};
use crate::local::request::LocalRequest;
use crate::local::services::Services;
use crate::pace::Pace;
use crate::service::ServiceName;

/// A stand-in for the services: it records what it was asked and answers as it was told.
#[derive(Default)]
struct Recorder {
    asked: RefCell<Vec<(String, String)>>,
    /// What a tick found: the service, and how its revert ended.
    ticks: RefCell<Vec<(ServiceName, Answer)>>,
}

impl Services for Recorder {
    fn handle(&self, service: &ServiceName, request: &LocalRequest) -> Answer {
        self.asked
            .borrow_mut()
            .push((service.as_str().to_owned(), request.verb().to_owned()));
        match request {
            LocalRequest::Confirm { version } => Answer::Confirmed {
                version: version.as_str().to_owned(),
            },
            _ => Answer::Refused(Code::NothingPending, "stand-in".to_owned()),
        }
    }

    fn tick(&self) -> Vec<(ServiceName, Answer)> {
        std::mem::take(&mut *self.ticks.borrow_mut())
    }
}

fn asked(recorder: &Recorder) -> Vec<(String, String)> {
    recorder.asked.borrow().clone()
}

/// Every line kbf's agent sends today is answered as before, and its audit line has no
/// `service` key: the line of a service is the only one that does.
#[test]
fn todays_requests_get_todays_answers_and_audit_lines() {
    with_serving("sv-compat", |fx, wire| {
        assert_eq!(wire.ask("query")["result"], "query");
        assert_eq!(
            wire.ask(&format!("install kbf={VB} from stable")),
            json!({"result":"staged","version":VB,"commit":super::B,"current":false})
        );
        assert_eq!(wire.ask(&format!("switch kbf={VB}"))["result"], "switched");
        assert_eq!(
            wire.ask(&format!("confirm kbf={VB}")),
            json!({"result":"confirmed","version":VB})
        );
        assert_eq!(
            wire.ask("revert"),
            json!({"result":"refused","reason":"nothing_pending"})
        );
        // `query kbf` names the same service and answers the same.
        assert_eq!(wire.ask("query kbf")["result"], "query");
        let lines = audit_lines(fx);
        assert_eq!(lines.len(), 4, "the queries leave no line");
        assert!(
            lines.iter().all(|l| l.get("service").is_none()),
            "{lines:?}"
        );
    });
}

/// A request for another service goes to the services and not to kbf's backend; its answer is
/// the one line it gives, its audit line names the service, and the other fields are those of
/// a kbf line of the same verb.
#[test]
fn a_request_for_another_service_reaches_the_services_and_is_audited_with_its_name() {
    let recorder = Recorder::default();
    with_services("sv-svc", &Pace::real(), &recorder, |fx, wire| {
        let calls = fx.calls().len();
        assert_eq!(
            wire.ask(&format!("confirm js-agent={VB}")),
            json!({"result":"confirmed","version":VB})
        );
        assert_eq!(
            wire.ask(&format!("install js-agent={VB} from js-stable")),
            json!({"result":"refused","reason":"nothing_pending"})
        );
        assert_eq!(
            wire.ask("query js-agent"),
            json!({"result":"refused","reason":"nothing_pending"})
        );
        assert_eq!(
            wire.ask("revert js-agent"),
            json!({"result":"refused","reason":"nothing_pending"})
        );
        assert_eq!(fx.calls().len(), calls, "kbf's backend was not asked");
        let lines = audit_lines(fx);
        // The query leaves no line; the others name the service, the verb, the version and
        // (for the install) the channel.
        let seen: Vec<(&str, &str)> = lines
            .iter()
            .map(|l| (l["verb"].as_str().unwrap(), l["service"].as_str().unwrap()))
            .collect();
        assert_eq!(
            seen,
            [
                ("confirm", "js-agent"),
                ("stage", "js-agent"),
                ("revert", "js-agent")
            ]
        );
        assert_eq!(lines[0]["version"], VB);
        assert_eq!(lines[1]["channel"], "js-stable");
        assert_eq!(lines[0]["outcome"], "confirmed");
    });
    let verbs: Vec<String> = asked(&recorder).into_iter().map(|(_, v)| v).collect();
    assert_eq!(verbs, ["confirm", "stage", "query", "revert"]);
    assert!(asked(&recorder).iter().all(|(s, _)| s == "js-agent"));
}

/// A service name that is not one, and a verb that has no service form, are refused as any
/// request that is not in the grammar is, and recorded.
#[test]
fn a_service_form_outside_the_grammar_is_a_bad_command() {
    let recorder = Recorder::default();
    with_services("sv-svc-bad", &Pace::real(), &recorder, |fx, wire| {
        let refused = json!({"result":"refused","reason":"bad_command"});
        for line in [
            "query Js",
            "query js agent",
            "revert js-",
            "restart js-agent=-",
            &format!("install JS={VB} from stable"),
            &format!("switch js-agent={VB} from stable"),
            &format!("confirm js-agent {VB}"),
        ] {
            assert_eq!(wire.ask(line), refused, "{line:?}");
        }
        assert_eq!(audit_lines(fx).len(), 7);
    });
    assert!(asked(&recorder).is_empty());
}

/// A host that serves kbf alone refuses every other service, and nothing runs.
#[test]
fn a_helper_without_services_refuses_every_other_service() {
    with_serving("sv-none", |fx, wire| {
        let calls = fx.calls().len();
        let refused = json!({"result":"refused","reason":"service_not_configured"});
        for line in [
            "query js-agent".to_owned(),
            "revert js-agent".to_owned(),
            format!("install js-agent={VB} from stable"),
            format!("switch js-agent={VB}"),
            format!("confirm js-agent={VB}"),
        ] {
            assert_eq!(wire.ask(&line), refused, "{line}");
        }
        assert_eq!(fx.calls().len(), calls);
        assert_eq!(fx.query().commit.as_deref(), Some(super::A));
    });
}

/// Without the audit log a service's install, switch and confirm do not run either; its query
/// and its revert still do.
#[test]
fn without_the_audit_log_a_service_runs_only_the_query_and_the_revert() {
    let recorder = Recorder::default();
    with_services("sv-svc-noaudit", &Pace::real(), &recorder, |fx, wire| {
        std::fs::remove_file(fx.dir.join("audit.log")).ok();
        std::fs::create_dir(fx.dir.join("audit.log")).unwrap();
        let unavailable = json!({"result":"failed","reason":"audit_unavailable"});
        assert_eq!(
            wire.ask(&format!("install js-agent={VB} from stable")),
            unavailable
        );
        assert_eq!(wire.ask(&format!("switch js-agent={VB}")), unavailable);
        assert_eq!(wire.ask(&format!("confirm js-agent={VB}")), unavailable);
        assert!(asked(&recorder).is_empty(), "nothing was run");
        wire.ask("query js-agent");
        wire.ask("revert js-agent");
    });
    let verbs: Vec<String> = asked(&recorder).into_iter().map(|(_, v)| v).collect();
    assert_eq!(verbs, ["query", "revert"]);
}

/// The tick asks the services too: a revert one of them made is logged with its name,
/// audited as a revert of that service, and a revert that did not work is a failure the loop
/// holds off retrying (the tick says so).
#[test]
fn a_services_deadline_is_kept_and_audited_with_its_name() {
    let recorder = Recorder::default();
    let logged = with_services("sv-svc-tick", &Pace::real(), &recorder, |fx, wire| {
        assert!(!wire.serving.tick(), "nothing to do");
        recorder.ticks.borrow_mut().push((
            ServiceName::new("js-agent").unwrap(),
            Answer::Reverted { version: None },
        ));
        assert!(!wire.serving.tick(), "the revert worked");
        recorder.ticks.borrow_mut().push((
            ServiceName::new("js-agent").unwrap(),
            Answer::Failed(Code::RevertFailed, "stand-in".to_owned()),
        ));
        assert!(wire.serving.tick(), "the revert failed");
        let lines = audit_lines(fx);
        let fields: Vec<(&str, &str, &str)> = lines
            .iter()
            .map(|l| {
                (
                    l["verb"].as_str().unwrap(),
                    l["service"].as_str().unwrap(),
                    l["outcome"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            fields,
            [
                ("revert", "js-agent", "reverted"),
                ("revert", "js-agent", "failed")
            ]
        );
        assert_eq!(lines[1]["reason"], Value::from("revert_failed"));
    });
    assert!(
        logged
            .iter()
            .any(|l| l.contains("the deadline passed (js-agent)") && l.contains("reverted")),
        "{logged:?}"
    );
}
