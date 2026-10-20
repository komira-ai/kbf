//! The start-up check that this host can reach the storage network: once the first
//! storage map is held, a TCP connect to each member's advertised address, all at once,
//! each bounded by [`REACH_WITHIN`].
//!
//! A daemon on a host with no address on the network the members advertise (a Mac with
//! only a route to the frontend) would otherwise start, take leases, and fail every blob
//! call after a dial timeout. The check turns that into a start that fails at once and
//! says what is missing.
//!
//! - No member answers: [`NoStorageReachable`], which names every address tried and why
//!   it failed. The caller ends the daemon with it.
//! - Some answer: the others are named in a warning and the daemon goes on; a blob has
//!   other owners, and a member that is down is the storage layer's business.
//! - The map holds no member to try (none but this process's own, or all `Down`): the
//!   check passes, since there is nothing it could say.
//!
//! The probe is a connect and no more: no TLS handshake, so it passes on a member that
//! would refuse this daemon's certificate, which the first call reports in its own
//! words. It runs on every daemon that keeps a map (not only Macs): a Linux node that
//! reaches no member is as unable to serve, and a farm whose members answer is
//! unchanged.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, join_all};
use kbf_map::MemberState;
use kbf_mapsync::MapHolder;
use tokio::net::TcpStream;

/// How long a connect to one member may take. Members are probed together, so the whole
/// check takes at most this.
pub const REACH_WITHIN: Duration = Duration::from_secs(3);

/// An address that did not answer, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreachable {
    /// The member's advertised address.
    pub addr: String,
    /// The connect error, or that none came in time.
    pub reason: String,
}

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.addr, self.reason)
    }
}

/// No storage member answered.
#[derive(Debug, PartialEq, Eq)]
pub struct NoStorageReachable {
    /// Every address tried.
    pub tried: Vec<Unreachable>,
}

impl fmt::Display for NoStorageReachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no storage member is reachable from this host: tried ")?;
        write!(f, "{}", Tried(&self.tried))?;
        write!(f, "; this host needs an address on the storage network")?;
        if let Some(network) = network_of(&self.tried) {
            write!(f, " ({network})")?;
        }
        f.write_str(
            ", and a route and firewall that let it reach the members' ports; check that \
             its interface on that network is up",
        )
    }
}

impl std::error::Error for NoStorageReachable {}

/// The network the tried addresses share, as `a.b.c.x`: the first three octets of the
/// addresses when all are IPv4 literals that agree; `None` for names, IPv6, or addresses
/// on different networks, where a guess would mislead.
fn network_of(tried: &[Unreachable]) -> Option<String> {
    let mut prefix = None;
    for one in tried {
        let SocketAddr::V4(addr) = one.addr.parse().ok()? else {
            return None;
        };
        let [a, b, c, _] = addr.ip().octets();
        if *prefix.get_or_insert([a, b, c]) != [a, b, c] {
            return None;
        }
    }
    prefix.map(|[a, b, c]| format!("{a}.{b}.{c}.x"))
}

/// One connect, given up on after `within`.
async fn attempt(
    connect: impl Future<Output = io::Result<()>>,
    within: Duration,
) -> Result<(), String> {
    match tokio::time::timeout(within, connect).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!("no answer within {within:?}")),
    }
}

/// How a connect to one address is made: the TCP connect in production, a stand-in in
/// the tests that need time or answers they control.
pub type Connect = dyn Fn(&str) -> BoxFuture<'static, io::Result<()>> + Send + Sync;

/// The real connect.
fn tcp(addr: &str) -> BoxFuture<'static, io::Result<()>> {
    let addr = addr.to_owned();
    Box::pin(async move { TcpStream::connect(addr).await.map(drop) })
}

/// How hard the check tries before it says no member is reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The bound on each connect.
    pub within: Duration,
    /// Rounds of probing, at least 1. A first map can name members whose listeners are
    /// not up yet (the whole farm starting together), so one round that finds nobody is
    /// not yet a refusal.
    pub attempts: u32,
    /// The wait between rounds.
    pub pause: Duration,
}

impl Plan {
    /// Three rounds of at most [`REACH_WITHIN`] each, 3 s apart: a refusal comes about 15 s
    /// after the map at the latest.
    pub const DEFAULT: Self = Self {
        within: REACH_WITHIN,
        attempts: 3,
        pause: Duration::from_secs(3),
    };
}

/// Connects to each of `addrs` at once; the ones that did not answer, in the order given.
pub async fn probe(addrs: &[String], within: Duration) -> Vec<Unreachable> {
    probe_with(addrs, within, &tcp).await
}

/// [`probe`] over `connect`.
pub async fn probe_with(addrs: &[String], within: Duration, connect: &Connect) -> Vec<Unreachable> {
    let tries = addrs.iter().map(|addr| async move {
        attempt(connect(addr), within)
            .await
            .err()
            .map(|reason| Unreachable {
                addr: addr.clone(),
                reason,
            })
    });
    join_all(tries).await.into_iter().flatten().collect()
}

/// [`check_with`] over real connects.
///
/// # Errors
/// As [`check_with`].
pub async fn check_first_map(
    maps: Arc<MapHolder>,
    local: Option<&str>,
    plan: Plan,
) -> Result<(), NoStorageReachable> {
    check_with(maps, local, plan, &tcp).await
}

/// Waits for the first map `maps` holds, probes its members except `local` (this
/// process's own, in-process) and the `Down` ones, and judges the answers as the module
/// documentation says. While no member answers it probes again after `plan.pause`, up to
/// `plan.attempts` rounds, logging each failed round; the refusal is the last round's.
///
/// # Errors
/// Members were probed and none answered in any round.
pub async fn check_with(
    maps: Arc<MapHolder>,
    local: Option<&str>,
    plan: Plan,
    connect: &Connect,
) -> Result<(), NoStorageReachable> {
    // Not `MapHolder::reach`, which would record a generation as wanted and make the
    // session ask the frontend for it.
    let mut changes = maps.changes();
    let _ = changes.wait_for(|generation| *generation > 0).await;
    let map = maps.current().expect("a generation above 0 is a held map");
    let addrs: Vec<String> = map
        .members()
        .iter()
        .filter(|m| m.state != MemberState::Down && Some(m.addr.as_str()) != local)
        .map(|m| m.addr.clone())
        .collect();
    if addrs.is_empty() {
        return Ok(());
    }
    let mut round = 1;
    let down = loop {
        let down = probe_with(&addrs, plan.within, connect).await;
        if down.len() < addrs.len() {
            break down;
        }
        if round >= plan.attempts {
            return Err(NoStorageReachable { tried: down });
        }
        tracing::warn!(
            round,
            of = plan.attempts,
            tried = %Tried(&down),
            "no storage member answers yet; probing again"
        );
        tokio::time::sleep(plan.pause).await;
        round += 1;
    };
    for one in &down {
        tracing::warn!(
            addr = %one.addr,
            reason = %one.reason,
            "a storage member is unreachable from this host; going on with the others"
        );
    }
    if down.is_empty() {
        tracing::info!(members = addrs.len(), "every storage member is reachable");
    }
    Ok(())
}

/// The addresses of a failed round, as the error lists them.
struct Tried<'a>(&'a [Unreachable]);

impl fmt::Display for Tried<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, one) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{one}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use futures::future::BoxFuture;

    use kbf_map::{Map, Member, MemberId, Rule};
    use tokio::net::TcpListener;

    use super::*;

    const LONG: Duration = Duration::from_secs(30);

    /// One round, as the tests with no stand-in connector want: nothing to wait for.
    const ONCE: Plan = Plan {
        within: LONG,
        attempts: 1,
        pause: Duration::ZERO,
    };

    fn map_of(addrs: &[&str], state: MemberState) -> Map {
        let members = addrs
            .iter()
            .zip(1u64..)
            .map(|(addr, id)| Member {
                id: MemberId(id),
                addr: (*addr).to_owned(),
                zone: String::new(),
                weight: 1,
                state,
                boot: 1,
            })
            .collect();
        Map::new(1, members, Rule::Rendezvous, 1, 0, Vec::new()).expect("a map")
    }

    /// A listening address, and the listener that keeps it answering.
    async fn alive() -> (String, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        (listener.local_addr().expect("addr").to_string(), listener)
    }

    /// An address nothing listens on: a low port (not an ephemeral one, which another
    /// test's listener could be given once released), `n` telling the addresses apart.
    fn closed(n: u16) -> String {
        format!("127.0.0.1:{n}")
    }

    fn held(map: Map) -> Arc<MapHolder> {
        let holder = Arc::new(MapHolder::new());
        assert!(holder.offer(map));
        holder
    }

    /// Catches: a check that fails a start when every member answers.
    #[tokio::test]
    async fn every_member_reachable_passes() {
        let (a, _la) = alive().await;
        let (b, _lb) = alive().await;
        let maps = held(map_of(&[&a, &b], MemberState::Active));
        assert_eq!(check_first_map(maps, None, ONCE).await, Ok(()));
    }

    /// Catches: a check that refuses a start with one member down (the farm is degraded,
    /// not unreachable), and a probe that reports a member that answered.
    #[tokio::test]
    async fn some_members_unreachable_go_on() {
        let (up, _l) = alive().await;
        let down = closed(1);
        let probed = probe(&[up.clone(), down.clone()], LONG).await;
        assert_eq!(probed.len(), 1);
        assert_eq!(probed[0].addr, down);
        let maps = held(map_of(&[&up, &down], MemberState::Active));
        assert_eq!(check_first_map(maps, None, ONCE).await, Ok(()));
    }

    /// Catches: a start that goes on with no member reachable; an error that does not
    /// name every address tried or the fix.
    #[tokio::test]
    async fn no_member_reachable_fails_naming_the_addresses() {
        let (a, b) = (closed(1), closed(2));
        let maps = held(map_of(&[&a, &b], MemberState::Active));
        let error = check_first_map(maps, None, ONCE).await.expect_err("fails");
        let said = error.to_string();
        assert_eq!(error.tried.len(), 2);
        assert!(said.contains(&a), "{said}");
        assert!(said.contains(&b), "{said}");
        assert!(said.contains("address on the storage network"), "{said}");
        assert!(said.contains("127.0.0.x"), "{said}");
    }

    /// Catches: a hint that names a network the addresses do not share, or that guesses
    /// at a name; one that is missing when they do share it.
    #[test]
    fn the_network_hint_comes_from_the_addresses() {
        let one = |addr: &str| Unreachable {
            addr: addr.to_owned(),
            reason: String::new(),
        };
        assert_eq!(
            network_of(&[one("198.51.100.4:8983"), one("198.51.100.9:8983")]).as_deref(),
            Some("198.51.100.x")
        );
        assert_eq!(
            network_of(&[one("198.51.100.4:1"), one("203.0.113.9:1")]),
            None
        );
        assert_eq!(network_of(&[one("198.51.100.4:1"), one("store-1:1")]), None);
        assert_eq!(network_of(&[one("[::1]:1")]), None);
        assert_eq!(network_of(&[]), None);
    }

    /// Catches: a hint written as `()` or `(None)` when the addresses are names.
    #[test]
    fn the_error_without_a_known_network_has_no_hint() {
        let error = NoStorageReachable {
            tried: vec![Unreachable {
                addr: "store-1:8983".to_owned(),
                reason: "refused".to_owned(),
            }],
        };
        let said = error.to_string();
        assert!(said.contains("store-1:8983 (refused)"), "{said}");
        assert!(said.contains("storage network, and a route"), "{said}");
    }

    /// Catches: this process's own member counted as a peer to reach (it is served
    /// in-process and its listener may not be up yet), and `Down` members counted.
    #[tokio::test]
    async fn local_and_down_members_are_not_probed() {
        let own = closed(1);
        let maps = held(map_of(&[&own], MemberState::Active));
        assert_eq!(check_first_map(maps, Some(&own), ONCE).await, Ok(()));
        let gone = closed(1);
        let maps = held(map_of(&[&gone], MemberState::Down));
        assert_eq!(check_first_map(maps, None, ONCE).await, Ok(()));
    }

    /// Catches: a check that decides before a map is held (it would find nothing to try
    /// and pass), or that polls for one.
    #[tokio::test]
    async fn waits_for_the_first_map() {
        let maps = Arc::new(MapHolder::new());
        let down = closed(1);
        let waiting = tokio::spawn(check_first_map(Arc::clone(&maps), None, ONCE));
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        assert!(maps.offer(map_of(&[&down], MemberState::Active)));
        assert!(waiting.await.expect("joined").is_err());
    }

    /// Catches: a connect that is not bounded: paused time, a connect that never
    /// completes, and the attempt must end at the bound.
    #[tokio::test(start_paused = true)]
    async fn a_connect_that_never_answers_ends_at_the_bound() {
        let started = tokio::time::Instant::now();
        let reason = attempt(std::future::pending(), REACH_WITHIN)
            .await
            .expect_err("gives up");
        assert_eq!(started.elapsed(), REACH_WITHIN);
        assert!(reason.contains("no answer within"), "{reason}");
    }

    /// A connector that never answers.
    fn silent(_: &str) -> BoxFuture<'static, io::Result<()>> {
        Box::pin(std::future::pending())
    }

    /// Catches: a probe that waits for the members one after the other (3 members cost 3
    /// bounds). Paused time, connects that never answer: the whole probe takes exactly
    /// one bound.
    #[tokio::test(start_paused = true)]
    async fn members_are_probed_together() {
        let addrs: Vec<String> = ["a:1", "b:1", "c:1"].map(str::to_owned).to_vec();
        let started = tokio::time::Instant::now();
        let down = probe_with(&addrs, REACH_WITHIN, &silent).await;
        assert_eq!(started.elapsed(), REACH_WITHIN);
        assert_eq!(down.len(), 3);
        assert!(down.iter().all(|d| d.reason.contains("no answer within")));
    }

    /// Smoke check on the real network: 192.0.2.1 is TEST-NET-1, nothing answers, and
    /// the connect either hangs to the bound or fails at once.
    #[tokio::test]
    async fn a_blackhole_address_is_given_up_on() {
        let within = Duration::from_millis(300);
        let started = Instant::now();
        let down = probe(&["192.0.2.1:8983".to_owned()], within).await;
        assert_eq!(down.len(), 1);
        assert!(started.elapsed() < within * 5, "{:?}", started.elapsed());
    }

    /// A connector that refuses the first `refusals` rounds of calls (`per_round` calls
    /// each) and answers after.
    fn flaky(
        refusals: usize,
        per_round: usize,
    ) -> impl Fn(&str) -> BoxFuture<'static, io::Result<()>> {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        move |_| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if n / per_round < refusals {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else {
                    Ok(())
                }
            })
        }
    }

    /// Catches: a refusal on the first round when the members' listeners come up a moment
    /// after the map (a farm starting together), and a retry that does not wait between
    /// rounds.
    #[tokio::test(start_paused = true)]
    async fn members_that_come_up_within_the_window_are_not_a_refusal() {
        let maps = held(map_of(&["a:1", "b:1"], MemberState::Active));
        let started = tokio::time::Instant::now();
        let connect = flaky(2, 2);
        let checked = check_with(maps, None, Plan::DEFAULT, &connect).await;
        assert_eq!(checked, Ok(()));
        // Refused twice, answered in the third round: two pauses.
        assert_eq!(started.elapsed(), Plan::DEFAULT.pause * 2);
    }

    /// Catches: a window that never ends or ends early: members down in every round are
    /// refused after exactly `attempts` rounds and the pauses between them.
    #[tokio::test(start_paused = true)]
    async fn members_down_throughout_the_window_are_refused_after_it() {
        let maps = held(map_of(&["a:1", "b:1"], MemberState::Active));
        let started = tokio::time::Instant::now();
        let connect = flaky(usize::MAX, 2);
        let error = check_with(maps, None, Plan::DEFAULT, &connect)
            .await
            .expect_err("never answers");
        assert_eq!(error.tried.len(), 2);
        assert_eq!(started.elapsed(), Plan::DEFAULT.pause * 2);
    }
}
