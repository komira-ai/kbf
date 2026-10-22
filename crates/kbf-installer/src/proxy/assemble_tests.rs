//! What the verb is given to supervise the swap with, and what answers on the update route:
//! with the proxy's own copies of `kbf` the swap is wired to them and the route answers 410;
//! without them the update API is built as it has always been.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use kbf_proxy::control::{UpdateHandler, UpdateRequest};

use super::*;
use crate::proxy::handover_swap::{GateSlot, NOT_BOUND, SlotAddrs};
use crate::proxy::kinds::Supervisor;
use crate::proxy::releases;
use crate::testkit::A;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A configuration over a scratch root, without copies: the state directory exists, and the
/// order spool and the root half's directory do not.
fn config(name: &str) -> HandoverConfig {
    let root = kbf_host::testkit::scratch(&format!("proxy-assemble-{name}"));
    let state_dir = root.join("state");
    fs::create_dir(&state_dir).expect("state dir");
    HandoverConfig {
        state_dir,
        orders_dir: root.join("orders"),
        status_file: root.join("update").join("status.json"),
        local: None,
        confirm_window_s: 900,
        fetch_timeout_s: 900,
        slots: SlotAddrs {
            admin: [addr(18003), addr(18013)],
            operator: [addr(18002), addr(18012)],
        },
        installer_toml: root.join("installer.toml"),
        gate: GateSlot::new(),
    }
}

/// A configuration for a proxy that keeps its own copies: the status file is its
/// `releases.json`, in the state directory with the copies.
fn config_local(name: &str) -> HandoverConfig {
    let mut cfg = config(name);
    let local = Local::open(PathBuf::from("/usr/bin/kbf"), &cfg.state_dir).expect("copies");
    cfg.status_file = cfg.state_dir.join(releases::STATUS);
    cfg.local = Some(local);
    cfg
}

/// The names in `dir`, in order.
fn names_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("a directory")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort_unstable();
    names
}

/// Catches the swap wired to anything but the proxy's own copies, and the update API's seams
/// built for a proxy that has none to use them: with a file where `orders/` would be, the
/// wiring is built; it is the config's `Local` (the same binary, the same copies, the same
/// orders, not new ones); the swap in it is the unbound supervisor, waiting for the gate; and
/// nothing is made in the state directory but the copies' own directory (no counter, no request
/// map, no record).
///
/// Asserts that the wiring is over the `Local` it was given (its orders are the config's), that
/// the spool is not opened (the file in the way is not an error), and that the request map and
/// the counter are not opened.
#[test]
fn with_copies_the_swap_is_wired_to_them_and_no_spool_or_api_is_made() {
    let cfg = config_local("copies");
    fs::write(&cfg.orders_dir, b"a file").expect("a file in the way");
    let own = cfg.local.clone().expect("copies");

    let Ok(assembled) = assemble(&cfg) else {
        panic!("there is no spool to fail on");
    };

    let wiring = assembled.watch.expect("a proxy with copies is watched");
    assert_eq!(wiring.local.binary, own.binary);
    assert!(Arc::ptr_eq(&wiring.local.copies, &own.copies));
    assert!(Arc::ptr_eq(&wiring.local.orders, &own.orders));
    assert_eq!(wiring.swap.start_blocked().as_deref(), Some(NOT_BOUND));
    assert_eq!(fs::read(&cfg.orders_dir).expect("untouched"), b"a file");
    assert_eq!(names_in(&cfg.state_dir), [releases::RELEASES]);
}

/// Catches the binary watch handed to a proxy that does not keep copies, or the update API left
/// unbuilt for one that does not: the seams are the API's (its order spool is opened where the
/// config says, and the line the verb logs says what the API will do), and there is no watch.
///
/// Asserts that no wiring is made when `local` is absent, and that the spool is opened for it.
#[test]
fn without_copies_the_update_api_is_built_and_no_watch_is_wired() {
    let cfg = config("api");

    let Ok(assembled) = assemble(&cfg) else {
        panic!("the seams of the API");
    };

    assert!(assembled.watch.is_none());
    assert!(cfg.orders_dir.is_dir(), "the order spool was opened");
    let note = assembled.pending.note();
    assert!(note.starts_with("update API on (status "), "{note}");
}

/// Catches an error of the update API's seams lost on the way to the verb: a file where
/// `orders/` should be is a usage error with the API's words, and it is the error of a proxy
/// without copies only (the test above has the same file under a proxy with them).
///
/// Asserts that the error is a usage error, not a failure, which the unit would restart for
/// ever.
#[test]
fn a_spool_that_cannot_be_opened_is_a_usage_error_naming_it() {
    let cfg = config("spool");
    fs::write(&cfg.orders_dir, b"a file").expect("a file in the way");

    match assemble(&cfg).err() {
        Some(AssembleError::Usage(text)) => {
            assert!(
                text.starts_with("update API: the order spool at "),
                "{text}"
            );
            assert!(
                text.contains(&cfg.orders_dir.display().to_string()),
                "{text}"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// Catches an update route that is served when the proxy keeps its own copies: every call on it,
/// whatever the method, path and body, is answered 410 with the words of [`GONE`] and no header,
/// nothing is written (the state directory holds the copies' directory and nothing else), and
/// starting the route never stops the proxy.
///
/// Asserts that the API's route is not served (a `GET` is not a 200, a `POST` not a 503 or 202),
/// that the status is 410 with the body of [`GONE`], and that no call writes a request map.
#[tokio::test]
async fn with_copies_every_call_on_the_update_route_is_answered_410_and_nothing_is_written() {
    let cfg = config_local("gone");
    let Ok(assembled) = assemble(&cfg) else {
        panic!("the wiring");
    };
    let route = assembled
        .pending
        .start(Box::new(|| panic!("the route died")))
        .expect("the route");
    let post = format!(r#"{{"commit":"{A}","request_id":"r1","nodes":false}}"#);

    for (method, path, body) in [
        ("GET", "/v1/update/frontend", ""),
        ("POST", "/v1/update/frontend", post.as_str()),
        ("POST", "/v1/update/frontend/abort", ""),
        ("POST", "/v1/update/frontend/rollback", ""),
        ("PUT", "/v1/update/frontend", ""),
    ] {
        let reply = route
            .handle(UpdateRequest {
                method: method.to_owned(),
                path: path.to_owned(),
                body: body.as_bytes().to_vec(),
            })
            .await;
        assert_eq!(reply.status, 410, "{method} {path}");
        assert_eq!(reply.body, GONE.as_bytes(), "{method} {path}");
        assert!(reply.headers.is_empty(), "{method} {path}");
    }

    assert_eq!(names_in(&cfg.state_dir), [releases::RELEASES]);
    assert!(!cfg.orders_dir.exists(), "no order spool was made");
}

/// Catches a body that is not the JSON error the clients of the route read: it parses, and its
/// `error` says the API is retired.
#[test]
fn the_body_of_the_410_is_a_json_error_that_says_the_api_is_retired() {
    let body: serde_json::Value = serde_json::from_str(GONE).expect("JSON");
    let error = body["error"].as_str().expect("an error word");
    assert!(error.contains("retired"), "{error}");
}

/// Catches a line that does not say what the verb did: with copies it says the API is retired and
/// what is watched, and where the status is.
#[test]
fn with_copies_the_note_says_the_api_is_retired_and_what_is_watched() {
    let cfg = config_local("note");
    let Ok(assembled) = assemble(&cfg) else {
        panic!("the wiring");
    };

    assert_eq!(
        assembled.pending.note(),
        format!(
            "update API retired, /v1/update/* answers 410; the swap starts when the kbf at \
             /usr/bin/kbf is replaced (status {})",
            cfg.status_file.display()
        )
    );
}
