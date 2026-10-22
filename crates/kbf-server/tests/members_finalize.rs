//! The farm's finalize target and the report of where each member stands against it
//! (`Members::raise_finalize`, `Members::finalize_target`, `Members::finalize_report`), and
//! the `finalize_to` that the frontend's registration and heartbeat answers carry
//! (`kbf_server::members_rpc`). `GET /v1/finalize` and `POST /v1/finalize` over a real
//! listener are in `api_finalize.rs`; the node's side is in `finalize_node.rs`.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_server::members::{
    Fill, FinalizeState, Members, MembershipConfig, MembershipError, NodeFacts, PROTOCOL, Progress,
    Registrant, Registration,
};
use kbf_server::members_rpc::MembershipService;
use kbf_server::storage_health::Reported;
use kbf_state::{Cond, Store, Txn};
use kbf_storage::service::Admission;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Server};

const TTL: Duration = Duration::from_secs(30);

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: TTL,
        dwell: Duration::from_secs(120),
        replication: 2,
    }
}

fn build(store: &Arc<Hooked>) -> Members {
    Members::new(Arc::clone(store) as Arc<dyn Store>, config())
}

/// Asserts that the farm's target is a key that only rises: absent before anyone asks, written
/// by the first ask, left alone by the same or a lower target (which say they wrote nothing),
/// raised by a higher one, kept as four big-endian bytes, and the same for another frontend
/// over the same store.
#[test]
fn the_target_only_rises() {
    let s = Hooked::new();
    let members = build(&s);
    assert_eq!(members.finalize_target().expect("read"), None);
    assert!(s.fake.get("meta/finalize").expect("read").is_none());

    assert!(members.raise_finalize(4).expect("the first ask"));
    assert_eq!(members.finalize_target().expect("read"), Some(4));
    for target in [4, 3, 1] {
        let wrote = members.raise_finalize(target).expect("no-op");
        assert!(!wrote, "{target} wrote under 4");
    }
    assert_eq!(members.finalize_target().expect("read"), Some(4));

    assert!(members.raise_finalize(5).expect("a higher ask"));
    let stored = s.fake.get("meta/finalize").expect("read").expect("a key");
    assert_eq!(stored.value, [0, 0, 0, 5]);
    assert_eq!(build(&s).finalize_target().expect("read"), Some(5));
}

/// Asserts that a writer whose read is stale when it commits (another frontend raised the
/// target between its read and its write) reads again and does not lower the target: it
/// finds 9 held against its 7, writes nothing and says so.
#[test]
fn a_writer_that_loses_a_race_reads_again_and_keeps_the_higher_target() {
    let s = Hooked::new();
    let members = build(&s);
    assert!(members.raise_finalize(2).expect("the first ask"));
    let other = Arc::new(build(&s));
    s.after_get("meta/finalize", move || {
        assert!(other.raise_finalize(9).expect("the other frontend"));
    });
    let wrote = members.raise_finalize(7).expect("loses the race");
    assert!(!wrote, "7 was written over 9");
    assert_eq!(members.finalize_target().expect("read"), Some(9));
}

/// Asserts that a writer that loses every compare-and-set gives up with `Contended` and
/// writes nothing, and that the same ask succeeds once the conflicts stop.
#[test]
fn a_writer_that_keeps_losing_gives_up_without_writing() {
    let s = Hooked::new();
    let members = build(&s);
    s.conflict_on(Some("meta/finalize"));
    let err = members.raise_finalize(3).expect_err("conflicts");
    assert!(matches!(err, MembershipError::Contended), "{err}");
    s.conflict_on(None);
    assert_eq!(members.finalize_target().expect("read"), None);
    assert!(members.raise_finalize(3).expect("no conflict now"));
}

/// Asserts that a stored target that is not four bytes is reported as corrupt, by a read and by
/// a write, and not read as a number or written over.
#[test]
fn a_stored_target_that_is_not_four_bytes_is_corrupt() {
    let s = Hooked::new();
    let members = build(&s);
    let put = Txn::new().put("meta/finalize", vec![1, 2, 3], Cond::Absent);
    s.fake.txn(put).expect("store").expect("no conflict");
    let read = members.finalize_target().expect_err("three bytes");
    assert!(matches!(read, MembershipError::Corrupt(_)), "{read}");
    let write = members.raise_finalize(5).expect_err("three bytes");
    assert!(matches!(write, MembershipError::Corrupt(_)), "{write}");
}

/// Storage node `i`, whose index is whole.
fn node(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// What a node that runs log version `runs_log`, has its log at `log_version` and was built
/// from `commit` says.
fn said(runs_log: u32, log_version: u32, commit: &str) -> NodeFacts {
    NodeFacts {
        runs_log: Some(runs_log),
        log_version: Some(log_version),
        build_commit: Some(commit.to_owned()),
    }
}

/// Storage node `i` registers saying `facts`.
fn join(members: &Members, i: u32, facts: &NodeFacts) -> Registration {
    members
        .register_reporting(&node(i), None, facts)
        .expect("registers")
}

/// The member `joined` beats saying `facts`.
fn beat(members: &Members, joined: &Registration, facts: &NodeFacts) {
    members
        .heartbeat_reporting(
            joined.id,
            joined.boot,
            Progress::CaughtUp,
            Reported::default(),
            (Fill::default(), facts),
            None,
        )
        .expect("beats");
}

/// Five members against a finalize to 5: 1 is at 5, 2 runs 5 and is at 4, 3 runs only 4, 4 says
/// nothing of its build or log, and 5 registered and then fell silent, so its row lapsed and
/// the map holds it `Down`. The stores' clock is at 40 s.
fn five(s: &Arc<Hooked>) -> Members {
    let members = build(s);
    let facts = [
        said(5, 5, "aaa"),
        said(5, 4, "bbb"),
        said(4, 4, "ccc"),
        NodeFacts::default(),
        said(5, 5, "ddd"),
    ];
    let joined: Vec<Registration> = (1..=5_u32)
        .zip(&facts)
        .map(|(i, facts)| join(&members, i, facts))
        .collect();
    s.clock.advance(Duration::from_secs(20));
    for (joined, facts) in joined.iter().zip(&facts).take(4) {
        beat(&members, joined, facts);
    }
    s.clock.advance(Duration::from_secs(20));
    members.refresh().expect("the lapsed member goes down");
    members
}

/// Asserts that the report judges every member of the map against the target, in member
/// order: `done` when the log is at the target, `pending` when the build runs it and the log
/// is below, `blocked` when the build runs less (saying how much) or the member has no live
/// row (it is `Down` in the map), and `unknown` when the row says nothing of the build or the
/// log; that what the row says is shown with the verdict (nothing for the member without a
/// row); and that only `blocked` and `unknown` block.
#[test]
fn the_report_judges_every_member_against_the_target() {
    let s = Hooked::new();
    let members = five(&s);
    let report = members.finalize_report(5).expect("the report");
    let ids: Vec<u64> = report.iter().map(|n| n.member).collect();
    assert_eq!(ids, [1, 2, 3, 4, 5]);
    let states: Vec<&str> = report.iter().map(|n| n.state.name()).collect();
    let want = ["done", "pending", "blocked", "unknown", "blocked"];
    assert_eq!(states, want);
    let blocks: Vec<bool> = report.iter().map(|n| n.state.blocks()).collect();
    assert_eq!(blocks, [false, false, true, true, true]);

    let seen = |i: usize| {
        let node = &report[i];
        (node.build_commit.clone(), node.runs_log, node.log_version)
    };
    assert_eq!(seen(0), (Some("aaa".to_owned()), Some(5), Some(5)));
    assert_eq!(seen(1), (Some("bbb".to_owned()), Some(5), Some(4)));
    assert_eq!(seen(2), (Some("ccc".to_owned()), Some(4), Some(4)));
    assert_eq!(seen(3), (None, None, None));
    assert_eq!(seen(4), (None, None, None), "no row");

    let reasons: Vec<&str> = report.iter().map(|n| n.reason.as_str()).collect();
    assert_eq!(reasons[..2], ["", ""]);
    assert!(reasons[2].contains("up to 4, below 5"), "{reasons:?}");
    assert!(reasons[3].contains("no runs_log"), "{reasons:?}");
    assert!(reasons[4].contains("no live row"), "{reasons:?}");
}

/// Asserts that the verdicts follow the target asked about: against 4 the member that runs 4
/// and the ones whose logs are at 4 or past it are done, and the two that say nothing still
/// block; against 6, which no member's build runs, every member that reports its facts is
/// blocked.
#[test]
fn the_verdicts_follow_the_target_asked_about() {
    let s = Hooked::new();
    let members = five(&s);
    let at_four = members.finalize_report(4).expect("the report");
    let states: Vec<&str> = at_four.iter().map(|n| n.state.name()).collect();
    let want = ["done", "done", "done", "unknown", "blocked"];
    assert_eq!(states, want);

    let at_six = members.finalize_report(6).expect("the report");
    let states: Vec<&str> = at_six.iter().map(|n| n.state.name()).collect();
    let want = ["blocked", "blocked", "blocked", "unknown", "blocked"];
    assert_eq!(states, want);
}

/// Asserts that the four states are spelled as the API spells them.
#[test]
fn the_states_are_spelled_as_the_api_spells_them() {
    let names = [
        (FinalizeState::Done, "done"),
        (FinalizeState::Pending, "pending"),
        (FinalizeState::Blocked, "blocked"),
        (FinalizeState::Unknown, "unknown"),
    ];
    for (state, name) in names {
        assert_eq!(state.name(), name);
        let json = serde_json::to_string(&state).expect("JSON");
        assert_eq!(json, format!("\"{name}\""));
    }
}

/// A frontend's membership service on a loopback port, over `store`.
async fn serve(store: &Arc<Hooked>) -> (Arc<Members>, MembershipClient<Channel>) {
    let members = Arc::new(build(store));
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr: SocketAddr = incoming.local_addr().expect("addr");
    let service = MembershipService::new(Arc::clone(&members), Admission::Open).server();
    tokio::spawn(
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming),
    );
    let channel = Channel::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    (members, MembershipClient::new(channel))
}

fn request(i: u32) -> pb::RegisterRequest {
    pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: format!("node-{i}"),
        addr: format!("10.0.0.{i}:8982"),
        weight: 1,
        zone: "a".to_owned(),
        protocol: PROTOCOL,
        catching_up: false,
        ..Default::default()
    }
}

/// What the frontend answers the registration of node `i`.
async fn registers(client: &mut MembershipClient<Channel>, i: u32) -> pb::RegisterResponse {
    let answer = client.register(request(i)).await;
    answer.expect("registers").into_inner()
}

/// What the frontend answers a heartbeat of member 1 at `boot`.
async fn beats(client: &mut MembershipClient<Channel>, boot: u64) -> pb::HeartbeatResponse {
    let sent = pb::HeartbeatRequest {
        member_id: 1,
        boot,
        ..Default::default()
    };
    let answer = client.heartbeat(sent).await;
    answer.expect("beats").into_inner()
}

/// Asserts that a registration answer and a heartbeat answer carry `finalize_to` exactly when
/// the farm's target is set, and the target as it stands: unset before anyone asked (a node
/// reads "nothing asked", not 0), the target after, a registration of a restarted node
/// included, a higher target when it rises, and not a lower one a stale writer asked for.
#[tokio::test]
async fn the_answers_carry_the_target_exactly_when_it_is_set() {
    let store = Hooked::new();
    let (members, mut client) = serve(&store).await;
    assert_eq!(registers(&mut client, 1).await.finalize_to, None);
    assert_eq!(beats(&mut client, 1).await.finalize_to, None);

    assert!(members.raise_finalize(4).expect("raise the target"));
    assert_eq!(beats(&mut client, 1).await.finalize_to, Some(4));
    let again = registers(&mut client, 1).await;
    assert_eq!((again.member_id, again.boot), (1, 2));
    assert_eq!(again.finalize_to, Some(4));

    assert!(members.raise_finalize(5).expect("raise the target"));
    assert!(!members.raise_finalize(2).expect("a lower ask"));
    assert_eq!(beats(&mut client, 2).await.finalize_to, Some(5));
}
