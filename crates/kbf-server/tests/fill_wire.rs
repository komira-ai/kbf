//! A storage node's fill report over the wire (`kbf_server::join::Joiner`,
//! `kbf_server::fill::FillState`, `kbf.map.v1.Membership`): the heartbeat carries the flush
//! generation, the tail acknowledgement and the fingerprint into the frontend's rows, the
//! answer carries the tail epoch back, and a real node-side [`Joiner`] against a real
//! membership service is promoted by its own beat, as in `members_auto_promote.rs`, which
//! has the rules.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::FlushReport;
use kbf_map::{MemberId, MemberState};
use kbf_server::join::{Identity, Joiner};
use kbf_server::members::{
    Block, Fill, Handoff, Members, MembershipConfig, PROTOCOL, Progress, Registrant, fill_fp,
};
use kbf_server::members_rpc::MembershipService;
use kbf_server::storage_health::Reported;
use kbf_state::Store;
use kbf_storage::service::Admission;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};

const MS: Duration = Duration::from_millis(1);
const SETTLE: Duration = Duration::from_secs(30);

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(3600),
        dwell: Duration::from_secs(7200),
        replication: 2,
    }
}

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

/// A frontend that promotes joiners, serving membership on a loopback port.
async fn serve(store: &Arc<Hooked>) -> (Arc<Members>, Channel) {
    let members = Arc::new(
        Members::new(Arc::clone(store) as Arc<dyn Store>, config()).with_handoff(Handoff {
            settle: SETTLE,
            allow_degraded: false,
        }),
    );
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr: SocketAddr = incoming.local_addr().expect("addr");
    let service = MembershipService::new(Arc::clone(&members), Admission::Open).server();
    tokio::spawn(
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming),
    );
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    (members, channel)
}

fn flushed_to(generation: u64) -> FlushReport {
    FlushReport {
        clear_gen: Some(generation),
        staged_objects: 3,
        staged_bytes: 30,
    }
}

/// Catches: a node whose heartbeat says nothing of its flush and tail (the fields not sent,
/// or sent and not stored), an answer that does not carry the tail epoch back, and a node
/// that acknowledges an epoch it was not owed. A real joiner joins a farm that holds data
/// over the wire: before it says anything its member is blocked on its own flush; once
/// it reports one (a flush report, which the node's cache makes) and the other members do,
/// the sweeper opens the fence and the next answer carries the epoch, which is then owed;
/// the node finishes its pass (what `fill::round` does) and its acknowledgement and
/// fingerprint arrive in the next beat, after which the only thing left is the settle
/// time, and a beat after it promotes the node. Catches: the request fields not
/// filled from the node's state (the barrier never opens); the answer's epoch not learned
/// (nothing is owed); the acknowledgement not sent (blocked on the epoch).
#[tokio::test]
async fn a_node_reports_its_fill_and_is_promoted_by_its_own_heartbeat() {
    let store = Hooked::new();
    let (members, channel) = serve(&store).await;
    let a = members.register(&node(1)).expect("a");
    let b = members.register(&node(2)).expect("b");
    store.clock.advance(Duration::from_secs(120));
    let mut joiner = Joiner::new(
        channel,
        Identity {
            instance_id: "node-3".to_owned(),
            addr: "192.0.2.3:8982".to_owned(),
            zone: String::new(),
            weight: 1,
        },
        MS,
    );
    let fill = joiner.fill();
    let joined = joiner.register().await.expect("registers");
    let c = MemberId(joined.member_id);
    let map = members.current_map().expect("read").expect("a map");
    assert_eq!(map.member(c).map(|m| m.state), Some(MemberState::Joining));
    let g = map.generation();

    // Nothing reported yet: the join's barrier is not met by the joiner.
    joiner.beat(&joined).await.expect("beat");
    assert!(matches!(
        members.blocked_on(c).expect("blocked").as_slice(),
        [.., Block::Flushing { member, have: None, .. }] if *member == c
    ));
    assert_eq!(fill.owed(), None);

    for who in [&a, &b] {
        members
            .heartbeat_with(
                who.id,
                who.boot,
                Progress::CaughtUp,
                Reported::default(),
                Fill {
                    flush_clear_gen: Some(g),
                    ..Fill::default()
                },
                None,
            )
            .expect("beat");
    }
    fill.flushed(Some(flushed_to(g)));
    joiner.beat(&joined).await.expect("beat");
    members.refresh().expect("refresh");
    assert_eq!(members.tail_epoch().expect("epoch"), 1);
    assert_eq!(fill.owed(), None, "not told yet");
    joiner.beat(&joined).await.expect("beat");
    assert_eq!(fill.owed(), Some(1), "the answer carries the epoch");

    // The pass is done under the map as the node holds it.
    let held = members.current_map().expect("read").expect("a map");
    let fp = fill_fp(&held, c).expect("a member");
    fill.finish(1, fp);
    assert_eq!(fill.beat().tail_epoch_done, 1);
    joiner.beat(&joined).await.expect("beat");
    store.clock.advance(SETTLE + Duration::from_secs(1));
    joiner.beat(&joined).await.expect("beat");
    let after = members.current_map().expect("read").expect("a map");
    assert_eq!(after.member(c).map(|m| m.state), Some(MemberState::Active));

    // A registration again (a restart, or a lapsed row) is a new boot, which has done no pass.
    assert_eq!(fill.beat().tail_epoch_done, 1);
    joiner.register().await.expect("registers again");
    assert_eq!(
        fill.beat().tail_epoch_done,
        0,
        "a new boot has done no pass"
    );
}

/// Catches: a node's write-tier health that never leaves the node (the heartbeat built
/// without it), arrives on other fields, or is not unknown before the node has any: a real
/// node-side [`Joiner`] beats once with the health its fill state holds and the frontend's
/// storage health shows those values for it; before the node has said any they are unknown
/// (`NONE`, never zeros); after it registers again (a restart) they are unknown until its
/// next beat, which brings them back.
#[tokio::test]
async fn a_nodes_write_tier_health_reaches_the_frontends_view_and_recovers() {
    use kbf_server::vitals::Vitals;

    let store = Hooked::new();
    let (members, channel) = serve(&store).await;
    let mut joiner = Joiner::new(
        channel,
        Identity {
            instance_id: "node-1".to_owned(),
            addr: "192.0.2.1:8982".to_owned(),
            zone: String::new(),
            weight: 1,
        },
        MS,
    );
    let fill = joiner.fill();
    let seen = |members: &Members| {
        let health = members.storage_health().expect("health");
        health.nodes[0].vitals
    };
    let said = Vitals {
        unflushed_bytes: Some(1),
        unflushed_blocks: Some(2),
        oldest_unflushed_ms: Some(3),
        budget_bytes: Some(4),
        used_bytes: Some(5),
        flush_failures: Some(6),
        flush_conflicts: Some(7),
    };

    let joined = joiner.register().await.expect("registers");
    joiner.beat(&joined).await.expect("beat");
    assert_eq!(seen(&members), Vitals::NONE, "a node with no round yet");

    fill.tiered(said);
    joiner.beat(&joined).await.expect("beat");
    assert_eq!(seen(&members), said, "every value, on its own field");

    let again = joiner.register().await.expect("registers again");
    assert_ne!(again.boot, joined.boot);
    assert_eq!(seen(&members), Vitals::NONE, "a new boot has said nothing");
    joiner.beat(&again).await.expect("beat");
    assert_eq!(seen(&members), said, "recovered");
}
