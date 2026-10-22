//! Where an update's release comes from (`source`): what the API takes in a request, refuses
//! before it saves or writes anything, keeps in the request map, puts in the order and hands
//! to the supervisor. The wire rule is held here too: a request that fetches is the bytes it
//! has always been, and the word `fetch` is read as the default.

use std::fs;

use hyper::StatusCode;
use serde_json::json;

use super::core::{ApiError, Core, Fault, Reply};
use super::fakes::{Rig, idle, req, staged};
use super::idempotency::Idempotency;
use super::model::{StatusBody, UpdateId, UpdateRequest};
use super::traits::{AvailableWire, InstallerState, InstallerStatus};
use crate::proxy::model::{Phase, Source, Verb};
use crate::testkit::{A, B, C, c};

fn accepted(r: Result<Reply, Fault>) -> (UpdateId, Phase) {
    match r {
        Ok(Reply::Update { update_id, phase }) => (update_id, phase),
        other => panic!("expected an update reply, got {other:?}"),
    }
}

fn refused(r: Result<Reply, Fault>) -> ApiError {
    match r {
        Err(Fault::Api(e)) => e,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The refusal has this status and this code.
fn is_refusal(e: &ApiError, status: StatusCode, code: &str) {
    assert_eq!(e.status, status, "{e:?}");
    assert_eq!(e.code, code, "{e:?}");
}

fn body(core: &mut Core) -> StatusBody {
    match core.status() {
        Reply::Status(b) => *b,
        other => panic!("expected a status, got {other:?}"),
    }
}

/// A request to swap the frontend only, taking the release from the host's own source.
fn fetch_req(commit: &str, request_id: &str) -> UpdateRequest {
    UpdateRequest {
        nodes: false,
        ..req(commit, request_id)
    }
}

/// A request to swap the frontend only, taking the release the host's package carries.
fn staged_req(commit: &str, request_id: &str) -> UpdateRequest {
    UpdateRequest {
        source: Source::Staged,
        ..fetch_req(commit, request_id)
    }
}

/// `status.json` of an idle host running `A`, whose package carries the seed `seed` (a commit
/// and its `seq`), and whose installer says it fetches from `release_source`.
fn host(seed: Option<&str>, release_source: Option<&str>) -> InstallerStatus {
    InstallerStatus {
        available: seed.map(|commit| AvailableWire {
            commit: c(commit),
            seq: 812,
        }),
        release_source: release_source.map(str::to_owned),
        ..idle()
    }
}

/// The text of the request map on disk.
fn map_text(rig: &Rig) -> String {
    fs::read_to_string(rig.dir.join("requests.json")).expect("the request map")
}

/// Nothing was saved and nothing was written for `request_id`.
fn left_no_trace(rig: &Rig, request_id: &str) {
    assert!(
        rig.orders.written.lock().unwrap().is_empty(),
        "an order was written"
    );
    assert!(
        Idempotency::open(&rig.dir).get(request_id).is_none(),
        "the request id was saved"
    );
    assert_eq!(rig.sup.starts(), 0);
}

/// Catches a host that fetches nothing being sent an order to fetch: the package is the only
/// source of a bundled host, so the request that does not name it is refused at the API, with
/// a code of its own, before the request id is saved or an order is written (an entry would
/// make the retry of a refused request an update that never began). The same request id then
/// asks for the package's release and is a fresh request.
///
/// Mutant planted: the check moved to after the request is saved (the entry is there).
#[test]
fn a_fetch_on_a_bundled_host_is_refused_before_anything_is_saved_or_written() {
    let rig = Rig::new("pr5a-bundled-fetch");
    rig.set_status(host(Some(B), Some("bundled")));
    let mut core = rig.core();
    let e = refused(core.post(fetch_req(B, "r1")));
    is_refusal(&e, StatusCode::CONFLICT, "source_refused");
    assert!(e.message.contains("bundled"), "{}", e.message);
    left_no_trace(&rig, "r1");
    let (_, phase) = accepted(core.post(staged_req(B, "r1")));
    assert_eq!(phase, Phase::Accepted);
}

/// Catches a refusal that is too wide: a host that fetches from a URL or from the frontend, or
/// whose installer says nothing of its source (it predates the report), is ordered as ever.
///
/// Mutant planted: any host that tells its source is refused.
#[test]
fn a_fetch_is_still_ordered_on_a_host_that_fetches_or_says_nothing() {
    for (i, source) in [None, Some("url"), Some("frontend")]
        .into_iter()
        .enumerate()
    {
        let rig = Rig::new(&format!("pr5a-fetching-{i}"));
        rig.set_status(host(None, source));
        let (_, phase) = accepted(rig.core().post(fetch_req(B, "r1")));
        assert_eq!(phase, Phase::Accepted, "{source:?}");
    }
}

/// Catches a staged update ordered for a release the package does not carry: no seed told,
/// the seed of another release, and the seed of the release that runs are each a 422 with a
/// code of its own, before anything is saved or written, and the text names what the host does
/// have.
///
/// Mutant planted: the commit compared with the installed release instead of the seed.
#[test]
fn a_staged_update_needs_the_package_to_carry_that_commit() {
    for (i, seed) in [None, Some(C), Some(A)].into_iter().enumerate() {
        let rig = Rig::new(&format!("pr5a-not-staged-{i}"));
        rig.set_status(host(seed, Some("bundled")));
        let e = refused(rig.core().post(staged_req(B, "r1")));
        is_refusal(&e, StatusCode::UNPROCESSABLE_ENTITY, "not_staged");
        match seed {
            Some(seed) => assert!(e.message.contains(&seed[..12]), "{}", e.message),
            None => assert!(e.message.contains("nothing"), "{}", e.message),
        }
        left_no_trace(&rig, "r1");
    }
}

/// Catches the order that does not say where its release comes from, and a request map, a
/// status and a supervisor that forget it: a staged update's order is a `frontend_update` for
/// the commit with source `staged`; the request map keeps the source and writes it as the one
/// key a build from before `source` cannot read; the status says it.
///
/// Mutants planted: the order written with source `fetch`; the entry saved with source
/// `fetch`; the status always `fetch`.
#[test]
fn a_staged_update_is_ordered_kept_and_reported_by_its_source() {
    let rig = Rig::new("pr5a-staged");
    rig.set_status(host(Some(B), Some("bundled")));
    let mut core = rig.core();
    let (id, phase) = accepted(core.post(staged_req(B, "r1")));
    assert_eq!(phase, Phase::Accepted);
    let order = rig.orders.written.lock().unwrap()[0].clone();
    assert_eq!(
        (order.verb, order.commit, order.source),
        (Verb::FrontendUpdate, c(B), Source::Staged)
    );
    let kept = Idempotency::open(&rig.dir);
    assert_eq!(kept.get("r1").map(|e| e.source), Some(Source::Staged));
    let file = map_text(&rig);
    assert!(file.contains(r#""source":"staged""#), "{file}");
    let s = body(&mut core);
    assert_eq!((s.update_id, s.source), (Some(id), Source::Staged));
}

/// Catches an update that fetches leaving a trace of `source` anywhere an older build would
/// choke on it: the request map's file, the order and the status carry no `source` for a fetch.
///
/// Mutant planted: `source` written whatever it is.
#[test]
fn an_update_that_fetches_writes_no_source_anywhere() {
    let rig = Rig::new("pr5a-fetching-bytes");
    let mut core = rig.core();
    accepted(core.post(fetch_req(B, "r1")));
    let file = map_text(&rig);
    assert!(!file.contains("source"), "{file}");
    let order = rig.orders.written.lock().unwrap()[0].clone();
    assert_eq!(order.source, Source::Fetch);
    let sent = serde_json::to_string(&order).unwrap();
    assert!(!sent.contains("source"), "{sent}");
    let status = serde_json::to_string(&body(&mut core)).unwrap();
    assert!(!status.contains("source"), "{status}");
}

/// Catches a source that stops at the API: once the installer has staged the release, the
/// supervisor is given the source, so that its record and the order for the nodes name it.
/// A process that is restarted while the update waits for the installer goes on with the
/// source of the entry it took the update from, and says it while it waits.
///
/// Mutants planted: the spec built with source `fetch`; a resumed update given `fetch`.
#[test]
fn the_supervisor_is_handed_the_source_also_by_a_process_that_took_the_update_up() {
    let rig = Rig::new("pr5a-handover");
    rig.set_status(host(Some(B), Some("bundled")));
    let mut core = rig.core();
    accepted(core.post(staged_req(B, "r1")));
    drop(core);
    let mut restarted = rig.core();
    assert_eq!(body(&mut restarted).source, Source::Staged);
    rig.set_status(staged(B));
    let s = body(&mut restarted);
    assert_eq!(s.phase, Some(Phase::Preflight));
    assert_eq!(s.source, Source::Staged);
    let starts = rig.sup.0.lock().unwrap().starts.clone();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].source, Source::Staged);
}

/// Catches a rollback ordered from the package: a rollback goes back to a release the host
/// fetched and the package carries only the newest, so `staged` with `rollback` is refused.
///
/// Mutant planted: the rollback flag not looked at when the source is `staged`.
#[test]
fn a_rollback_is_refused_for_a_staged_update() {
    let rig = Rig::new("pr5a-staged-rollback");
    rig.set_status(host(Some(B), None));
    let wanted = UpdateRequest {
        rollback: true,
        ..staged_req(B, "r1")
    };
    let e = refused(rig.core().post(wanted));
    is_refusal(&e, StatusCode::CONFLICT, "source_refused");
    assert!(e.message.contains("rollback"), "{}", e.message);
    left_no_trace(&rig, "r1");
}

/// Catches nodes being ordered from a release only this host has: the order for the nodes
/// does not name its source yet, so a staged update that asks for the nodes is refused, and
/// the same request without them is taken.
///
/// Mutant planted: the refusal dropped (a staged update with `nodes` is accepted).
#[test]
fn a_staged_update_takes_no_nodes_yet() {
    let rig = Rig::new("pr5a-staged-nodes");
    rig.set_status(host(Some(B), None));
    let with_nodes = UpdateRequest {
        nodes: true,
        ..staged_req(B, "r1")
    };
    let e = refused(rig.core().post(with_nodes));
    is_refusal(&e, StatusCode::CONFLICT, "source_refused");
    assert!(e.message.contains("nodes"), "{}", e.message);
    left_no_trace(&rig, "r1");
    accepted(rig.core().post(staged_req(B, "r1")));
}

/// Catches a request id reused for a request that asks for another source being taken for
/// the retry of the first: the source is part of what the id was used for, both ways round.
/// The retry with the same source is the same update and writes no second order.
///
/// Mutant planted: the source left out of the tuple a retry is compared by.
#[test]
fn a_request_id_reused_with_another_source_is_refused_and_with_the_same_one_is_a_retry() {
    let rig = Rig::new("pr5a-reuse-fetch");
    rig.set_status(host(Some(B), None));
    let mut core = rig.core();
    accepted(core.post(fetch_req(B, "r1")));
    let e = refused(core.post(staged_req(B, "r1")));
    is_refusal(&e, StatusCode::CONFLICT, "request_id_reused");

    let rig = Rig::new("pr5a-reuse-staged");
    rig.set_status(host(Some(B), None));
    let mut core = rig.core();
    let (first, _) = accepted(core.post(staged_req(B, "r1")));
    let e = refused(core.post(fetch_req(B, "r1")));
    is_refusal(&e, StatusCode::CONFLICT, "request_id_reused");
    let (again, _) = accepted(core.post(staged_req(B, "r1")));
    assert_eq!(first, again);
    assert_eq!(rig.orders.written.lock().unwrap().len(), 1);
}

/// Catches a retry judged by what it asks for before it is judged by its id: a request id used
/// for an update that fetches, asked again with source `staged` and the nodes, is a request id
/// reused (409 `request_id_reused`, exit 1 in the CLI), not a staged update that cannot take
/// nodes (409 `source_refused`, exit 3).
///
/// Mutant planted: the source and nodes check run before the request map is looked up.
#[test]
fn a_retry_with_another_source_and_nodes_is_a_reused_id_before_it_is_a_refused_source() {
    let rig = Rig::new("pr5a-reuse-nodes");
    rig.set_status(host(Some(B), None));
    let mut core = rig.core();
    accepted(core.post(fetch_req(B, "r1")));
    let again = UpdateRequest {
        nodes: true,
        ..staged_req(B, "r1")
    };
    let e = refused(core.post(again));
    is_refusal(&e, StatusCode::CONFLICT, "request_id_reused");
}

/// Catches the source lost when an update ends: the installer refuses the order of a staged
/// update, the update ends `failed`, and the status that is then read still says `staged`.
///
/// Mutant planted: the ended update built with source `fetch`.
#[test]
fn the_status_of_a_staged_update_that_ended_still_says_its_source() {
    let rig = Rig::new("pr5a-ended-source");
    rig.set_status(host(Some(B), Some("bundled")));
    let mut core = rig.core();
    accepted(core.post(staged_req(B, "r1")));
    rig.set_status(InstallerStatus {
        state: InstallerState::Refused,
        refusal: "hash: the digest differs".to_owned(),
        refused_order_id: Some(101),
        ..host(Some(B), Some("bundled"))
    });
    let s = body(&mut core);
    assert_eq!(s.phase, Some(Phase::Failed));
    assert_eq!(s.source, Source::Staged);
}

/// Catches a refusal of a fetch on a bundled host that leaves the operator at a dead end: the
/// text offers the staged source, and for a rollback (which a staged update cannot do) the
/// installer's own `rollback` on the host.
///
/// Mutant planted: the text without the way to roll back.
#[test]
fn the_refusal_of_a_fetch_on_a_bundled_host_says_what_can_be_asked() {
    let rig = Rig::new("pr5a-bundled-text");
    rig.set_status(host(Some(B), Some("bundled")));
    let e = refused(rig.core().post(fetch_req(B, "r1")));
    assert!(e.message.contains("source staged"), "{}", e.message);
    assert!(
        e.message.contains("kbf-installer rollback"),
        "{}",
        e.message
    );
}

/// Catches a request map written before `source` existed being refused or read as anything
/// but a fetch: its entries have no `source`, and a map kept by this build holds the entry of
/// a staged update beside them.
///
/// Mutant planted: `default` removed from the entry's `source`.
#[test]
fn a_request_map_from_before_source_reads_and_its_entries_fetch() {
    let rig = Rig::new("pr5a-old-map");
    let old = json!({
        "schema": 1,
        "next_seq": 3,
        "entries": [
            {"request_id": "old-1", "update_id": "upd-1", "commit": A, "nodes": true,
             "rollback": false, "started": true},
            {"request_id": "old-2", "update_id": "upd-2", "commit": B, "nodes": false,
             "rollback": false, "started": true, "waiting": null},
        ],
    });
    fs::write(rig.dir.join("requests.json"), old.to_string()).expect("seed the map");
    let map = Idempotency::open(&rig.dir);
    assert!(!map.lost());
    for id in ["old-1", "old-2"] {
        let entry = map.get(id).expect("the entry");
        assert_eq!(entry.source, Source::Fetch, "{id}");
    }
    let mut core = rig.core();
    rig.set_status(host(Some(B), None));
    accepted(core.post(staged_req(B, "new-1")));
    let again = Idempotency::open(&rig.dir);
    assert_eq!(again.get("old-1").map(|e| e.source), Some(Source::Fetch));
    assert_eq!(again.get("new-1").map(|e| e.source), Some(Source::Staged));
}

/// The keys after the commit of a request to swap the frontend only, as the API has always
/// had them.
const REST: &str = r#""request_id":"r1","nodes":false,"rollback":false"#;

fn parse(text: &str) -> Result<UpdateRequest, serde_json::Error> {
    serde_json::from_str(text)
}

/// A request body for `who` whose `source` is `word`, a JSON value as text.
fn named(who: &str, word: &str) -> String {
    format!(r#"{{"commit":"{who}","request_id":"r1","source":{word}}}"#)
}

/// Catches the word `fetch` refused, a request that names no source read as anything but a
/// fetch, and a word that is not a source being read as one: the parse takes exactly
/// `fetch` and `staged`.
///
/// Mutants planted: `fetch` removed from the words read; an unknown word read as `fetch`.
#[test]
fn a_request_reads_fetch_staged_or_nothing_and_no_other_source() {
    let fetch = parse(&named(B, r#""fetch""#)).unwrap();
    assert_eq!(fetch.source, Source::Fetch);
    let package = parse(&named(B, r#""staged""#)).unwrap();
    assert_eq!(package.source, Source::Staged);
    let none = format!(r#"{{"commit":"{B}","request_id":"r1"}}"#);
    assert_eq!(parse(&none).unwrap().source, Source::Fetch);
    for bad in [r#""bogus""#, r#""Staged""#, r#""""#, "2", "null"] {
        assert!(parse(&named(B, bad)).is_err(), "{bad}");
    }
}

/// Catches a request that fetches being written differently from today: a client that sends
/// `source` always breaks against a proxy that has not restarted since its package was
/// upgraded (the request type refuses a key it does not know). A fetch is the four keys it
/// has always been; a staged request names its source after them.
///
/// Mutant planted: `skip_serializing_if` removed from `source`.
#[test]
fn a_request_that_fetches_is_todays_bytes_and_a_staged_one_names_its_source() {
    let fetch = serde_json::to_string(&fetch_req(B, "r1")).unwrap();
    let today = format!(r#"{{"commit":"{B}",{REST}}}"#);
    assert_eq!(fetch, today);
    let sent = serde_json::to_string(&staged_req(B, "r1")).unwrap();
    let want = format!(r#"{},"source":"staged"}}"#, fetch.trim_end_matches('}'));
    assert_eq!(sent, want);
}
