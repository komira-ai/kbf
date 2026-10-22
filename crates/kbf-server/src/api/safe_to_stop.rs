//! `GET /v1/members/{id}/safe-to-stop`: may the host of this storage member be stopped now,
//! without leaving a blob with one copy? The updater of a host asks it before it stops the
//! host's process (`docs/api.md`).
//!
//! It is the storage gate of the update controller ([`storage_gate`], mode
//! [`GateMode::Out`]) asked about one member, over a look at the members as the controller
//! reads them: with this member out, every other member must be `Active`, heard within the
//! time to live of the membership, not planned down, and report a repair debt of zero; and
//! at least one other member must exist. The member's own state is not asked (a stop does not
//! need it healthy), only its own repair debt. A read: it changes no member, no row and no
//! placement, and it passes the same gates as the member write, because it tells an operator
//! whether to stop a host.
//!
//! `{id}` is a storage member's number (`2` for member `m2`) or a node's id. A node is judged
//! as the member that is its storage half, joined by the address it enrolled with as the
//! controller joins a host to its member ([`storage_half`]), so asking by either name gives
//! the same answer. A node that has no storage half (a Mac, a daemon-only host, any host of a
//! server without a state store) is a daemon, which drains in its own stop and needs no
//! permit: it answers safe, with a line that says what was judged. A node that cannot be told
//! from one that has a member (the enrollment registry does not know it or cannot be read, or
//! its address is no member's or two members') is `409`, never safe. An id that is neither a
//! member nor a node is `404`.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use kbf_map::MemberId;
use kbf_types::{FarmTime, WorkerId};
use serde::{Deserialize, Serialize};
use tokio::task::JoinError;

use super::{ApiState, encode_segment, error, gate, json, member_failure};
use crate::farm::Farm;
use crate::members::{MemberLook, Members, MembersLook, MembershipError};
use crate::proxy_auth::Caller;
use crate::update::{Facts, GateMode, Policy, Rows, member_facts, storage_gate, storage_half};

/// The instant a look is judged at. The gate reads only how long ago each member was heard,
/// and a look carries those ages, so any instant later than the oldest age serves; this one
/// is about thirty-five years after the epoch.
const LOOK_AT: FarmTime = FarmTime::from_millis(1 << 40);

/// What a daemon-only answer says: only the daemon is judged.
const DAEMON: &str = "judged as a daemon only: the host runs no storage member, and a \
                      daemon drains in its own stop";

/// The answer of the query.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// Whether the host may be stopped now.
    pub safe: bool,
    /// Why not: what keeps the storage gate shut (the worst thing found, one line), or, for
    /// a daemon, what was judged. Empty for a storage member that is safe to stop.
    pub reasons: Vec<String>,
    /// Whether the member is already planned down. It does not make the stop unsafe: the
    /// member is already out of the write path.
    pub planned_down: bool,
}

impl Verdict {
    /// The answer for a host that has no storage member.
    fn daemon() -> Self {
        Self {
            safe: true,
            reasons: vec![DAEMON.to_owned()],
            planned_down: false,
        }
    }
}

/// The path of `GET /v1/members/{id}/safe-to-stop`, with `id` percent-encoded as one path
/// segment (a node id may hold a `/`, a `:` or a space).
#[must_use]
pub fn safe_to_stop_path(id: &str) -> String {
    format!("/v1/members/{}/safe-to-stop", encode_segment(id))
}

/// The verdict for the storage member `own` of `look`, with `ttl` the time to live of the
/// membership.
fn verdict(look: &MembersLook, own: &MemberLook, ttl: Duration) -> Verdict {
    let facts = Facts::of_members(member_facts(look, LOOK_AT));
    let policy = Policy {
        ttl,
        ..Policy::default()
    };
    let mode = GateMode::Out { fresh_since: None };
    let shut = storage_gate(&facts, own.id, mode, &policy, LOOK_AT).err();
    Verdict {
        safe: shut.is_none(),
        reasons: shut.into_iter().map(|fail| fail.to_string()).collect(),
        planned_down: own.planned_down,
    }
}

/// The verdict for the storage member that `id` names in `look` (a member's number); `None`
/// if `id` is not a number or no member has it.
fn member_verdict(id: &str, look: &MembersLook, ttl: Duration) -> Option<Verdict> {
    let want = MemberId(id.parse().ok()?);
    let own = look.members.iter().find(|m| m.id == want)?;
    Some(verdict(look, own, ttl))
}

/// What the farm and the enrollment registry say of an id that names no member.
#[derive(Debug)]
enum Node {
    /// No node of that name has registered (or the id named a member, and none was asked).
    Unknown,
    /// A node with no storage half: a daemon.
    Daemon,
    /// A node whose storage half is this member.
    Member(MemberLook),
    /// A node that may have a storage half and cannot be told from one; the text says why.
    Unsure(String),
}

/// What the blocking read found: the members, the time to live of the membership and, for an
/// id that is no member's, the node of that name.
#[derive(Debug)]
struct Seen {
    look: MembersLook,
    node: Node,
    ttl: Duration,
}

/// What the blocking read returned.
type Looked = Result<Result<Seen, MembershipError>, JoinError>;

/// The node `id` as the farm and the enrollment registry (`rows`, the state store's rows;
/// `None` on a server without one, which keeps no members) have it, given the members `look`.
fn node_of(farm: &Farm, rows: Option<&Rows>, id: &str, look: &MembersLook) -> Node {
    let worker = WorkerId::new(id);
    let Some(seen) = farm.update_look().into_iter().find(|n| n.id == worker) else {
        return Node::Unknown;
    };
    // Only a Linux host has a storage half, as the update controller reads the farm.
    let Some(rows) = rows.filter(|_| seen.os == "linux") else {
        return Node::Daemon;
    };
    match storage_half(rows, &worker, look) {
        Ok(None) => Node::Daemon,
        Ok(Some(member)) => Node::Member(member),
        Err(why) => Node::Unsure(why),
    }
}

/// The blocking half of the query: the members and, if `id` names none of them, the node of
/// that name. The store's calls block (its trait says so), and so does the farm's lock.
fn read(
    id: &str,
    farm: &Farm,
    members: Option<&Members>,
    rows: Option<&Rows>,
) -> Result<Seen, MembershipError> {
    let ttl = members.map_or(Duration::ZERO, |m| m.config().ttl);
    let look = members.map_or_else(|| Ok(MembersLook::default()), Members::look)?;
    let node = if member_verdict(id, &look, ttl).is_none() {
        node_of(farm, rows, id, &look)
    } else {
        Node::Unknown
    };
    Ok(Seen { look, node, ttl })
}

/// The answer to a query for `id`, from what the blocking read returned. An id that is a
/// number and a member's id is that member; a number that is no member's falls through to the
/// node of that name.
fn respond(id: &str, looked: Looked) -> Response {
    let seen = match looked {
        Ok(Ok(seen)) => seen,
        Ok(Err(e)) => return error(member_failure(&e), &e.to_string()),
        Err(e) => {
            let why = format!("membership task: {e}");
            return error(StatusCode::INTERNAL_SERVER_ERROR, &why);
        }
    };
    if let Some(said) = member_verdict(id, &seen.look, seen.ttl) {
        return json(StatusCode::OK, &said);
    }
    match seen.node {
        Node::Daemon => json(StatusCode::OK, &Verdict::daemon()),
        Node::Member(own) => json(StatusCode::OK, &verdict(&seen.look, &own, seen.ttl)),
        Node::Unsure(why) => {
            let why = format!("node {id:?} may run a storage member: {why}");
            error(StatusCode::CONFLICT, &why)
        }
        Node::Unknown => {
            let why = format!("no member or node {id:?} (a member is its number: 2 for m2)");
            error(StatusCode::NOT_FOUND, &why)
        }
    }
}

/// The handler: the write gates, then the verdict.
pub(super) async fn check(
    State(api): State<Arc<ApiState>>,
    peer: Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err((code, why)) = gate(api.token.as_ref(), &peer, &headers) {
        return error(code, &why);
    }
    // A server without a state store keeps no members: every host it knows is a daemon.
    let members = api.members.clone();
    let (farm, rows, name) = (Arc::clone(&api.farm), api.update.clone(), id.clone());
    let job = move || read(&name, &farm, members.as_deref(), rows.as_ref());
    let looked = tokio::task::spawn_blocking(job).await;
    respond(&id, looked)
}

#[cfg(test)]
mod tests {
    use kbf_map::MemberState;
    use kbf_state::StoreError;

    use super::*;

    const TTL: Duration = Duration::from_secs(15);
    const MINUTE: Duration = Duration::from_secs(60);

    /// An `Active` member heard a second ago that owes nothing.
    fn member(id: u64) -> MemberLook {
        MemberLook {
            id: MemberId(id),
            addr: format!("192.0.2.{id}:8982"),
            state: MemberState::Active,
            boot: 1,
            planned_down: false,
            heard_ago: Some(Duration::from_secs(1)),
            repair_debt: Some(0),
            open_writes: Some(0),
            reported_generation: Some(1),
            marked_ago: None,
        }
    }

    fn look(members: Vec<MemberLook>) -> MembersLook {
        MembersLook {
            generation: 1,
            members,
        }
    }

    /// What the blocking read found: `members` and, for an id that is no member's, `node`.
    fn seen(members: Vec<MemberLook>, node: Node) -> Looked {
        Ok(Ok(Seen {
            look: look(members),
            node,
            ttl: TTL,
        }))
    }

    fn verdict_of(look: &MembersLook, id: u64, ttl: Duration) -> Verdict {
        member_verdict(&id.to_string(), look, ttl).expect("a member")
    }

    fn reasons(look: &MembersLook, id: u64) -> Vec<String> {
        verdict_of(look, id, TTL).reasons
    }

    fn panics() -> Result<Seen, MembershipError> {
        panic!("the membership call panicked")
    }

    async fn body(answer: Response) -> (StatusCode, serde_json::Value) {
        let code = answer.status();
        let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("a body");
        (code, serde_json::from_slice(&bytes).expect("JSON"))
    }

    /// Catches: a member whose peers are all fine answered unsafe, and a safe answer that
    /// still carries a reason or says the member is planned down, or that does not survive
    /// the JSON the CLI reads it back from. Mutants it catches: the verdict's `safe` taken
    /// from `shut.is_some()` (the gate inverted), which turns this and every test below that
    /// asks for a verdict red.
    #[test]
    fn a_member_with_healthy_peers_is_safe_to_stop() {
        let healthy = look(vec![member(1), member(2), member(3)]);
        for id in [1, 2, 3] {
            let said = verdict_of(&healthy, id, TTL);
            assert!(said.safe, "member {id}");
            assert!(said.reasons.is_empty(), "member {id}");
            assert!(!said.planned_down, "member {id}");
            let wire = serde_json::to_value(&said).expect("JSON");
            let back = serde_json::from_value::<Verdict>(wire).expect("a verdict");
            assert_eq!(back, said, "member {id}");
        }
    }

    /// Catches: a gate reason dropped from the answer, or one that leaves the answer safe:
    /// each thing the storage gate shuts on in another member (not `Active`, catching up,
    /// planned down, silent, an unreported or a non-zero repair debt) makes `safe` false
    /// with the gate's own line, the member's own debt does too, and so does a lone member
    /// (no second copy). Mutants it catches: `reasons` left empty; a reason text changed;
    /// the peers' debt not read (the gate asked about the member alone).
    #[test]
    fn each_thing_the_gate_shuts_on_makes_the_stop_unsafe_with_its_reason() {
        let cases = [
            (
                MemberLook {
                    state: MemberState::Down,
                    ..member(2)
                },
                "member m2 is Down",
            ),
            (
                MemberLook {
                    state: MemberState::Draining,
                    ..member(2)
                },
                "member m2 is Draining",
            ),
            (
                MemberLook {
                    state: MemberState::CatchingUp,
                    ..member(2)
                },
                "member m2 is catching up",
            ),
            (
                MemberLook {
                    planned_down: true,
                    ..member(2)
                },
                "member m2 is planned down",
            ),
            (
                MemberLook {
                    heard_ago: None,
                    ..member(2)
                },
                "member m2 was not heard within the time to live",
            ),
            (
                MemberLook {
                    heard_ago: Some(Duration::from_secs(20)),
                    ..member(2)
                },
                "member m2 was not heard within the time to live",
            ),
            (
                MemberLook {
                    repair_debt: None,
                    ..member(2)
                },
                "member m2 does not report its repair debt",
            ),
            (
                MemberLook {
                    repair_debt: Some(4),
                    ..member(2)
                },
                "member m2 owes 4 blobs",
            ),
        ];
        for (peer, want) in cases {
            let spoiled = look(vec![member(1), peer]);
            let said = verdict_of(&spoiled, 1, TTL);
            assert!(!said.safe, "{want}");
            assert_eq!(said.reasons, [want]);
        }
        let owing = MemberLook {
            repair_debt: Some(3),
            ..member(1)
        };
        let own = look(vec![owing, member(2)]);
        assert_eq!(reasons(&own, 1), ["member m1 owes 3 blobs"]);
        assert_eq!(reasons(&own, 2), ["member m1 owes 3 blobs"]);
        let lone = "no other member holds the second copy";
        let alone = look(vec![member(1)]);
        assert_eq!(reasons(&alone, 1), [lone]);
    }

    /// Catches: a worse reason hidden behind a lesser one: a second member down is named
    /// before another member's debt, as the controller's gate ranks them. Mutants it
    /// catches: the first reason found given instead of the worst.
    #[test]
    fn the_worst_reason_is_the_one_given() {
        let owing = MemberLook {
            repair_debt: Some(9),
            ..member(2)
        };
        let gone = MemberLook {
            state: MemberState::Down,
            ..member(3)
        };
        let both = look(vec![member(1), owing, gone]);
        assert_eq!(reasons(&both, 1), ["member m3 is Down"]);
    }

    /// Catches: the time to live taken from a constant instead of the membership's own: a
    /// member heard 30 s ago is silent for a membership whose ttl is 15 s and fresh for one
    /// whose ttl is a minute.
    #[test]
    fn the_time_to_live_is_the_memberships() {
        let slow = MemberLook {
            heard_ago: Some(Duration::from_secs(30)),
            ..member(2)
        };
        let slow = look(vec![member(1), slow]);
        assert!(!verdict_of(&slow, 1, TTL).safe);
        assert!(verdict_of(&slow, 1, MINUTE).safe);
    }

    /// Catches: a member already planned down answered unsafe, or its mark lost from the
    /// answer: its own state is not asked, a stop of a member already out of the write path
    /// is safe, and the answer says it is planned down. Its peer, whose other member is
    /// planned down, is not safe.
    #[test]
    fn a_member_already_planned_down_is_safe_and_says_so() {
        let out = MemberLook {
            state: MemberState::Down,
            planned_down: true,
            ..member(1)
        };
        let planned = look(vec![out, member(2)]);
        let said = verdict_of(&planned, 1, TTL);
        assert!(said.safe);
        assert!(said.planned_down);
        let other = verdict_of(&planned, 2, TTL);
        assert!(!other.safe);
        assert!(!other.planned_down);
    }

    /// Catches: an id that is no member answered as one: the look has no member 9, a name is
    /// no number, and `m2` (how the reasons spell member 2) is not member 2's id, since a
    /// node may be named that.
    #[test]
    fn an_unknown_member_has_no_verdict() {
        let pair = look(vec![member(1), member(2)]);
        for id in ["9", "m2", "linux-1", ""] {
            assert!(member_verdict(id, &pair, TTL).is_none(), "{id:?}");
        }
    }

    /// Catches: each outcome answered with the wrong status or body: a member is `200` with
    /// its verdict, and wins over a node of the same name; an id that is no member and no
    /// node is `404` naming it; a node with no storage half, and a number that is no
    /// member's but a node's, are `200` and safe with the line that says only the daemon was
    /// judged; a node whose storage half is a member is `200` with that member's verdict, the
    /// gate run; a node that cannot be told from one with a member is `409` with the reason;
    /// and the membership failing is `503`, a row that does not decode and a panicked task
    /// `500`. Mutants it catches: a node with a member answered as a daemon; an unsure node
    /// answered safe.
    #[tokio::test]
    async fn each_outcome_is_answered_with_its_status() {
        let two = || vec![member(1), member(2)];
        let (code, said) = body(respond("1", seen(two(), Node::Unknown))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            said,
            serde_json::json!({"safe": true, "reasons": [], "planned_down": false})
        );
        let (code, said) = body(respond("1", seen(two(), Node::Daemon))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(said["reasons"], serde_json::json!([]));

        let (code, said) = body(respond("9", seen(two(), Node::Unknown))).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let text = said["error"].as_str().unwrap_or_default();
        assert!(text.starts_with("no member or node \"9\""), "{text}");
        let (code, _) = body(respond("linux-1", seen(two(), Node::Unknown))).await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        for id in ["linux-1", "9"] {
            let (code, said) = body(respond(id, seen(two(), Node::Daemon))).await;
            assert_eq!(code, StatusCode::OK, "{id}");
            assert_eq!(said["safe"], true, "{id}");
            assert_eq!(said["planned_down"], false, "{id}");
            assert_eq!(said["reasons"], serde_json::json!([DAEMON]), "{id}");
        }

        let owing = MemberLook {
            repair_debt: Some(3),
            ..member(1)
        };
        let host = seen(vec![owing, member(2)], Node::Member(member(2)));
        let (code, said) = body(respond("linux-3", host)).await;
        let line = "member m1 owes 3 blobs";
        assert_eq!(code, StatusCode::OK);
        assert_eq!(said["safe"], false);
        assert_eq!(said["reasons"], serde_json::json!([line]));

        let unsure = Node::Unsure("linux-5 is not enrolled".to_owned());
        let (code, said) = body(respond("linux-5", seen(two(), unsure))).await;
        assert_eq!(code, StatusCode::CONFLICT);
        let text = said["error"].as_str().unwrap_or_default();
        assert!(text.contains("linux-5 is not enrolled"), "{text}");

        let down = MembershipError::Store(StoreError::Failed("down".to_owned()));
        let text = down.to_string();
        let (code, said) = body(respond("1", Ok(Err(down)))).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(said["error"], text);
        let corrupt = MembershipError::Corrupt("row".to_owned());
        let (code, _) = body(respond("1", Ok(Err(corrupt)))).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        let panicked = tokio::task::spawn_blocking(panics).await;
        let failed = Err(panicked.expect_err("a panic"));
        let (code, said) = body(respond("1", failed)).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        let message = said["error"].as_str().unwrap_or_default();
        assert!(message.contains("membership task"));
    }

    /// Catches: an id sent unencoded, which a `/` or a space in a node id would split into
    /// more path segments than the route has.
    #[test]
    fn the_path_holds_the_id_as_one_segment() {
        assert_eq!(safe_to_stop_path("7"), "/v1/members/7/safe-to-stop");
        assert_eq!(
            safe_to_stop_path("rack 1/a:b"),
            "/v1/members/rack%201%2Fa%3Ab/safe-to-stop"
        );
    }
}
