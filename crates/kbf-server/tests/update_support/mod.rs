//! Builders for the update tests: hosts and members with healthy defaults, and a test
//! changes the one field it is about.
//!
//! Every test reads the world at [`NOW`]. Time passes by moving the other end: `since`
//! earlier for a step that has waited, `wait_since` earlier for a picker that has waited,
//! `down_since` earlier for a mark that has aged. So members never need to be re-heard.

#![allow(dead_code)]

use std::collections::BTreeSet;

use kbf_map::{MemberId, MemberState};
use kbf_server::update::{
    AvailableFact, Command, DaemonLoad, Facts, Generation, HelperReport, HelperStatus, Holder,
    HostFacts, HostKind, HostPhase, HostRecord, MemberFacts, OrderId, Platform, PlatformClass,
    Policy, Release, Slot, SlotClass, Source,
};
use kbf_server::update_status::ReleaseSource;
use kbf_types::{FarmTime, WorkerId};

pub mod harness;
pub mod stores;

/// The release every host runs at the start.
pub const OLD: &str = "v1";
/// The release the run drives to.
pub const NEW: &str = "v2";
/// The time of the look, in seconds. Large, so a test can say a step began hours before.
pub const NOW: u64 = 1_000_000;
/// The generation of the run.
pub const GENERATION: Generation = Generation(7);
/// The store's epoch the orders of these tests are drawn under.
pub const EPOCH: u64 = 1;
/// The activation order a host's row holds (the one the host's current update was given).
pub const ORDER: OrderId = OrderId::new(EPOCH, 41);
/// The id the fleet will give the next order.
pub const NEXT_ORDER: OrderId = OrderId::new(EPOCH, 42);

/// The order `n` of the tests' epoch.
pub const fn order(n: u64) -> OrderId {
    OrderId::new(EPOCH, n)
}
/// A member's boot counter, and the one a host's row recorded at the mark.
pub const BOOT: u64 = 10;

/// A load of nothing.
pub const IDLE: DaemonLoad = DaemonLoad {
    leases: 0,
    undelivered: 0,
    whole_machine_hold: false,
};

/// The farm time `secs` seconds after the epoch.
pub fn t(secs: u64) -> FarmTime {
    FarmTime::from_millis(secs * 1000)
}

/// The worker called `name`.
pub fn id(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// A load of `leases` leases.
pub const fn busy(leases: u32) -> DaemonLoad {
    DaemonLoad {
        leases,
        undelivered: 0,
        whole_machine_hold: false,
    }
}

/// What a helper reports about the order [`ORDER`], the one a host's row holds.
pub const fn report(status: HelperStatus) -> Option<HelperReport> {
    Some(HelperReport {
        order: ORDER,
        status,
    })
}

/// The activate order a move issues when it enters `Activating`: the fleet's next id.
pub const fn activate() -> Command {
    Command::Activate { order: NEXT_ORDER }
}

/// The confirm order for the activation a host's row holds.
pub const fn confirm() -> Command {
    Command::Confirm { order: ORDER }
}

fn host(name: &str, kind: HostKind, platform: Platform, member: Option<u64>) -> HostFacts {
    HostFacts {
        id: id(name),
        kind,
        platform,
        member: member.map(MemberId),
        release: Some(Release::new(OLD)),
        available: None,
        release_source: ReleaseSource::Unreported,
        connected: true,
        cordoned: Some(false),
        load: Some(IDLE),
        open_writes: Some(0),
        routed_writes: Some(0),
        helper: None,
        work: None,
        record: HostRecord {
            phase: HostPhase::Pending,
            since: t(0),
            generation: GENERATION,
            order: ORDER,
            boot_before: Some(BOOT),
        },
    }
}

/// A Linux daemon-only host at [`OLD`], connected, idle, not cordoned.
pub fn linux(name: &str) -> HostFacts {
    host(name, HostKind::DaemonOnly, Platform::Linux, None)
}

/// A storage-and-daemon host whose storage half is member `member`.
pub fn storage(name: &str, member: u64) -> HostFacts {
    host(
        name,
        HostKind::StorageAndDaemon,
        Platform::Linux,
        Some(member),
    )
}

/// A Mac of class `class`.
pub fn mac(name: &str, class: &str) -> HostFacts {
    let platform = Platform::Mac(PlatformClass::new(class));
    host(name, HostKind::DaemonOnly, platform, None)
}

/// Member `n`: `Active`, heard at [`NOW`], no debt, at boot [`BOOT`].
pub fn member(n: u64) -> MemberFacts {
    MemberFacts {
        id: MemberId(n),
        state: MemberState::Active,
        planned_down: false,
        heard_at: Some(t(NOW)),
        repair_debt: Some(0),
        down_since: None,
        boot: Some(BOOT),
    }
}

/// Members `1..=n`, all healthy.
pub fn members(n: u64) -> Vec<MemberFacts> {
    (1..=n).map(member).collect()
}

/// A run to [`NEW`] over `hosts` and `members`: no slot, an empty queue that needs no Mac
/// class, and a picker that began waiting at [`NOW`].
pub fn facts(hosts: Vec<HostFacts>, members: Vec<MemberFacts>) -> Facts {
    Facts {
        target: Release::new(NEW),
        source: Source::Fetch,
        generation: GENERATION,
        next_order: NEXT_ORDER,
        hosts,
        members,
        slot: Slot::default(),
        queued_classes: Some(BTreeSet::new()),
        wait_since: t(NOW),
    }
}

/// Gives `name` a place in the slot, of the kind the host in `facts` takes. A name that is
/// not a host of the run takes a place of `class_if_unknown`.
pub fn hold_as(facts: &mut Facts, name: &str, class_if_unknown: SlotClass) {
    let class = facts
        .host(&id(name))
        .map_or(class_if_unknown, HostFacts::slot_class);
    let holder = Holder {
        host: id(name),
        order: ORDER,
    };
    assert!(
        facts.slot.claim(class, holder, usize::MAX),
        "{name} could not take a place"
    );
}

/// [`hold_as`] for a host of the run.
pub fn hold(facts: &mut Facts, name: &str) {
    hold_as(facts, name, SlotClass::Host);
}

/// The host `name` of `facts`, to change.
pub fn host_mut<'a>(facts: &'a mut Facts, name: &str) -> &'a mut HostFacts {
    facts
        .hosts
        .iter_mut()
        .find(|h| h.id == id(name))
        .expect("the host is in the facts")
}

/// Makes `name` the host of an installer that reports packages: its package carries `commit`,
/// and its releases come from `release_source`. Without this a host carries no package and
/// reports no source, as an installer that predates the report does.
pub fn package(facts: &mut Facts, name: &str, commit: &str, release_source: ReleaseSource) {
    let host = host_mut(facts, name);
    host.available = Some(AvailableFact {
        commit: Release::new(commit),
        seq: 1,
    });
    host.release_source = release_source;
}

/// The defaults of the design.
pub fn policy() -> Policy {
    Policy::default()
}
