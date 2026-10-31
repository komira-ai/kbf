//! A limit for a role the service does not have is dropped (one body serves every service),
//! and the drop is reported: a warning in the log and the role names in the answer.

use std::sync::{Arc, Mutex};

use crate::desired::Desired;
use crate::services::{RoleDef, ServiceDef, Services};
use crate::testutil::{Rig, ci_token};

fn services() -> Services {
    Services::default().with(ServiceDef::new("pool", &[("builder", RoleDef::plain())]))
}

/// A body for `kbf` with limits for `builder`, a role only `pool` has.
const WITH_BUILDER: &str = r#"{"version":"1.0","max_updating":{"builder":3,"daemon":1},
    "min_up":{"storage":1,"frontend":1,"builder":2}}"#;

const WITHOUT_BUILDER: &str = r#"{"version":"1.0","max_updating":{"daemon":1},
    "min_up":{"storage":1,"frontend":1}}"#;

/// What a subscriber wrote.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Log {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("lock").clone()).expect("UTF-8")
    }
}

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// The names come back sorted and once each, for roles dropped from both maps, and none for
/// a body that keeps every limit.
#[test]
fn the_dropped_roles_are_named_sorted_and_once() {
    let body = br#"{"version":"1.0","max_updating":{"zed":1,"builder":3},
        "min_up":{"storage":1,"frontend":1,"builder":2,"zed":1}}"#;
    let services = services().with(ServiceDef::new("other", &[("zed", RoleDef::plain())]));
    let (_, _, dropped) = Desired::parse_request_noting(body, &services).expect("accepted");
    assert_eq!(dropped, ["builder", "zed"]);
    let (_, _, dropped) =
        Desired::parse_request_noting(WITHOUT_BUILDER.as_bytes(), &services).expect("accepted");
    assert!(dropped.is_empty());
}

/// A drop is a warning naming the service and the roles, and a body that drops nothing logs
/// no warning.
#[test]
fn a_dropped_limit_is_a_warning_naming_the_roles() {
    let log = Log::default();
    let _installed = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    Desired::parse_request_noting(WITHOUT_BUILDER.as_bytes(), &services()).expect("accepted");
    assert_eq!(log.text(), "");
    Desired::parse_request_noting(WITH_BUILDER.as_bytes(), &services()).expect("accepted");
    let text = log.text();
    assert!(text.contains("WARN"), "{text}");
    assert!(text.contains("builder") && text.contains("kbf"), "{text}");
}

/// The answer to the POST carries `dropped_roles` after the state's own fields when roles
/// were dropped, and is byte for byte the state alone when none were.
#[tokio::test]
async fn the_answer_names_the_dropped_roles_and_is_unchanged_without_any() {
    let mut rig = Rig::new("desired-dropped");
    rig.coordinator = Rig::start(&rig.dir, &rig.clock).with_services(services());
    let answer = |body: &str| {
        let coordinator = &rig.coordinator;
        let token = ci_token();
        let body = body.to_owned();
        async move {
            coordinator
                .set_desired_noting(Some(&token), body.as_bytes())
                .await
                .expect("set")
        }
    };
    let plain = answer(WITHOUT_BUILDER).await;
    assert!(plain.dropped_roles.is_empty());
    let state = serde_json::to_string(&plain.desired).expect("json");
    assert_eq!(serde_json::to_string(&plain).expect("json"), state);

    let dropped = answer(WITH_BUILDER).await;
    assert_eq!(dropped.dropped_roles, ["builder"]);
    let expected = format!(
        r#"{},"dropped_roles":["builder"]}}"#,
        &state[..state.len() - 1]
    );
    assert_eq!(serde_json::to_string(&dropped).expect("json"), expected);
}
