//! The controller starts a real Linux node (`docs/design/split-mode-updates.md` sections 2.6
//! and 2.7): a farm with real worker sessions, real members over a state store, the real
//! enrollment registry, and the controller driven look by look through the real ports. The
//! node's two halves are the test: it registers the storage member, heartbeats it, writes the
//! helper's status and registers again as the restarted process does.
//!
//! A host is joined to its storage member by the address it advertised when it enrolled,
//! matched byte for byte.

use std::sync::Arc;

use kbf_caps::NodeCaps;
use kbf_enroll_registry::{EnrollRequest, IssuedRole, TokenSpec, create_token, enroll_txn};
use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::{MemberId, MemberState};
use kbf_objstore::MemoryStore;
use kbf_proto::update::update_order::Verb;
use kbf_proto::update::update_status::State;
use kbf_proto::update::{UpdateOrder, UpdateStatus};
use kbf_proto::worker::{ServerMessage, server_message};
use kbf_server::farm::{Farm, Reported, StreamId};
use kbf_server::members::{
    MemberLook, Members, MembershipConfig, MembershipError, PROTOCOL, Progress, Registrant,
};
use kbf_server::storage_health::{Class, Debts, NotIdle, Reported as Said, Writes};
use kbf_server::update::{Controller, FactsSource, FarmFacts, HostPhase, Release, Rows, ports};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{NodeId, Resources, Role, RoleSet, TokenId, TokenSecret, WorkerId};
use tokio::sync::mpsc;

const A: &str = "0123456789abcdef0123456789abcdef01234567";
const OLD12: &str = "fedcba987654";
const DAY: u64 = 24 * 3600;

type Inbound = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

fn farm() -> Arc<Farm> {
    Arc::new(Farm::new(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ))
}

/// A storage member as the node's storage half is: registered, and beating.
struct Storage {
    id: MemberId,
    boot: u64,
    instance: String,
    addr: String,
    /// The writes it says it has open.
    open_writes: u64,
    /// Whether it keeps up with the map; false pins what it reports to generation 1.
    follows_map: bool,
}

/// A Linux daemon: a session on the farm.
struct Daemon {
    id: WorkerId,
    stream: StreamId,
    inbound: Inbound,
}

struct Rig {
    farm: Arc<Farm>,
    store: Arc<dyn Store>,
    members: Arc<Members>,
    rows: Rows,
    controller: Controller,
    observer: FarmFacts,
    storage: Vec<Storage>,
    next_token: u8,
}

fn rig() -> Rig {
    let clock = Arc::new(ManualClock::default());
    clock.advance(std::time::Duration::from_secs(10 * DAY));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let farm = farm();
    let members = Arc::new(Members::new(
        Arc::clone(&store),
        MembershipConfig::default(),
    ));
    let rows = Rows::new(Arc::clone(&store));
    rows.offer(&Release::new(A), farm.wall_now())
        .expect("a run");
    let (transport, source) = ports(Arc::clone(&farm), rows.clone(), Arc::clone(&members));
    let controller = Controller::new(
        Arc::clone(&store),
        farm.lease_epoch(),
        Box::new(transport),
        Box::new(source),
    );
    let (_, observer) = ports(Arc::clone(&farm), rows.clone(), Arc::clone(&members));
    Rig {
        farm,
        store,
        members,
        rows,
        controller,
        observer,
        storage: Vec::new(),
        next_token: 0,
    }
}

impl Rig {
    /// A storage member at `addr`, registered whole (`Active`).
    fn member(&mut self, instance: &str, addr: &str) -> MemberId {
        let registration = self
            .members
            .register(&registrant(instance, addr))
            .expect("registered");
        self.storage.push(Storage {
            id: registration.id,
            boot: registration.boot,
            instance: instance.to_owned(),
            addr: addr.to_owned(),
            open_writes: 0,
            follows_map: true,
        });
        registration.id
    }

    /// The node `name` enrolled for both roles, advertising `advertise`.
    fn enrolled(&mut self, name: &str, advertise: &str) {
        self.next_token += 1;
        let n = self.next_token;
        let roles = RoleSet::new([Role::Storage, Role::Daemon]).expect("roles");
        let spec = TokenSpec {
            roles,
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
        create_token(
            &*self.store,
            &token,
            &TokenSecret::from_bytes([n; 32]),
            &spec,
        )
        .expect("token");
        let node: NodeId = name.parse().expect("a node id");
        let csrs = [
            (Role::Storage, b"csr-storage".as_slice()),
            (Role::Daemon, b"csr-daemon".as_slice()),
        ];
        let now = self.store.now().as_duration().as_secs();
        let seed = n;
        enroll_txn(
            &*self.store,
            &EnrollRequest {
                token_id: &token,
                secret: &[n; 32],
                node_id: &node,
                csrs: &csrs,
                advertise,
                zone: "z",
            },
            &mut || {
                Ok([Role::Storage, Role::Daemon]
                    .iter()
                    .enumerate()
                    .map(|(i, role)| {
                        let mut spki = [0u8; 32];
                        spki[0] = seed;
                        spki[1] = u8::try_from(i).expect("small");
                        IssuedRole {
                            role: *role,
                            spki_sha256: spki,
                            serial: vec![0, seed, spki[1]],
                            not_before: now - 300,
                            not_after: now + 7 * DAY,
                            cert_der: vec![seed, spki[1]],
                        }
                    })
                    .collect())
            },
        )
        .expect("enrolled");
    }

    /// A Linux daemon `name`, running the old release.
    fn daemon(&self, name: &str) -> Daemon {
        let (id, stream, inbound) = self.session(name);
        let daemon = Daemon {
            id,
            stream,
            inbound,
        };
        self.say(&daemon, OLD12, State::Idle, 0, "");
        daemon
    }

    fn session(&self, name: &str) -> (WorkerId, StreamId, Inbound) {
        let (outbound, inbound) = mpsc::unbounded_channel();
        let caps = NodeCaps::from_report([("arch", "arm64"), ("os", "linux")]).expect("caps");
        let id = WorkerId::new(name);
        let stream = self.farm.register(
            &id,
            kbf_sched::DaemonInstance::new(name),
            kbf_sched::DaemonInstance::default(),
            Reported::serving(Resources::new(8_000, 16 << 30), caps),
            outbound,
            ServerMessage::default(),
        );
        (id, stream, inbound)
    }

    /// The daemon's `UpdateStatus`, as its session sends it after every change.
    fn say(&self, daemon: &Daemon, running: &str, state: State, acked: u64, installed: &str) {
        self.farm.update_status(
            &daemon.id,
            daemon.stream,
            UpdateStatus {
                running_commit: running.to_owned(),
                installed_commit: installed.to_owned(),
                state: state as i32,
                acked_order_id: acked,
                ..UpdateStatus::default()
            },
        );
    }

    /// Every storage member sends a heartbeat, as they do each beat.
    fn beat(&self) {
        let generation = self
            .members
            .current_map()
            .expect("map")
            .map_or(0, |m| m.generation());
        for s in &self.storage {
            let seen = if s.follows_map { generation } else { 1 };
            let said = Said::debts(Debts::Known(0)).with_writes(Writes::Known(s.open_writes), seen);
            self.members
                .heartbeat(s.id, s.boot, Progress::CaughtUp, said, None)
                .expect("a heartbeat");
        }
    }

    /// One look of the controller, after the members' beat.
    fn tick(&mut self) {
        self.beat();
        let tick = self.controller.tick(self.farm.wall_now()).expect("a look");
        assert!(
            tick.errors.is_empty(),
            "an order did not go: {:?}",
            tick.errors
        );
    }

    fn phase(&self, host: &str) -> Option<HostPhase> {
        self.rows
            .hosts()
            .expect("rows")
            .get(&WorkerId::new(host))
            .map(|row| row.value.record.phase.clone())
    }

    fn member_look(&self, id: MemberId) -> MemberLook {
        self.members
            .look()
            .expect("a look")
            .members
            .into_iter()
            .find(|m| m.id == id)
            .expect("listed")
    }

    /// The storage half restarts: the process registers again, a new boot counter, `Active`.
    fn restart_storage(&mut self, id: MemberId) {
        let s = self.storage.iter_mut().find(|s| s.id == id).expect("known");
        let registration = self
            .members
            .register(&registrant(&s.instance, &s.addr))
            .expect("registered again");
        assert_eq!(registration.id, id);
        s.boot = registration.boot;
    }

    /// The observer's look: what the facts say now.
    fn facts(&mut self) {
        self.observer
            .look(self.farm.wall_now())
            .expect("the observer's look");
    }
}

fn registrant(instance: &str, addr: &str) -> Registrant {
    Registrant {
        instance_id: instance.to_owned(),
        addr: addr.to_owned(),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

impl Daemon {
    fn orders(&mut self) -> Vec<UpdateOrder> {
        let mut orders = Vec::new();
        while let Ok(sent) = self.inbound.try_recv() {
            if let Some(server_message::Message::UpdateOrder(order)) =
                sent.expect("no error").message
            {
                orders.push(order);
            }
        }
        orders
    }
}

fn cordoned(farm: &Farm, node: &WorkerId) -> bool {
    matches!(
        farm.node_view(node).expect("listed").placement,
        kbf_server::fleet::PlacementView::Cordoned
    )
}

/// Ticks until `done`, at most `looks` times, collecting the daemon's orders.
fn until(
    rig: &mut Rig,
    daemon: &mut Daemon,
    looks: usize,
    orders: &mut Vec<UpdateOrder>,
    mut done: impl FnMut(&Rig, &[UpdateOrder]) -> bool,
) {
    for _ in 0..looks {
        if done(rig, orders) {
            return;
        }
        rig.tick();
        orders.extend(daemon.orders());
    }
    assert!(
        done(rig, orders),
        "not done after {looks} looks: {orders:?}"
    );
}

const LOOKS: usize = 40;

/// A farm of two storage members, the second with no host here, and a Linux daemon `linux-1`
/// that enrolled advertising the first's address.
fn two_members() -> (Rig, Daemon, MemberId, MemberId) {
    let mut rig = rig();
    let node = rig.member("storage-1", "s1.example:7000");
    let peer = rig.member("storage-2", "s2.example:7000");
    rig.enrolled("linux-1", "s1.example:7000");
    let daemon = rig.daemon("linux-1");
    (rig, daemon, node, peer)
}

/// Catches a controller that cannot take a real storage node through its update, and one that
/// takes it in the wrong order. A storage-and-daemon host whose advertised address is its
/// member's is started; the member is planned down (a `Down` member in the map, the mark
/// written) before the activation order is sent; the order goes out while the mark holds;
/// the node restarts (the storage half registers again with a larger boot counter, `Active`,
/// the mark gone); the daemon is back at the release; the confirmation goes out; the host is
/// done and uncordoned, and the member is `Active` and not planned down.
///
/// Mutants seen red: the join skipped (the host is never started); `plan_down` not called
/// (the host never gets past the mark: no activation, and the member was never planned
/// down); the activation sent before the mark (the order is seen with no mark).
#[test]
fn a_storage_host_linked_by_address_is_marked_down_activated_restarted_and_made_active() {
    let (mut rig, mut daemon, node, _peer) = two_members();
    let boot_before = rig.member_look(node).boot;
    let mut events: Vec<&str> = Vec::new();
    let mut orders: Vec<UpdateOrder> = Vec::new();

    // Up to the activation: the mark is seen before the order, and holds when it arrives.
    let mut marked_when_activated = None;
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, orders| {
        let look = rig.member_look(node);
        if look.planned_down && !events.contains(&"marked down") {
            assert_eq!(
                look.state,
                MemberState::Down,
                "a mark with the member still serving"
            );
            events.push("marked down");
        }
        if orders.iter().any(|o| o.verb == Verb::Activate as i32) {
            marked_when_activated = Some(look.planned_down);
            events.push("activation order");
            true
        } else {
            false
        }
    });
    assert_eq!(marked_when_activated, Some(true), "events: {events:?}");
    assert!(cordoned(&rig.farm, &daemon.id));
    let activation = orders
        .iter()
        .find(|o| o.verb == Verb::Activate as i32)
        .cloned()
        .expect("an activation");
    assert_eq!(activation.commit, A);

    // The helper flips and restarts the daemon; the storage half registers again.
    rig.restart_storage(node);
    events.push("storage restarted");
    let (_, stream, inbound) = rig.session("linux-1");
    (daemon.stream, daemon.inbound) = (stream, inbound);
    rig.say(
        &daemon,
        &A[..12],
        State::ActivatedUnconfirmed,
        activation.order_id,
        A,
    );
    let mut orders = Vec::new();
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |_, orders| {
        orders.iter().any(|o| o.verb == Verb::Confirm as i32)
    });
    events.push("confirmation order");
    let confirmation = orders
        .iter()
        .find(|o| o.verb == Verb::Confirm as i32)
        .cloned()
        .expect("a confirmation");
    rig.say(
        &daemon,
        &A[..12],
        State::Confirmed,
        confirmation.order_id,
        A,
    );
    let mut orders = Vec::new();
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, _| {
        rig.phase("linux-1") == Some(HostPhase::Done)
    });
    events.push("done");

    let after = rig.member_look(node);
    assert_eq!(after.state, MemberState::Active);
    assert!(!after.planned_down);
    assert!(after.boot > boot_before, "the restart is a new boot");
    assert!(!cordoned(&rig.farm, &daemon.id));
    assert_eq!(
        events,
        [
            "marked down",
            "activation order",
            "storage restarted",
            "confirmation order",
            "done"
        ]
    );
}

/// Catches a host joined to a member it does not own. Two hosts advertise `s1.example:7000`
/// and `s2.example:7000`; the members that exist registered `s1.example:70001` (the advertised
/// address is a prefix of it) and `s2.example:700` (it is a prefix of the advertised one), a
/// third node is not in the registry at all, and a peer member exists. None of them is
/// started: no cordon, no mark, no order, each said to be unlinked with the address it
/// advertised in the reason. Then the member that registers `s1.example:7000` links the
/// first host, which is started (cordoned) while the second is still left alone.
///
/// Mutants seen red: the match on `member.addr.starts_with(advertised)` (the first host is
/// started on the longer address); on `advertised.starts_with(member.addr)` (the second);
/// the registry's absence read as a daemon-only host (the third is started).
#[test]
fn a_host_whose_address_is_no_members_is_not_started_and_says_why() {
    let mut rig = rig();
    let prefix_of_member = rig.member("long", "s1.example:70001");
    let member_is_prefix = rig.member("short", "s2.example:700");
    let peer = rig.member("peer", "s3.example:7000");
    rig.enrolled("linux-a", "s1.example:7000");
    rig.enrolled("linux-b", "s2.example:7000");
    let mut a = rig.daemon("linux-a");
    let mut b = rig.daemon("linux-b");
    let mut unknown = rig.daemon("linux-unenrolled");
    let mut sent = Vec::new();
    for _ in 0..LOOKS {
        rig.tick();
        for d in [&mut a, &mut b, &mut unknown] {
            sent.extend(d.orders());
        }
    }
    assert_eq!(sent, [], "an unlinked host was ordered");
    for host in [&a, &b, &unknown] {
        assert!(!cordoned(&rig.farm, &host.id), "{} was cordoned", host.id);
        assert_eq!(rig.phase(host.id.as_str()), None, "{} has a row", host.id);
    }
    for id in [prefix_of_member, member_is_prefix, peer] {
        let look = rig.member_look(id);
        assert!(!look.planned_down, "member {id} was marked down");
        assert_eq!(look.state, MemberState::Active);
    }
    rig.facts();
    let why = |rig: &Rig, host: &Daemon| rig.observer.unlinked(&host.id).map(str::to_owned);
    let a_why = why(&rig, &a).expect("a reason");
    assert!(a_why.contains("s1.example:7000"), "{a_why}");
    assert!(a_why.contains("no storage member registered"), "{a_why}");
    let b_why = why(&rig, &b).expect("a reason");
    assert!(b_why.contains("s2.example:7000"), "{b_why}");
    let unknown_why = why(&rig, &unknown).expect("a reason");
    assert!(unknown_why.contains("enrollment registry"), "{unknown_why}");

    // The member at the exact address links the first host, and only that one starts.
    let exact = rig.member("exact", "s1.example:7000");
    until(&mut rig, &mut a, LOOKS, &mut Vec::new(), |rig, _| {
        cordoned(&rig.farm, &WorkerId::new("linux-a"))
    });
    assert!(rig.phase("linux-a").is_some());
    assert_eq!(rig.phase("linux-b"), None);
    assert_eq!(rig.phase("linux-unenrolled"), None);
    rig.facts();
    assert_eq!(rig.observer.unlinked(&a.id), None);
    assert!(rig.observer.unlinked(&b.id).is_some());
    assert_ne!(exact, prefix_of_member);
}

/// Catches a node taken down while it is not idle. The host is picked while quiet and marked
/// down, and then the member reports three open writes: the controller waits at the mark, sends no activation, and the
/// facts say it waits for the open writes (a `Wait` reason). The writes end but the member's
/// last report was made under an older map than the one that took it out of the write path:
/// still no activation, and the reason is that report (`Wait`). When it reports under the
/// current map with nothing open, the activation goes out and no reason remains.
///
/// Mutants seen red: both write counts read as zero whatever the member says (the activation
/// goes out at once, with three writes open); only the routed count read as zero (the
/// activation goes out on the stale report); the open writes read as zero (the first stage).
#[test]
fn a_node_with_open_writes_or_an_old_report_is_not_taken_down() {
    let (mut rig, mut daemon, node, _peer) = two_members();
    let host = WorkerId::new("linux-1");
    let mut orders = Vec::new();
    // Idle when it is picked (the picker waits for a quiet node), writes open after the mark.
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, _| {
        rig.member_look(node).planned_down
    });
    rig.storage[0].open_writes = 3;
    for _ in 0..LOOKS {
        rig.tick();
        orders.extend(daemon.orders());
    }
    assert_eq!(orders, [], "an activation with writes open");
    assert_eq!(rig.phase("linux-1"), Some(HostPhase::PlannedDown));
    rig.facts();
    let reason = rig.observer.not_idle(&host).cloned();
    assert_eq!(reason, Some(NotIdle::OpenWrites { member: node, n: 3 }));
    assert_eq!(reason.map(|r| r.class()), Some(Class::Wait));

    // The writes end, but the member still reports under the map before the mark.
    rig.storage[0].open_writes = 0;
    rig.storage[0].follows_map = false;
    for _ in 0..LOOKS {
        rig.tick();
        orders.extend(daemon.orders());
    }
    assert_eq!(orders, [], "an activation on a report made before the mark");
    rig.facts();
    assert!(
        matches!(
            rig.observer.not_idle(&host),
            Some(NotIdle::ReportOlderThan { .. })
        ),
        "{:?}",
        rig.observer.not_idle(&host)
    );

    // It reports under the current map with nothing open: now it goes.
    rig.storage[0].follows_map = true;
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |_, orders| {
        orders.iter().any(|o| o.verb == Verb::Activate as i32)
    });
    rig.facts();
    assert_eq!(rig.observer.not_idle(&host), None);
}

/// Catches a restart told from a stale report by anything but the boot counter. After the
/// activation order the storage half's plan is undone and it goes on beating with the boot
/// counter it had (the old process still serving, `Active`): the host stays at the rejoin and
/// never reaches the repair wait however often it is looked at. The process that registers
/// again, with a larger boot counter, moves it on.
///
/// Mutant seen red: the boot counter not compared (`restarted` true for any member), which
/// moves the host to repair on the old process.
#[test]
fn a_report_of_the_old_boot_does_not_count_as_the_node_coming_back() {
    let (mut rig, mut daemon, node, _peer) = two_members();
    let mut orders = Vec::new();
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |_, orders| {
        orders.iter().any(|o| o.verb == Verb::Activate as i32)
    });
    let activation = orders[0].clone();
    // The helper flipped; the daemon is back at the release. The member is Active again by
    // an undo, with its old boot.
    let (_, stream, inbound) = rig.session("linux-1");
    (daemon.stream, daemon.inbound) = (stream, inbound);
    rig.say(
        &daemon,
        &A[..12],
        State::ActivatedUnconfirmed,
        activation.order_id,
        A,
    );
    assert!(rig.members.undo_plan(node).expect("undone"));
    for _ in 0..LOOKS {
        rig.tick();
    }
    assert_eq!(
        rig.phase("linux-1"),
        Some(HostPhase::RejoiningStorage),
        "the old process counted as the node coming back"
    );
    rig.restart_storage(node);
    until(&mut rig, &mut daemon, LOOKS, &mut Vec::new(), |rig, _| {
        !matches!(
            rig.phase("linux-1"),
            Some(HostPhase::RejoiningStorage | HostPhase::Activating)
        )
    });
}

/// Catches a mark that outlives a start that did not go on. The host is marked down; then the
/// peer member is drained, so the gate with the node out fails: the controller gives the host
/// back, and the member must not stay planned down (`Down` in the map, held out of the write
/// path until the plan runs out). The facts, looked at before the give-back, name the peer
/// as the reason the node must not be stopped (`Held`).
///
/// Mutant seen red: the removal of the mark not given (the member is `Down` and planned down
/// after the host went back to `Pending`).
#[test]
fn a_start_given_back_unmarks_the_member() {
    let (mut rig, mut daemon, node, peer) = two_members();
    let host = WorkerId::new("linux-1");
    let mut orders = Vec::new();
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, _| {
        rig.member_look(node).planned_down
    });
    rig.members
        .set_state(peer, MemberState::Draining)
        .expect("drained");
    rig.facts();
    let reason = rig.observer.not_idle(&host).cloned();
    assert_eq!(reason, Some(NotIdle::OtherMemberNotActive { member: peer }));
    assert_eq!(reason.map(|r| r.class()), Some(Class::Held));
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, _| {
        rig.phase("linux-1") == Some(HostPhase::Pending)
    });
    let after = rig.member_look(node);
    assert!(!after.planned_down, "the member stayed planned down");
    assert_eq!(after.state, MemberState::Active);
    assert_eq!(orders, [], "an activation on a start that was given back");
    assert!(!cordoned(&rig.farm, &host));
}

/// Catches an undo that promotes a node that is not planned down: a member that registered
/// again and is rebuilding (`CatchingUp`) is left as it is and nothing changes; a member that
/// is planned down is `Active` again with the mark gone; a member with no live row is an
/// error the transport reads as nothing to undo.
///
/// Mutant seen red: `undo_plan` setting `Active` whatever the state (the rebuilding member is
/// promoted).
#[test]
fn undoing_a_plan_touches_only_a_member_that_is_planned_down() {
    let mut rig = rig();
    let id = rig.member("storage-1", "s1.example:7000");
    let _peer = rig.member("storage-2", "s2.example:7000");
    // Not planned down: nothing to undo.
    assert!(!rig.members.undo_plan(id).expect("nothing"));
    // Planned down: Active again.
    rig.members
        .plan_down(id, std::time::Duration::from_secs(270))
        .expect("planned");
    assert!(rig.member_look(id).planned_down);
    assert!(rig.members.undo_plan(id).expect("undone"));
    let after = rig.member_look(id);
    assert_eq!(
        (after.state, after.planned_down),
        (MemberState::Active, false)
    );
    // Registered again while rebuilding: left as it is.
    let rebuilding = Registrant {
        progress: Progress::CatchingUp,
        ..registrant("storage-1", "s1.example:7000")
    };
    rig.members.register(&rebuilding).expect("registered");
    assert_eq!(rig.member_look(id).state, MemberState::CatchingUp);
    assert!(!rig.members.undo_plan(id).expect("nothing"));
    assert_eq!(rig.member_look(id).state, MemberState::CatchingUp);
    // No such member: no live row.
    assert!(matches!(
        rig.members.undo_plan(MemberId(99)),
        Err(MembershipError::NotRegistered(_))
    ));
}
