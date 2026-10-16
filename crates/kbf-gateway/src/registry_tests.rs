use super::*;

const TTL: Duration = Duration::from_secs(6);

/// A clock the test moves by hand.
#[derive(Clone)]
struct Manual {
    base: Instant,
    elapsed: Arc<Mutex<Duration>>,
}

impl Manual {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            elapsed: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    fn clock(&self) -> Clock {
        let this = self.clone();
        Clock::from_fn(move || this.base + *this.elapsed.lock().expect("clock"))
    }

    fn advance(&self, by: Duration) {
        *self.elapsed.lock().expect("clock") += by;
    }
}

fn claim(name: &str, port: u16, is_leader: bool, term: u64) -> Claim {
    Claim {
        name: name.to_owned(),
        address: SocketAddr::from(([192, 0, 2, 1], port)),
        is_leader,
        term,
    }
}

fn leader(name: &str, port: u16, term: u64) -> Leader {
    Leader {
        name: name.to_owned(),
        address: SocketAddr::from(([192, 0, 2, 1], port)),
        term,
    }
}

/// Catches: a follower's claim, or a leader at a lower term, taking the role from the
/// leader at the highest term; a registration order that changes the answer.
#[test]
fn the_highest_term_among_leader_claims_wins() {
    let manual = Manual::new();
    let registry = Registry::new(TTL, manual.clock());
    let seen = registry.subscribe();
    assert_eq!(*seen.borrow(), None, "nothing is registered");
    registry.register(claim("b", 2, true, 4));
    registry.register(claim("a", 1, true, 3));
    registry.register(claim("c", 3, false, 9));
    assert_eq!(*seen.borrow(), Some(leader("b", 2, 4)));
    registry.register(claim("d", 4, true, 5));
    assert_eq!(*seen.borrow(), Some(leader("d", 4, 5)));
    assert_eq!(registry.servers(), ["a", "b", "c", "d"]);
}

/// Catches: a tie at the top term decided by arrival order or by address instead of the
/// lowest name; the tied names not being reported (they are what the warning logs), or
/// logged again at every call while the tie stands.
#[test]
fn a_tie_goes_to_the_lowest_name() {
    let log = crate::testlog::captured();
    let claims = [
        claim("tie-zed", 1, true, 7),
        claim("tie-amy", 2, true, 7),
        claim("tie-old", 3, true, 6),
    ];
    let mut in_name_order: Vec<&Claim> = claims.iter().collect();
    in_name_order.sort_by(|a, b| a.name.cmp(&b.name));
    let (leader_found, tied) = elect(in_name_order.into_iter());
    assert_eq!(leader_found, Some(leader("tie-amy", 2, 7)));
    assert_eq!(tied, ["tie-amy", "tie-zed"]);

    let (alone, tied) = elect(claims[2..].iter());
    assert_eq!(alone, Some(leader("tie-old", 3, 6)));
    assert!(tied.is_empty(), "one claimant is no tie");

    let registry = Registry::new(TTL, Manual::new().clock());
    registry.register(claim("tie-zed", 1, true, 7));
    registry.register(claim("tie-amy", 2, true, 7));
    registry.register(claim("tie-amy", 2, true, 7));
    assert_eq!(
        *registry.subscribe().borrow(),
        Some(leader("tie-amy", 2, 7))
    );
    let ties = log.lines_with("tie-zed");
    let warned: Vec<&String> = ties
        .iter()
        .filter(|line| line.contains("several servers claim"))
        .collect();
    assert_eq!(warned.len(), 1, "{ties:?}");
    assert!(warned[0].contains("tie-amy"), "{warned:?}");
    assert!(
        format!("{registry:?}").contains("Clock"),
        "the clock is debuggable"
    );
}

/// Catches: an entry that outlives its TTL, one dropped at the TTL's last instant, a
/// renewal that does not extend the life, and an expiry that does not reach the
/// subscribers (the ADS streams) as a changed leader.
#[test]
fn an_entry_expires_after_the_ttl_and_renewal_extends_it() {
    let manual = Manual::new();
    let registry = Registry::new(TTL, manual.clock());
    let mut seen = registry.subscribe();
    registry.register(claim("a", 1, true, 1));
    registry.register(claim("b", 2, true, 1));
    assert_eq!(*seen.borrow_and_update(), Some(leader("a", 1, 1)));

    manual.advance(Duration::from_secs(4));
    registry.register(claim("b", 2, true, 1));
    manual.advance(TTL - Duration::from_secs(4) - Duration::from_millis(1));
    registry.sweep();
    assert_eq!(registry.servers(), ["a", "b"], "just inside the TTL");
    assert!(!seen.has_changed().expect("sender"), "nothing changed");

    manual.advance(Duration::from_millis(1));
    registry.sweep();
    assert_eq!(registry.servers(), ["b"], "a's entry ran out; b renewed");
    assert!(seen.has_changed().expect("sender"));
    assert_eq!(*seen.borrow_and_update(), Some(leader("b", 2, 1)));

    manual.advance(TTL);
    registry.sweep();
    assert!(registry.servers().is_empty());
    assert_eq!(*seen.borrow_and_update(), None, "nobody is left to lead");
    assert_eq!(registry.ttl(), TTL);
}

/// Catches: a heartbeat that repeats the same claim waking every subscriber (each wake
/// is an xDS response to Envoy), and a changed address of the same server not being
/// announced.
#[test]
fn only_a_changed_leader_is_announced() {
    let registry = Registry::new(TTL, Manual::new().clock());
    let mut seen = registry.subscribe();
    registry.register(claim("a", 1, true, 1));
    assert!(seen.has_changed().expect("sender"));
    seen.borrow_and_update();
    registry.register(claim("a", 1, true, 1));
    assert!(!seen.has_changed().expect("sender"), "the same claim again");
    registry.register(claim("a", 9, true, 1));
    assert!(seen.has_changed().expect("sender"), "a new address");
    seen.borrow_and_update();
    registry.register(claim("a", 9, false, 1));
    assert!(seen.has_changed().expect("sender"), "it gave the role up");
    assert_eq!(*seen.borrow(), None);
}
