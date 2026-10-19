//! The controller starts a real Linux node (`docs/design/split-mode-updates.md` sections 2.6
//! and 2.7): a farm with real worker sessions, real members over a state store, the real
//! enrollment registry, and the controller driven look by look through the real ports. The
//! node's two halves are the test: it registers the storage member, heartbeats it, writes the
//! helper's status and registers again as the restarted process does.
//!
//! A host is joined to its storage member by the address it advertised when it enrolled,
//! both read through the one canonical spelling (ASCII lower case).

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
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
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
    clock: Arc<ManualClock>,
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
    rig_over(|store| store)
}

/// A rig whose store is `wrap`ped (to fail reads on demand).
fn rig_over(wrap: impl FnOnce(Arc<dyn Store>) -> Arc<dyn Store>) -> Rig {
    let clock = Arc::new(ManualClock::default());
    clock.advance(std::time::Duration::from_secs(10 * DAY));
    let store = wrap(Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None)));
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
        clock,
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
        self.enrolled_as(&[Role::Storage, Role::Daemon], name, advertise);
    }

    /// The node `name` enrolled for `granted` roles, advertising `advertise` (empty for a
    /// node with no storage role).
    fn enrolled_as(&mut self, granted: &[Role], name: &str, advertise: &str) {
        self.next_token += 1;
        let n = self.next_token;
        let roles = RoleSet::new(granted.iter().copied()).expect("roles");
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
        let csrs: Vec<(Role, &[u8])> = granted
            .iter()
            .map(|role| (*role, b"csr".as_slice()))
            .collect();
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
                Ok(granted
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

// ---------------------------------------------------------------------------------------------
// One spelling of an address, the shapes of host the join sees, the facts source's failures and
// its log lines, and a controller that is fenced between the mark and its undoing.
// ---------------------------------------------------------------------------------------------

/// The controller's story for one host, in the log: where the global capture writes.
static LOG: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

struct SharedLog;

impl std::io::Write for SharedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        LOG.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs the subscriber that writes to [`LOG`], at INFO (the server's level), once.
fn capture() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| SharedLog)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
    });
    // Tests run in parallel: a callsite another test's thread first reached before the
    // subscriber was installed may have cached "never", so evaluate them all again.
    tracing_core::callsite::rebuild_interest_cache();
}

/// The logged lines that contain every one of `parts`.
fn lines_with(parts: &[&str]) -> Vec<String> {
    let log = LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    String::from_utf8_lossy(&log)
        .lines()
        .filter(|line| parts.iter().all(|p| line.contains(p)))
        .map(str::to_owned)
        .collect()
}

/// A store that fails the reads it is told to, so the facts source's errors can be seen.
struct Faulty {
    inner: Arc<dyn Store>,
    /// Fail `get` of the map.
    map: Arc<std::sync::atomic::AtomicBool>,
    /// Fail the scan of the enrollment registry's node rows.
    registry: Arc<std::sync::atomic::AtomicBool>,
    /// Fail `get` of a member's row.
    member_row: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone)]
struct Faults {
    map: Arc<std::sync::atomic::AtomicBool>,
    registry: Arc<std::sync::atomic::AtomicBool>,
    member_row: Arc<std::sync::atomic::AtomicBool>,
}

fn faulty() -> (Faults, impl FnOnce(Arc<dyn Store>) -> Arc<dyn Store>) {
    let faults = Faults {
        map: Arc::default(),
        registry: Arc::default(),
        member_row: Arc::default(),
    };
    let held = faults.clone();
    (faults, move |inner| {
        Arc::new(Faulty {
            inner,
            map: held.map,
            registry: held.registry,
            member_row: held.member_row,
        })
    })
}

fn failing(flag: &std::sync::atomic::AtomicBool) -> bool {
    flag.load(std::sync::atomic::Ordering::SeqCst)
}

fn down() -> StoreError {
    StoreError::Failed("down".to_owned())
}

impl Store for Faulty {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        let broken = (key == "map" && failing(&self.map))
            || (key.starts_with("member/") && failing(&self.member_row));
        if broken {
            return Err(down());
        }
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        if prefix.starts_with("enroll/node/") && failing(&self.registry) {
            return Err(down());
        }
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// Catches a member whose registered spelling differs from the enrolled one being passed over
/// (the registry stores an advertised address in lower case; a node registers the address it
/// was configured with), and the two spellings being kept apart. The storage half registers
/// `S1.Example:7000` and the host enrolled advertising `s1.EXAMPLE:7000`: the member is stored
/// in the one canonical spelling, the host is joined to it and started (the mark is written),
/// and `canonical_addr` is the function both sides use.
///
/// Mutant seen red: `Members::register` storing `who.addr` as given (the member reads back
/// as `S1.Example:7000`).
#[test]
fn a_member_and_a_host_that_spell_one_address_differently_are_joined() {
    assert_eq!(
        kbf_enroll_registry::canonical_addr("S1.Example:7000"),
        "s1.example:7000"
    );
    assert_eq!(
        kbf_enroll_registry::canonical_addr("10.0.0.1:7000"),
        "10.0.0.1:7000",
        "an address with no letters is as it was"
    );
    let mut rig = rig();
    let node = rig.member("storage-1", "S1.Example:7000");
    let _peer = rig.member("storage-2", "s2.example:7000");
    rig.enrolled("linux-1", "s1.EXAMPLE:7000");
    let mut daemon = rig.daemon("linux-1");
    assert_eq!(rig.member_look(node).addr, "s1.example:7000");
    let mut orders = Vec::new();
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, _| {
        rig.member_look(node).planned_down
    });
    rig.facts();
    assert_eq!(rig.observer.unlinked(&daemon.id), None);
}

/// Catches two members that are one address being told apart by case: `S1.example:7000` and
/// `s1.example:7000` are the same host, so a host advertising it matches more than one member
/// and is not started (it could be joined to the wrong node's storage), and the reason says
/// so.
///
/// Mutant seen red: the first match taken whatever follows it (the host is started).
#[test]
fn two_members_at_one_address_in_two_cases_link_nothing() {
    let mut rig = rig();
    let first = rig.member("storage-1", "S1.example:7000");
    let second = rig.member("storage-2", "s1.example:7000");
    assert_ne!(first, second);
    rig.enrolled("linux-1", "s1.example:7000");
    let mut daemon = rig.daemon("linux-1");
    rig.facts();
    let why = rig.observer.unlinked(&daemon.id).expect("not linked");
    assert!(
        why.contains("more than one storage member registered"),
        "{why}"
    );
    for _ in 0..5 {
        rig.tick();
    }
    assert_eq!(daemon.orders(), []);
    assert_eq!(
        rig.phase("linux-1"),
        None,
        "a host joined to no one started"
    );
    assert!(!cordoned(&rig.farm, &daemon.id));
}

/// Catches a Linux host with no storage half (it enrolled for the daemon role alone, so it
/// advertised nothing) being held back for want of a member: it is started with no member
/// anywhere in the farm, no mark, and is sent the activation.
///
/// Mutant seen red: an empty advertised address read as one that no member registered (the
/// host is left unlinked and never started).
#[test]
fn a_daemon_only_linux_host_is_started_without_a_member() {
    let mut rig = rig();
    rig.enrolled_as(&[Role::Daemon], "linux-1", "");
    let mut daemon = rig.daemon("linux-1");
    let mut orders = Vec::new();
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |_, orders| {
        orders.iter().any(|o| o.verb == Verb::Activate as i32)
    });
    rig.facts();
    assert_eq!(rig.observer.unlinked(&daemon.id), None);
    assert_eq!(rig.members.look().expect("a look").members, []);
    assert!(cordoned(&rig.farm, &daemon.id));
}

/// Catches the ports printing nothing useful (or too much) when a test or a log line prints
/// them: the transport prints its name, the facts source the hosts it holds back and the
/// reasons.
///
/// Mutant seen red: the `not_idle` field left out of the facts source's print.
#[test]
fn the_ports_print_what_they_hold_back() {
    let mut rig = rig();
    rig.member("storage-1", "s1.example:7000");
    rig.enrolled("linux-1", "nowhere.example:7000");
    let _daemon = rig.daemon("linux-1");
    let (transport, mut facts) = ports(
        Arc::clone(&rig.farm),
        rig.rows.clone(),
        Arc::clone(&rig.members),
    );
    assert_eq!(format!("{transport:?}"), "SessionTransport { .. }");
    assert_eq!(
        format!("{facts:?}"),
        "FarmFacts { unlinked: {}, not_idle: {}, .. }"
    );
    facts.look(rig.farm.wall_now()).expect("a look");
    let printed = format!("{facts:?}");
    assert!(
        printed.contains(
            "unlinked: {WorkerId(\"linux-1\"): \"linux-1 advertises nowhere.example:7000"
        ),
        "{printed}"
    );
    assert!(printed.contains("not_idle: {}"), "{printed}");
}

/// Catches a host held back for want of a member being held back in silence, or in a flood:
/// the reason is a warning at the attention target, naming the host and what it advertised,
/// said once while it lasts and again when it changes.
///
/// Mutant seen red: the warning said on every look (two lines for three looks).
#[test]
fn an_unlinked_host_is_warned_about_once_and_again_when_the_reason_changes() {
    capture();
    let mut rig = rig();
    rig.enrolled("linux-warn-1", "nowhere.example:7000");
    let _daemon = rig.daemon("linux-warn-1");
    for _ in 0..3 {
        rig.facts();
    }
    let said = lines_with(&["linux-warn-1 advertises nowhere.example:7000"]);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains("WARN") && said[0].contains("the host is not started"),
        "{said:?}"
    );
    // A member appears at the address twice (two instances): another reason, said again.
    rig.member("storage-1", "nowhere.example:7000");
    rig.member("storage-2", "NOWHERE.example:7000");
    rig.facts();
    rig.facts();
    let again = lines_with(&["linux-warn-1 advertises nowhere.example:7000"]);
    assert_eq!(again.len(), 2, "{again:?}");
    assert!(again[1].contains("more than one"), "{again:?}");
}

/// Catches an operator not being told why a node that is marked down is not stopped: a wait
/// (writes still open) is an info line and a hold (the other member is draining, so the node
/// must not be stopped) a warning at the attention target, each naming the host, each said
/// once while the reason lasts.
///
/// Mutants seen red: the two classes swapped (the wait warns); the reason said on every look.
#[test]
fn a_marked_node_that_is_not_idle_says_why_once_by_class() {
    capture();
    let mut rig = rig();
    let node = rig.member("storage-1", "s1.example:7000");
    let peer = rig.member("storage-2", "s2.example:7000");
    rig.enrolled("linux-idle-1", "s1.example:7000");
    let mut daemon = rig.daemon("linux-idle-1");
    let mut orders = Vec::new();
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, _| {
        rig.member_look(node).planned_down
    });
    // Three writes open: a wait.
    rig.storage[0].open_writes = 3;
    rig.beat();
    let before = lines_with(&["linux-idle-1", "marked down and not idle yet"]).len();
    for _ in 0..3 {
        rig.facts();
    }
    let waited = lines_with(&["linux-idle-1", "marked down and not idle yet"]);
    assert_eq!(waited.len() - before, 1, "{waited:?}");
    assert!(
        waited.last().is_some_and(|l| l.contains("INFO")),
        "{waited:?}"
    );
    // The other member drains: a hold.
    rig.members
        .set_state(peer, MemberState::Draining)
        .expect("drained");
    rig.beat();
    let before = lines_with(&["linux-idle-1", "must not be stopped"]).len();
    for _ in 0..3 {
        rig.facts();
    }
    let held = lines_with(&["linux-idle-1", "must not be stopped"]);
    assert_eq!(held.len() - before, 1, "{held:?}");
    assert!(
        held.last()
            .is_some_and(|l| l.contains("WARN") && l.contains("attention")),
        "{held:?}"
    );
}

/// Catches the look at the members dropping what a report does not say: a member whose newest
/// report says the debts and the writes are unknown reads as owing nothing and having
/// nothing open (the controller would stop it), instead of as unknown. The report's map
/// generation is still shown; with the report saying, the numbers are shown.
///
/// Mutants seen red: `Debts::Unknown` read as `Some(0)`; `Writes::Unknown` read as `Some(0)`.
#[test]
fn a_report_that_does_not_say_is_not_read_as_nothing() {
    let mut rig = rig();
    let id = rig.member("storage-1", "s1.example:7000");
    let generation = rig
        .members
        .current_map()
        .expect("map")
        .map_or(0, |m| m.generation());
    let boot = rig.storage[0].boot;
    let beat = |said: Said| {
        rig.members
            .heartbeat(id, boot, Progress::CaughtUp, said, None)
            .expect("a heartbeat");
    };
    beat(Said::debts(Debts::Known(2)).with_writes(Writes::Known(5), generation));
    let said = rig.member_look(id);
    assert_eq!(
        (said.repair_debt, said.open_writes, said.reported_generation),
        (Some(2), Some(5), Some(generation))
    );
    beat(Said::debts(Debts::Unknown).with_writes(Writes::Unknown, generation));
    let unknown = rig.member_look(id);
    assert_eq!(
        (
            unknown.repair_debt,
            unknown.open_writes,
            unknown.reported_generation
        ),
        (None, None, Some(generation))
    );
    assert!(unknown.heard_ago.is_some());
}

/// Catches a member the sweeper took down (it stopped beating; nobody planned it) being read
/// as planned: it has a `down/` mark, but an empty one, so it is `Down`, not planned down,
/// and the time since a plan was marked is not shown.
///
/// Mutant seen red: any mark read as a plan (`planned_down` true, `marked_ago` shown).
#[test]
fn a_member_that_stopped_beating_is_down_and_not_planned() {
    let mut rig = rig();
    let id = rig.member("storage-1", "s1.example:7000");
    let _peer = rig.member("storage-2", "s2.example:7000");
    rig.clock
        .advance(MembershipConfig::default().ttl + std::time::Duration::from_secs(1));
    rig.members.reap().expect("reaped");
    rig.members.refresh().expect("refreshed");
    assert!(
        rig.store
            .get(&format!("down/{}", id.0))
            .expect("a read")
            .is_some(),
        "the sweeper's mark"
    );
    let look = rig.member_look(id);
    assert_eq!(look.state, MemberState::Down);
    assert!(!look.planned_down);
    assert_eq!(look.marked_ago, None);
    assert_eq!(look.heard_ago, None);
}

/// Catches a store that fails mid-look being read as a farm with nothing in it: the members
/// failing to be read, and the enrollment registry failing to be read, are each a failed look
/// whose message names which, and the look after the store is back succeeds.
///
/// Mutants seen red: the members' failure read as no members (the look succeeds); the two
/// messages swapped.
#[test]
fn a_facts_look_that_cannot_read_the_members_or_the_registry_fails_and_says_which() {
    let (faults, wrap) = faulty();
    let mut rig = rig_over(wrap);
    rig.member("storage-1", "s1.example:7000");
    rig.enrolled("linux-1", "s1.example:7000");
    let _daemon = rig.daemon("linux-1");
    rig.facts();
    let flag = |flag: &Arc<std::sync::atomic::AtomicBool>, on: bool| {
        flag.store(on, std::sync::atomic::Ordering::SeqCst);
    };

    flag(&faults.map, true);
    let error = rig.observer.look(rig.farm.wall_now()).expect_err("fails");
    assert!(
        error.to_string().contains("the members could not be read"),
        "{error}"
    );
    flag(&faults.map, false);

    flag(&faults.registry, true);
    let error = rig.observer.look(rig.farm.wall_now()).expect_err("fails");
    assert!(
        error
            .to_string()
            .contains("the enrollment registry could not be read"),
        "{error}"
    );
    flag(&faults.registry, false);
    rig.facts();
}

/// Catches an undoing that fails (the store cannot read the member's row) being taken for
/// done: the host is not given back, the failure is reported with the member in it, and the
/// next look, with the store back, finishes the give-back: `Active`, no mark, host `Pending`.
///
/// Mutant seen red: every undoing error read as nothing to undo (no error, the mark stays
/// when the store is back and the host is gone from its step).
#[test]
fn a_failed_undoing_is_reported_and_tried_again() {
    let (faults, wrap) = faulty();
    let mut rig = rig_over(wrap);
    let node = rig.member("storage-1", "s1.example:7000");
    let peer = rig.member("storage-2", "s2.example:7000");
    rig.enrolled("linux-1", "s1.example:7000");
    let mut daemon = rig.daemon("linux-1");
    let mut orders = Vec::new();
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, _| {
        rig.member_look(node).planned_down
    });
    rig.members
        .set_state(peer, MemberState::Draining)
        .expect("drained");
    let mut reported = Vec::new();
    for _ in 0..LOOKS {
        // The member beats (the store is well), then the look finds its row unreadable.
        rig.beat();
        faults
            .member_row
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let tick = rig.controller.tick(rig.farm.wall_now()).expect("a look");
        faults
            .member_row
            .store(false, std::sync::atomic::Ordering::SeqCst);
        reported.extend(tick.errors.iter().map(|e| format!("{e:?}")));
        if !reported.is_empty() {
            break;
        }
    }
    assert!(
        reported.iter().any(|e| e.contains("cannot undo the mark")),
        "{reported:?}"
    );
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, _| {
        rig.phase("linux-1") == Some(HostPhase::Pending)
    });
    let after = rig.member_look(node);
    assert_eq!(after.state, MemberState::Active);
    assert!(!after.planned_down);
}

/// Catches a controller that is handed over after it marked a member down stranding the
/// member. The fenced controller may not undo its mark (the successor may have planned the
/// same member again, and an undo would put it back in service under the successor's feet),
/// so the mark is left; but a plan ends by itself: the member that goes on beating is `Down`
/// no longer than the restart deadline, then `Active` by its own heartbeat with no mark in
/// force, and nothing was ordered in between.
///
/// Mutants seen red: an undoing that ignores the fence (the member is `Active` straight away,
/// before the successor has acted); a plan that does not end (the member is still planned
/// down after the deadline).
#[test]
fn a_controller_handed_over_after_the_mark_does_not_strand_the_member() {
    let mut rig = rig();
    let node = rig.member("storage-1", "s1.example:7000");
    let peer = rig.member("storage-2", "s2.example:7000");
    rig.enrolled("linux-1", "s1.example:7000");
    let mut daemon = rig.daemon("linux-1");
    let mut orders = Vec::new();
    until(&mut rig, &mut daemon, LOOKS, &mut orders, |rig, _| {
        rig.member_look(node).planned_down
    });
    // The give-back is due (the other member drains), and the farm is handed over first.
    rig.members
        .set_state(peer, MemberState::Draining)
        .expect("drained");
    rig.farm.fence();
    let mut refused = Vec::new();
    for _ in 0..5 {
        rig.beat();
        if let Ok(tick) = rig.controller.tick(rig.farm.wall_now()) {
            refused.extend(tick.errors.iter().map(|e| format!("{e:?}")));
        }
        orders.extend(daemon.orders());
    }
    assert_eq!(orders, [], "an order from a handed-over farm");
    let held = rig.member_look(node);
    assert_eq!(
        (held.state, held.planned_down),
        (MemberState::Down, true),
        "a fenced controller undid its mark: {refused:?}"
    );

    // The old process beats on; the plan runs out.
    let config = MembershipConfig::default();
    let deadline = rig.rows.policy().expect("a policy").restart_deadline();
    let mut waited = std::time::Duration::ZERO;
    while waited <= deadline {
        rig.clock.advance(config.beat());
        waited += config.beat();
        rig.beat();
    }
    let after = rig.member_look(node);
    assert_eq!(after.state, MemberState::Active, "stranded Down");
    assert!(!after.planned_down);
    assert_eq!(after.marked_ago, None);
}
