//! The farm facts of a Linux host that is joined to its storage member by the address it
//! enrolled with (`update::ports`): which member it is, why a marked member is not idle yet,
//! and that nothing is kept once it is. A real farm, the enrollment registry and the members
//! over the fake store; no controller.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_enroll_registry::{EnrollRequest, IssuedRole, TokenSpec, create_token, enroll_txn};
use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::MemberId;
use kbf_objstore::MemoryStore;
use kbf_proto::worker::ServerMessage;
use kbf_server::farm::{Farm, Reported as NodeReported};
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::storage_health::{Class, Debts, NotIdle, Reported, Writes};
use kbf_server::update::{FactsSource as _, HostFacts, HostKind, Rows, ports};
use kbf_state::Store;
use kbf_types::{FarmTime, NodeId, Resources, Role, RoleSet, TokenId, TokenSecret, WorkerId};
use tokio::sync::mpsc;

const DAY: Duration = Duration::from_secs(86_400);

fn registrant(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// A Linux node joined to `farm` under `name`; the receiver is kept so its stream stays open.
fn join(farm: &Farm, name: &str) -> mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>> {
    let (outbound, inbound) = mpsc::unbounded_channel();
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    farm.register(
        &WorkerId::new(name),
        kbf_sched::DaemonInstance::new(name),
        kbf_sched::DaemonInstance::default(),
        NodeReported::serving(Resources::new(8_000, 16 << 30), caps),
        outbound,
        ServerMessage::default(),
    );
    inbound
}

/// The node `name` enrolled in the registry of `store` with `roles`, advertising `advertise`,
/// with the token numbered `n`.
fn enroll(store: &dyn Store, n: u8, name: &str, roles: &[Role], advertise: &str) {
    let spec = TokenSpec {
        roles: RoleSet::new(roles.iter().copied()).expect("roles"),
        max_uses: 1,
        ttl: None,
        node_id: None,
        node_prefix: None,
        names: Vec::new(),
        replace: false,
        label: "lab".to_owned(),
        created_by: "tester".to_owned(),
    };
    let token = TokenId::from_bits(u64::from(n));
    create_token(store, &token, &TokenSecret::from_bytes([n; 32]), &spec).expect("token");
    let node: NodeId = name.parse().expect("a node id");
    let csrs: Vec<(Role, &[u8])> = roles.iter().map(|r| (*r, b"csr".as_slice())).collect();
    let now = store.now().as_duration().as_secs();
    let request = EnrollRequest {
        token_id: &token,
        secret: &[n; 32],
        node_id: &node,
        csrs: &csrs,
        advertise,
        zone: "z",
    };
    let mut sign = || {
        Ok(roles
            .iter()
            .enumerate()
            .map(|(i, role)| {
                let mut spki = [0u8; 32];
                spki[0] = n;
                spki[1] = u8::try_from(i).expect("small");
                IssuedRole {
                    role: *role,
                    spki_sha256: spki,
                    serial: vec![0, n, spki[1]],
                    not_before: now - 300,
                    not_after: now + 7 * 86_400,
                    cert_der: vec![n, spki[1]],
                }
            })
            .collect())
    };
    enroll_txn(store, &request, &mut sign).expect("enrolled");
}

/// `member` reports no debt and no open write under the current map, as a quiet node does.
fn quiet(members: &Members, member: MemberId, boot: u64) {
    let generation = members
        .current_map()
        .expect("read")
        .expect("a map")
        .generation();
    let said = Reported::debts(Debts::Known(0)).with_writes(Writes::Known(0), generation);
    members
        .heartbeat(member, boot, Progress::CaughtUp, said, None)
        .expect("a beat");
}

fn host<'a>(hosts: &'a [HostFacts], name: &str) -> &'a HostFacts {
    hosts
        .iter()
        .find(|h| h.id.as_str() == name)
        .unwrap_or_else(|| panic!("{name} is listed"))
}

/// Catches: a Linux host linked to the wrong member (or none) when the registry holds the
/// address it enrolled with, a host enrolled with no storage address that is not daemon-only,
/// an unlinked host whose reason is not kept and then dropped once it links, a marked member
/// whose reason for not being idle is not kept (first the other member's silence, then the
/// node's own, which are held and waited for differently), and a reason kept after the member
/// is idle or no longer marked.
#[test]
fn a_host_is_linked_by_its_address_and_a_marked_member_keeps_its_reason() {
    let hooked = hooked::Hooked::new();
    hooked.clock.advance(10 * DAY);
    let store: Arc<dyn Store> = hooked.clone();
    let farm = Arc::new(Farm::new(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ));
    let members = Arc::new(Members::new(
        Arc::clone(&store),
        MembershipConfig::default(),
    ));
    let (_transport, mut source) = ports(
        Arc::clone(&farm),
        Rows::new(Arc::clone(&store)),
        Arc::clone(&members),
    );
    let _one = join(&farm, "linux-1");
    let _two = join(&farm, "linux-2");
    let now = FarmTime::from_millis(0);
    let look = |source: &mut kbf_server::update::FarmFacts| source.look(now).expect("a look").hosts;

    // Not in the registry yet: listed, linked to nothing, and the reason is kept.
    let hosts = look(&mut source);
    assert_eq!(host(&hosts, "linux-1").member, None);
    let reason = source
        .unlinked(&WorkerId::new("linux-1"))
        .expect("a reason")
        .to_owned();
    assert!(reason.contains("enrollment registry"), "{reason}");
    assert_eq!(
        look(&mut source).len(),
        2,
        "a second look lists the same hosts"
    );

    let both = [Role::Storage, Role::Daemon];
    enroll(&*store, 1, "linux-1", &both, "192.0.2.1:8982");
    enroll(&*store, 2, "linux-2", &[Role::Daemon], "");
    let a = members.register(&registrant(1)).expect("a");
    let b = members.register(&registrant(2)).expect("b");

    let hosts = look(&mut source);
    let linked = host(&hosts, "linux-1");
    assert_eq!(linked.member, Some(a.id));
    assert_eq!(linked.kind, HostKind::StorageAndDaemon);
    let daemon = host(&hosts, "linux-2");
    assert_eq!((daemon.member, daemon.kind), (None, HostKind::DaemonOnly));
    assert_eq!(source.unlinked(&WorkerId::new("linux-1")), None);
    assert_eq!(
        source.not_idle(&WorkerId::new("linux-1")),
        None,
        "not marked"
    );

    // Marked, with the other member never heard: held.
    members
        .plan_down(a.id, Duration::from_secs(600))
        .expect("marked");
    look(&mut source);
    let first = source
        .not_idle(&WorkerId::new("linux-1"))
        .cloned()
        .expect("a reason");
    assert_eq!(first, NotIdle::OtherMemberStale { member: b.id });
    assert_eq!(first.class(), Class::Held);
    look(&mut source);
    assert_eq!(source.not_idle(&WorkerId::new("linux-1")), Some(&first));

    // The other member is heard: the node's own silence is a wait.
    quiet(&members, b.id, b.boot);
    look(&mut source);
    let second = source
        .not_idle(&WorkerId::new("linux-1"))
        .cloned()
        .expect("a reason");
    assert_eq!(second, NotIdle::NodeStale { member: a.id });
    assert_eq!(second.class(), Class::Wait);

    // The node is heard too: idle, so no reason is kept.
    quiet(&members, a.id, a.boot);
    look(&mut source);
    assert_eq!(source.not_idle(&WorkerId::new("linux-1")), None, "idle");
}
