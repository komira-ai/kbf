//! A real storage node joining a farm that holds data, against a real membership service
//! that promotes joiners (`kbf_server::members::Handoff`): the node's own heartbeats carry
//! its flush generation (its cache dated by the map it holds), learn the tail epoch the
//! frontend opens, make the tail pass and acknowledge it with the placement fingerprint,
//! and the frontend promotes it from its own beat. `members_auto_promote.rs` has the rules,
//! one test per condition; this is where the node's half meets the frontend's.
//!
//! The store's clock is the test's and the lease is 60 ms, so the node beats every 20 ms of
//! real time and no row lapses while the clock stands still; the settle time is zero (a
//! settle needs the clock to move, which would lapse the rows).

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{MemberId, MemberState};
use kbf_server::members::{
    Block, Fill, Handoff, Members, MembershipConfig, PROTOCOL, Progress, Registrant,
};
use kbf_server::members_rpc::{MembershipService, sweep};
use kbf_server::storage_health::Reported;
use kbf_server::storage_node::{StorageFlags, command, parse, start_storage};
use kbf_state::Store;
use kbf_storage::service::Admission;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

const MS: Duration = Duration::from_millis(1);

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: 60 * MS,
        dwell: Duration::from_secs(1),
        replication: 2,
    }
}

fn dir(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("storage-fill-{name}"))
}

fn flags(dir: &Path, more: &[String]) -> Result<StorageFlags, clap::Error> {
    let mut argv = vec![
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
    ];
    argv.extend([
        format!("--cold-store=file://{}", dir.join("cold").display()),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--meta-dir={}", dir.join("meta").display()),
    ]);
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv)?)
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

async fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    for _ in 0..2000 {
        if let Some(found) = probe() {
            return found;
        }
        tokio::time::sleep(5 * MS).await;
    }
    panic!("timed out waiting for {what}");
}

fn registrant(instance: &str) -> Registrant {
    Registrant {
        instance_id: instance.to_owned(),
        addr: instance.to_owned(),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// Catches (join test 1 over real parts): a node whose half of the protocol is missing or
/// mismatched with the frontend's, which no unit test of either side can see: the node's
/// cache not dated by its map (it cannot say what it flushed, so the barrier never opens),
/// the epoch not carried in the answers, the tail pass not made or not acknowledged, the
/// fingerprint the node computes not the frontend's. A farm of one member that has held
/// data for over a minute; a real node joins it. The member's own report is the one thing
/// the test says (it is a node that is not run here). The joiner is `Joining`; the
/// barrier opens only when the member has reported; the node then makes its pass and is
/// promoted by its own heartbeat, and the mark goes. Catches: the registered
/// node's gate not installed (the node says it cannot say: the join stays blocked on it
/// until the test gives up); the node not learning the epoch from the answer (it never
/// acknowledges); `fill::run` not spawned (the same).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_node_joining_a_farm_with_data_is_promoted_by_its_own_heartbeat() {
    let store = Hooked::new();
    let members = Arc::new(
        Members::new(Arc::clone(&store) as Arc<dyn Store>, config()).with_handoff(Handoff {
            settle: Duration::ZERO,
            allow_degraded: false,
        }),
    );
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let frontend: SocketAddr = incoming.local_addr().expect("addr");
    let service = MembershipService::new(Arc::clone(&members), Admission::Open).server();
    tokio::spawn(
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming),
    );
    tokio::spawn(sweep(Arc::clone(&members)));

    // The farm's one member formed it, and has held data since the bootstrap minute.
    members
        .register(&registrant("198.51.100.9:8982"))
        .expect("the farm's member");
    store.clock.advance(Duration::from_secs(61));
    let old = members
        .register(&registrant("198.51.100.9:8982"))
        .expect("it restarts");
    assert_eq!(
        old.map.member(old.id).map(|m| m.state),
        Some(MemberState::Active)
    );

    let node_dir = dir("joins");
    let node = start_storage(
        &flags(&node_dir, &[format!("--frontend=http://{frontend}")]).expect("flags"),
        never,
    )
    .await
    .expect("starts");
    let _serving = tokio::spawn(node.serve());
    let joiner = MemberId(2);
    let joining = until("the node to join", || {
        let map = members.current_map().ok()??;
        (map.member(joiner)?.state == MemberState::Joining).then_some(map)
    })
    .await;
    let g = joining.generation();

    // The node says what it flushed, from its cache dated by the map it holds; the one
    // member has not said, and the barrier waits for it.
    until("the node's own flush report", || {
        let blocked = members.blocked_on(joiner).ok()?;
        (blocked
            .iter()
            .any(|b| matches!(b, Block::Flushing { member, .. } if *member == old.id))
            && !blocked
                .iter()
                .any(|b| matches!(b, Block::Flushing { member, .. } if *member == joiner)))
        .then_some(())
    })
    .await;
    assert_eq!(
        members.tail_epoch().expect("epoch"),
        0,
        "the member has not reported"
    );
    assert_eq!(
        members
            .current_map()
            .expect("read")
            .expect("a map")
            .member(joiner)
            .map(|m| m.state),
        Some(MemberState::Joining)
    );
    members
        .heartbeat_with(
            old.id,
            old.boot,
            Progress::CaughtUp,
            Reported::default(),
            Fill {
                flush_clear_gen: Some(g),
                ..Fill::default()
            },
            None,
        )
        .expect("the member reports");

    // The fence opens, the node makes its pass, and its next beat promotes it.
    let active = until("the node to be promoted", || {
        let map = members.current_map().ok()??;
        (map.member(joiner)?.state == MemberState::Active).then_some(map)
    })
    .await;
    assert!(members.tail_epoch().expect("epoch") >= 1);
    assert!(
        store.fake.get("unfilled/2").expect("read").is_none(),
        "the mark goes"
    );
    assert!(active.generation() > g);
}
