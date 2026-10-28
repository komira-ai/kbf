//! `GET /v1/nodes/{id}`: one node, as the list of `GET /v1/nodes` has it, with what it
//! reported it can run, what is left of that, and the leases it holds
//! ([`crate::fleet::NodeDetail`]).
//!
//! The route is the one `POST /v1/nodes/{node}:{verb}` takes: its `{target}` is a whole
//! path segment, so a verb after a colon would be read as part of the id. A read refuses
//! one: `GET /v1/nodes/a:drain` is `404`, never node `a` and never a node named
//! `a:drain`, since that is the path an operator's write takes and a read of it must not
//! look like one. An id that holds another colon is read as it is (`host:8080`).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use kbf_types::WorkerId;

use super::{ApiState, error, json};

/// The verbs a `POST /v1/nodes/{node}:{verb}` takes (`write_request`), which a `GET`
/// refuses at the end of an id.
const VERBS: [&str; 3] = ["cordon", "drain", "uncordon"];

/// The `error` of every `404` of this route: no node by that name, or a name that is a
/// write's path.
const NO_NODE: &str = "no_node";

/// Whether `target` is `{node}:{verb}`, the path of a write.
fn is_write_path(target: &str) -> bool {
    target
        .rsplit_once(':')
        .is_some_and(|(_, verb)| VERBS.contains(&verb))
}

pub(super) async fn one(State(api): State<Arc<ApiState>>, Path(target): Path<String>) -> Response {
    if is_write_path(&target) {
        return error(StatusCode::NOT_FOUND, NO_NODE);
    }
    match api.farm.node_detail(&WorkerId::new(&target)) {
        Ok(Some(detail)) => json(StatusCode::OK, &detail),
        Ok(None) => error(StatusCode::NOT_FOUND, NO_NODE),
        Err(unread) => error(StatusCode::SERVICE_UNAVAILABLE, &unread.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a verb added to `write_request` that a read of `{node}:{verb}` then
    /// answers as a node, or a read that refuses an id that only looks like a verb.
    #[test]
    fn only_a_write_verb_after_the_last_colon_makes_a_write_path() {
        for verb in ["cordon", "drain", "uncordon"] {
            assert!(is_write_path(&format!("a:{verb}")), "{verb}");
            assert!(is_write_path(&format!("host:8080:{verb}")), "{verb}");
        }
        for id in [
            "a",
            "a:",
            ":drain-not",
            "host:8080",
            "a:Drain",
            "drain",
            "a:cordon:x",
        ] {
            assert!(!is_write_path(id), "{id}");
        }
        assert!(is_write_path(":drain"), "an empty node with a verb");
    }
}
