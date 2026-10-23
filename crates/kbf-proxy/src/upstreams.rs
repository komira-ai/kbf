//! The members of a pool-mode proxy: the frontends it balances over, whether each takes new
//! streams, whether its last probe passed, and how many streams it has open.
//!
//! One lock guards the state of every member, so a pick and its count, and a drain and the
//! picks after it, are never apart: once [`Upstreams::set_draining`] returns, no stream is
//! placed on that member again, and the streams already there are the ones its count shows.
//! The lock is held for a few comparisons and never across an await.
//!
//! A pick takes the member with the fewest open streams among those that are up and ready;
//! members with as many are taken in turn, so an idle pool spreads its first streams over all
//! of them. A member that is up but not ready (not yet probed, or its probe failed) is taken
//! only when no member is ready: a pool whose probes all fail still serves, and the member that
//! really is down fails the stream, which goes to another when the stream may be tried again
//! (a whole request: `crate::replay`), and is `UNAVAILABLE` when it may not (an open worker
//! stream, say). A draining member is never taken.

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;

use crate::config::Frontend;
use crate::route::Target;

/// Streams open on a member, by the listener they go to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct OpenStreams {
    pub(crate) reapi: u64,
    pub(crate) worker: u64,
    pub(crate) operator: u64,
    pub(crate) enroll: u64,
}

impl OpenStreams {
    const fn total(self) -> u64 {
        self.reapi + self.worker + self.operator + self.enroll
    }

    fn counter(&mut self, target: Target) -> &mut u64 {
        match target {
            Target::Reapi => &mut self.reapi,
            Target::Worker => &mut self.worker,
            Target::Operator => &mut self.operator,
            Target::Enroll => &mut self.enroll,
        }
    }

    /// The counts with the name of the listener each is of.
    fn by_target(self) -> [(&'static str, u64); 4] {
        [
            ("reapi", self.reapi),
            ("worker", self.worker),
            ("operator", self.operator),
            ("enroll", self.enroll),
        ]
    }
}

/// Whether a member takes new streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Up,
    Draining,
}

/// One member as the admin API shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MemberView {
    pub(crate) name: String,
    pub(crate) state: State,
    pub(crate) ready: bool,
    pub(crate) open_streams: OpenStreams,
}

/// What changes about a member while the proxy runs.
#[derive(Clone, Copy, Debug, Default)]
struct Live {
    draining: bool,
    ready: bool,
    open: OpenStreams,
    /// Streams placed on it, ever.
    picked: u64,
    /// Streams that failed on it before an answer and were placed elsewhere or refused.
    failed: u64,
}

impl Live {
    /// Whether a pick may take it: not draining, and ready as `ready` says.
    const fn takes(&self, ready: bool) -> bool {
        !self.draining & (self.ready == ready)
    }

    const fn state(&self) -> State {
        if self.draining {
            State::Draining
        } else {
            State::Up
        }
    }
}

struct Shared {
    members: Vec<Live>,
    /// Where the next pick starts looking, so equal members are taken in turn.
    cursor: usize,
}

impl Shared {
    /// The member with the fewest open streams that a pick may take: the first of them from
    /// `start` on, round the pool, among those `takes(ready)` and not in `skip`.
    fn least(&self, start: usize, skip: &[usize], ready: bool) -> Option<usize> {
        let n = self.members.len();
        let order = (0..n).map(|k| (start + k) % n);
        let takes = |index: &usize| !skip.contains(index) & self.members[*index].takes(ready);
        let load = |index: &usize| self.members[*index].open.total();
        order.filter(takes).min_by_key(load)
    }
}

/// The pool's members, in the order of the `[[frontend]]` tables.
pub(crate) struct Upstreams {
    frontends: Vec<Frontend>,
    shared: Mutex<Shared>,
}

impl Upstreams {
    /// A pool over `frontends` (at least one: the config refuses none). Every member is up and
    /// not yet ready.
    pub(crate) fn new(frontends: &[Frontend]) -> Arc<Self> {
        Arc::new(Self {
            frontends: frontends.to_vec(),
            shared: Mutex::new(Shared {
                members: vec![Live::default(); frontends.len()],
                cursor: 0,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many members there are.
    pub(crate) fn len(&self) -> usize {
        self.frontends.len()
    }

    /// The loopback address of member `index`'s base port: its listeners are at the offsets of
    /// the targets from it.
    pub(crate) fn base(&self, index: usize) -> SocketAddr {
        let port = self.frontends[index].base_port;
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    /// Places a stream for `target` on a member: see the module doc for which. `skip` are the
    /// members that failed this stream already. `None`: no member may take it (all are
    /// draining or skipped).
    pub(crate) fn pick(self: &Arc<Self>, target: Target, skip: &[usize]) -> Option<MemberPermit> {
        let mut shared = self.lock();
        let start = shared.cursor % shared.members.len();
        shared.cursor = shared.cursor.wrapping_add(1);
        let ready = shared.least(start, skip, true);
        let index = ready.or_else(|| shared.least(start, skip, false))?;
        let live = &mut shared.members[index];
        *live.open.counter(target) += 1;
        live.picked += 1;
        Some(MemberPermit {
            upstreams: Arc::clone(self),
            index,
            target,
        })
    }

    /// Makes member `name` take no new stream (`draining`) or take them again; the streams it
    /// has go on. Doing what is done already changes nothing. `false`: there is no such member.
    pub(crate) fn set_draining(&self, name: &str, draining: bool) -> bool {
        let Some(index) = self.frontends.iter().position(|f| f.name == name) else {
            return false;
        };
        self.lock().members[index].draining = draining;
        true
    }

    /// Records the answer of member `index`'s last probe.
    pub(crate) fn set_ready(&self, index: usize, ready: bool) {
        self.lock().members[index].ready = ready;
    }

    /// Records that a stream failed on member `index` before any answer. A failure to connect
    /// (`connect`) also makes the member not ready, until a probe passes again.
    pub(crate) fn failed(&self, index: usize, connect: bool) {
        let mut shared = self.lock();
        let live = &mut shared.members[index];
        live.failed += 1;
        live.ready &= !connect;
    }

    /// Streams open on all members together, by every listener.
    pub(crate) fn open_total(&self) -> u64 {
        let shared = self.lock();
        shared.members.iter().map(|live| live.open.total()).sum()
    }

    /// Whether some member is up and ready: what `/readyz` says.
    pub(crate) fn is_ready(&self) -> bool {
        let shared = self.lock();
        shared.members.iter().any(|live| live.takes(true))
    }

    /// Every member as the admin API shows it.
    pub(crate) fn snapshot(&self) -> Vec<MemberView> {
        let shared = self.lock();
        let members = self.frontends.iter().zip(&shared.members);
        members.map(|(f, live)| view(f, live)).collect()
    }

    /// The per-member series of `GET /metrics`, in Prometheus text.
    pub(crate) fn render(&self) -> String {
        let shared = self.lock();
        let members = self.frontends.iter().zip(&shared.members);
        let mut out = String::new();
        for series in Series::ALL {
            let name = series.name();
            let kind = if name.ends_with("_total") {
                "counter"
            } else {
                "gauge"
            };
            let _ = writeln!(out, "# HELP {name} {}", series.help());
            let _ = writeln!(out, "# TYPE {name} {kind}");
            for (frontend, live) in members.clone() {
                let who = &frontend.name;
                for (target, value) in series.samples(live) {
                    let _ = match target {
                        "" => writeln!(out, "{name}{{member=\"{who}\"}} {value}"),
                        t => writeln!(out, "{name}{{member=\"{who}\",target=\"{t}\"}} {value}"),
                    };
                }
            }
        }
        out
    }
}

fn view(frontend: &Frontend, live: &Live) -> MemberView {
    MemberView {
        name: frontend.name.clone(),
        state: live.state(),
        ready: live.ready,
        open_streams: live.open,
    }
}

/// The series `GET /metrics` shows for each member.
#[derive(Clone, Copy)]
enum Series {
    Up,
    Ready,
    Open,
    Picked,
    Failed,
}

impl Series {
    const ALL: [Self; 5] = [
        Self::Up,
        Self::Ready,
        Self::Open,
        Self::Picked,
        Self::Failed,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Up => "kbf_proxy_member_up",
            Self::Ready => "kbf_proxy_member_ready",
            Self::Open => "kbf_proxy_member_open_streams",
            Self::Picked => "kbf_proxy_member_picked_total",
            Self::Failed => "kbf_proxy_member_failed_total",
        }
    }

    const fn help(self) -> &'static str {
        match self {
            Self::Up => "1 while the member takes new streams, 0 while it drains.",
            Self::Ready => "1 while the member's last probe passed.",
            Self::Open => "Streams open on the member, by listener.",
            Self::Picked => "Streams placed on the member.",
            Self::Failed => "Streams that failed on the member before an answer.",
        }
    }

    /// The samples of `live`: the listener each is of (empty for a series that has none) and
    /// its value.
    fn samples(self, live: &Live) -> Vec<(&'static str, u64)> {
        match self {
            Self::Up => vec![("", u64::from(!live.draining))],
            Self::Ready => vec![("", u64::from(live.ready))],
            Self::Open => live.open.by_target().to_vec(),
            Self::Picked => vec![("", live.picked)],
            Self::Failed => vec![("", live.failed)],
        }
    }
}

/// One stream's place on a member, for as long as the stream lives (until both its upstream
/// halves are done): it counts against the member's open streams and gives the count back when
/// it drops.
pub(crate) struct MemberPermit {
    upstreams: Arc<Upstreams>,
    index: usize,
    target: Target,
}

impl MemberPermit {
    /// The member's index in the pool.
    pub(crate) const fn index(&self) -> usize {
        self.index
    }

    /// The loopback address the member's listeners start at.
    pub(crate) fn base(&self) -> SocketAddr {
        self.upstreams.base(self.index)
    }
}

impl Drop for MemberPermit {
    fn drop(&mut self) {
        let mut shared = self.upstreams.lock();
        let count = shared.members[self.index].open.counter(self.target);
        *count = count.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frontends(n: u16) -> Vec<Frontend> {
        (0..n)
            .map(|i| Frontend {
                name: format!("m{i}"),
                base_port: 18_000 + 10 * i,
            })
            .collect()
    }

    /// A pool of `n` members that are all ready.
    fn ready_pool(n: u16) -> Arc<Upstreams> {
        let pool = Upstreams::new(&frontends(n));
        for index in 0..pool.len() {
            pool.set_ready(index, true);
        }
        pool
    }

    /// The members the next `n` picks take, each pick kept open.
    fn picks(pool: &Arc<Upstreams>, n: usize) -> (Vec<usize>, Vec<MemberPermit>) {
        let held: Vec<MemberPermit> = (0..n)
            .map(|_| pool.pick(Target::Reapi, &[]).expect("a member"))
            .collect();
        (held.iter().map(MemberPermit::index).collect(), held)
    }

    /// Asserts that a pick takes the member with the fewest open streams, and that members
    /// with as many are taken in turn: three idle members take three streams in order, a
    /// fourth goes to the first again, and a member whose stream ended is the least busy.
    #[test]
    fn a_pick_takes_the_member_with_the_fewest_open_streams() {
        let pool = ready_pool(3);
        let (order, mut held) = picks(&pool, 4);
        assert_eq!(order, [0, 1, 2, 0]);
        drop(held.remove(1));
        let again = pool.pick(Target::Reapi, &[]).expect("a member");
        assert_eq!(again.index(), 1, "its stream ended, so it has the fewest");
    }

    /// Asserts that idle members share short streams evenly: a stream that ends before the
    /// next starts does not send every stream to the first member.
    #[test]
    fn idle_members_take_streams_in_turn() {
        let pool = ready_pool(3);
        let mut order = Vec::new();
        for _ in 0..6 {
            order.push(pool.pick(Target::Reapi, &[]).expect("a member").index());
        }
        assert_eq!(order, [0, 1, 2, 0, 1, 2]);
    }

    /// Asserts that a ready member is taken over one that is not however busy it is, and that
    /// with no member ready the least busy one that is up is taken.
    #[test]
    fn a_ready_member_is_taken_before_one_that_is_not() {
        let pool = Upstreams::new(&frontends(2));
        pool.set_ready(1, true);
        let busy: Vec<MemberPermit> = (0..3)
            .map(|_| pool.pick(Target::Reapi, &[]).expect("a member"))
            .collect();
        assert!(busy.iter().all(|p| p.index() == 1), "only 1 is ready");
        pool.set_ready(1, false);
        let fallback = pool.pick(Target::Reapi, &[]).expect("a member");
        assert_eq!(fallback.index(), 0, "none ready: least busy");
    }

    /// Asserts that a draining member is never taken, however idle, that no member at all is
    /// `None`, and that undraining brings it back.
    #[test]
    fn a_draining_member_is_never_taken() {
        let pool = ready_pool(2);
        assert!(pool.set_draining("m0", true));
        let (order, held) = picks(&pool, 3);
        assert_eq!(order, [1, 1, 1]);
        assert!(pool.set_draining("m1", true));
        assert!(pool.pick(Target::Reapi, &[]).is_none());
        assert!(pool.set_draining("m0", false));
        let back = pool.pick(Target::Reapi, &[]).expect("m0 is back");
        assert_eq!(back.index(), 0);
        drop(held);
    }

    /// Asserts that a member in `skip` (one that failed this stream) is not taken, and that
    /// skipping every member leaves nothing to take.
    #[test]
    fn a_skipped_member_is_not_taken() {
        let pool = ready_pool(2);
        assert_eq!(pool.pick(Target::Reapi, &[0]).expect("one").index(), 1);
        assert_eq!(pool.pick(Target::Reapi, &[1]).expect("one").index(), 0);
        assert!(pool.pick(Target::Reapi, &[0, 1]).is_none());
    }

    /// Asserts that draining is idempotent, that an unknown name is reported, and that the
    /// state shows in the snapshot.
    #[test]
    fn draining_twice_is_draining_once_and_an_unknown_name_is_reported() {
        let pool = ready_pool(2);
        assert!(pool.set_draining("m1", true));
        assert!(pool.set_draining("m1", true));
        let states: Vec<State> = pool.snapshot().iter().map(|m| m.state).collect();
        assert_eq!(states, [State::Up, State::Draining]);
        assert!(!pool.set_draining("nobody", true));
        assert!(!pool.set_draining("nobody", false));
        assert!(pool.set_draining("m1", false));
        assert!(pool.set_draining("m1", false));
        let states: Vec<State> = pool.snapshot().iter().map(|m| m.state).collect();
        assert_eq!(states, [State::Up, State::Up]);
    }

    /// Asserts that a permit counts its stream against the listener it goes to, and gives the
    /// count back when it drops, and that the member's base address is loopback at its port.
    #[test]
    fn a_permit_counts_its_listener_and_gives_it_back() {
        let pool = ready_pool(2);
        let reapi = pool.pick(Target::Reapi, &[]).expect("a member");
        let worker = pool.pick(Target::Worker, &[0]).expect("a member");
        let operator = pool.pick(Target::Operator, &[0]).expect("a member");
        let enroll = pool.pick(Target::Enroll, &[0]).expect("a member");
        let open = pool.snapshot()[1].open_streams;
        let seen = (open.reapi, open.worker, open.operator, open.enroll);
        assert_eq!(seen, (0, 1, 1, 1));
        assert_eq!(pool.snapshot()[0].open_streams.reapi, 1);
        assert_eq!(pool.open_total(), 4);
        assert_eq!(reapi.base(), "127.0.0.1:18000".parse().unwrap());
        assert_eq!(worker.base(), "127.0.0.1:18010".parse().unwrap());
        drop((reapi, worker, operator, enroll));
        let idle = OpenStreams::default();
        assert!(pool.snapshot().iter().all(|m| m.open_streams == idle));
        assert_eq!(pool.open_total(), 0);
    }

    /// Asserts that `/readyz` is true exactly when some member is up and ready.
    #[test]
    fn the_pool_is_ready_when_some_member_is_up_and_ready() {
        let pool = Upstreams::new(&frontends(2));
        assert!(!pool.is_ready(), "nothing probed yet");
        pool.set_ready(0, true);
        assert!(pool.is_ready());
        assert!(pool.set_draining("m0", true));
        assert!(!pool.is_ready(), "the only ready member drains");
        pool.set_ready(1, true);
        assert!(pool.is_ready());
        pool.set_ready(1, false);
        assert!(!pool.is_ready());
    }

    /// Asserts that a stream's failure is counted, and that only a failure to connect makes
    /// the member not ready.
    #[test]
    fn a_failure_to_connect_makes_a_member_not_ready() {
        let pool = ready_pool(1);
        pool.failed(0, false);
        assert!(pool.snapshot()[0].ready, "a refusal leaves it ready");
        pool.failed(0, true);
        assert!(!pool.snapshot()[0].ready);
        let line = "kbf_proxy_member_failed_total{member=\"m0\"} 2\n";
        assert!(pool.render().contains(line));
    }

    /// Asserts the admin API's shape for a member: its name, state, readiness and the streams
    /// open by listener.
    #[test]
    fn the_snapshot_is_the_admin_apis_json() {
        let pool = Upstreams::new(&frontends(1));
        let held = pool.pick(Target::Operator, &[]).expect("a member");
        let json = serde_json::to_string(&pool.snapshot()).expect("json");
        let expected = concat!(
            r#"[{"name":"m0","state":"up","ready":false,"#,
            r#""open_streams":{"reapi":0,"worker":0,"operator":1,"enroll":0}}]"#
        );
        assert_eq!(json, expected);
        drop(held);
    }

    /// Asserts that the metrics have a family for each series, with its type, and one sample
    /// per member (per listener for the open streams).
    #[test]
    fn the_metrics_show_each_member() {
        let pool = ready_pool(2);
        assert!(pool.set_draining("m1", true));
        let held = pool.pick(Target::Worker, &[]).expect("a member");
        let text = pool.render();
        for line in [
            "# TYPE kbf_proxy_member_up gauge\n",
            "# TYPE kbf_proxy_member_ready gauge\n",
            "# TYPE kbf_proxy_member_open_streams gauge\n",
            "# TYPE kbf_proxy_member_picked_total counter\n",
            "# TYPE kbf_proxy_member_failed_total counter\n",
            "kbf_proxy_member_up{member=\"m0\"} 1\n",
            "kbf_proxy_member_up{member=\"m1\"} 0\n",
            "kbf_proxy_member_ready{member=\"m0\"} 1\n",
            "kbf_proxy_member_open_streams{member=\"m0\",target=\"worker\"} 1\n",
            "kbf_proxy_member_open_streams{member=\"m0\",target=\"reapi\"} 0\n",
            "kbf_proxy_member_open_streams{member=\"m1\",target=\"worker\"} 0\n",
            "kbf_proxy_member_picked_total{member=\"m0\"} 1\n",
            "kbf_proxy_member_failed_total{member=\"m1\"} 0\n",
        ] {
            assert!(text.contains(line), "{line:?} is not in:\n{text}");
        }
        drop(held);
    }
}
