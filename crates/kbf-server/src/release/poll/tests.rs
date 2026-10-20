//! The poller over a fake source and an in-memory state store.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Fenced, ManualClock};
use serde_json::{Value, json};

use super::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const RUNNING: &str = "cccccccccccc";
const NOW: FarmTime = FarmTime::from_millis(5_000);

/// A source that answers from a script and remembers the `ETag`s it was asked with.
#[derive(Default)]
struct Script {
    answers: Mutex<VecDeque<Result<Fetched, FetchError>>>,
    asked: Mutex<Vec<Option<String>>>,
    calls: AtomicUsize,
    /// Run in the middle of a request (after it was asked, before it answers).
    during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Script {
    fn say(&self, answer: Result<Fetched, FetchError>) {
        self.answers.lock().unwrap().push_back(answer);
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn asked(&self) -> Vec<Option<String>> {
        self.asked.lock().unwrap().clone()
    }
}

impl Source for Arc<Script> {
    async fn fetch(&self, etag: Option<&str>) -> Result<Fetched, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.asked.lock().unwrap().push(etag.map(str::to_owned));
        if let Some(during) = self.during.lock().unwrap().take() {
            during();
        }
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(FetchError::Status(500)))
    }
}

fn release(tag: &str, commit: &str, published: &str) -> Value {
    let tar = format!("kbf-{commit}-linux-x86_64.tar.gz");
    json!({
        "tag_name": tag,
        "draft": false,
        "prerelease": false,
        "published_at": published,
        "body": format!("built and attested for {commit} on main (https://x)"),
        "assets": [
            {"name": "SHA256SUMS"},
            {"name": tar},
            {"name": format!("{tar}.sigstore.json")},
        ],
    })
}

fn feed(etag: &str, entries: &[Value]) -> Result<Fetched, FetchError> {
    Ok(Fetched::Feed {
        etag: Some(etag.to_owned()),
        body: serde_json::to_vec(entries).unwrap(),
    })
}

fn config() -> Config {
    Config {
        every: Duration::from_secs(300),
        repo: "komira-ai/kbf".to_owned(),
        refs: vec!["main".to_owned()],
        api_url: "http://unused".to_owned(),
        timeout: Duration::from_secs(5),
        running: RUNNING.to_owned(),
    }
}

struct Rig {
    script: Arc<Script>,
    store: Arc<dyn Store>,
    rows: Rows,
    stats: Arc<Stats>,
    poller: Poller<Arc<Script>>,
}

fn rig() -> Rig {
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    rig_over(store)
}

fn rig_over(store: Arc<dyn Store>) -> Rig {
    let script = Arc::new(Script::default());
    let rows = Rows::new(Arc::clone(&store));
    let stats = Arc::new(Stats::default());
    let poller = Poller::new(
        Arc::clone(&script),
        rows.clone(),
        config(),
        Arc::clone(&stats),
    );
    Rig {
        script,
        store,
        rows,
        stats,
        poller,
    }
}

fn keys(store: &Arc<dyn Store>) -> Vec<String> {
    store
        .scan_prefix("")
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

// Catches: a new release the feed lists that is never recorded, or recorded with the wrong
// commit, tag or time; and the cycle forgetting to count it.
#[tokio::test]
async fn a_new_v_release_becomes_a_proposal() {
    let mut r = rig();
    r.script
        .say(feed("e1", &[release("v1", A, "2026-10-20T01:02:03Z")]));
    let got = r.poller.cycle(NOW, || true).await;
    let want = Proposal {
        commit: A.to_owned(),
        tag: "v1".to_owned(),
        published_at_secs: 1_792_458_123,
        proposed_at_ms: 5_000,
    };
    assert_eq!(got, Cycle::Proposed(want.clone()));
    assert_eq!(r.rows.proposals().unwrap(), vec![want]);
    assert_eq!(
        (r.stats.polls(), r.stats.proposals(), r.stats.errors()),
        (1, 1, 0)
    );
}

// Catches: only the newest of several releases being proposed (the older ones are history,
// not offers), and the order of the feed deciding instead of the publication time.
#[tokio::test]
async fn the_newest_of_the_feed_is_the_proposal() {
    let mut r = rig();
    r.script.say(feed(
        "e1",
        &[
            release("v1", A, "2026-10-19T00:00:00Z"),
            release("v2", B, "2026-10-20T00:00:00Z"),
        ],
    ));
    r.poller.cycle(NOW, || true).await;
    let held = r.rows.proposals().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].commit, B);
}

// Catches: the ETag not being sent back, a 304 doing work (a write, a count other than
// not-modified), and the stored tag being lost by a 304.
#[tokio::test]
async fn an_unchanged_feed_costs_nothing() {
    let mut r = rig();
    r.script
        .say(feed("e1", &[release("v1", A, "2026-10-20T01:02:03Z")]));
    r.poller.cycle(NOW, || true).await;
    let txns = r.store.commit_stats().txns;
    r.script.say(Ok(Fetched::NotModified));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::NotModified);
    r.script.say(Ok(Fetched::NotModified));
    r.poller.cycle(NOW, || true).await;
    assert_eq!(
        r.script.asked(),
        [None, Some("e1".to_owned()), Some("e1".to_owned())]
    );
    assert_eq!(r.store.commit_stats().txns, txns, "a 304 wrote");
    assert_eq!((r.stats.polls(), r.stats.not_modified()), (3, 2));
}

// Catches: the same release proposed twice (a second row, a second count), an older release
// proposed after a newer one, and the release that is desired, or running, proposed.
#[tokio::test]
async fn an_older_or_same_or_current_release_is_not_proposed() {
    let mut r = rig();
    r.script
        .say(feed("e1", &[release("v2", B, "2026-10-20T00:00:00Z")]));
    r.poller.cycle(NOW, || true).await;
    // The same release again, under a new tag (the feed changed, the commit did not).
    r.script
        .say(feed("e2", &[release("v2b", B, "2026-10-20T00:00:00Z")]));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Nothing);
    // An older one.
    r.script
        .say(feed("e3", &[release("v1", A, "2026-10-19T00:00:00Z")]));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Nothing);
    // Newer, but the build that is running.
    let running = format!("{RUNNING}{}", "c".repeat(28));
    r.script.say(feed(
        "e4",
        &[release("v3", &running, "2026-10-21T00:00:00Z")],
    ));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Nothing);
    // Newer, but already the desired release.
    let desired = "d".repeat(40);
    r.rows
        .offer(&crate::update::Release::new(desired.clone()), NOW)
        .unwrap();
    r.script.say(feed(
        "e5",
        &[release("v4", &desired, "2026-10-22T00:00:00Z")],
    ));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Nothing);
    assert_eq!(r.rows.proposals().unwrap().len(), 1);
    assert_eq!(r.stats.proposals(), 1);
}

// Catches: a tag that is not v*, a draft, a pre-release or a release without its
// attestation bundle becoming a proposal; and the reason not being counted.
#[tokio::test]
async fn entries_that_are_not_candidates_are_ignored_and_counted() {
    let mut r = rig();
    let mut draft = release("v1", A, "2026-10-20T00:00:00Z");
    draft["draft"] = json!(true);
    let mut bare = release("v2", B, "2026-10-20T00:00:00Z");
    bare["assets"] = json!([{"name": "SHA256SUMS"}]);
    r.script.say(feed(
        "e1",
        &[
            release("nightly-1", A, "2026-10-20T00:00:00Z"),
            draft,
            bare,
            json!(17),
        ],
    ));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Nothing);
    assert!(r.rows.proposals().unwrap().is_empty());
    let got: Vec<(Ignore, u64)> = Ignore::ALL
        .iter()
        .map(|i| (*i, r.stats.ignored(*i)))
        .collect();
    let counted: Vec<(&str, u64)> = got
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(i, n)| (i.label(), *n))
        .collect();
    assert_eq!(
        counted,
        [
            ("malformed", 1),
            ("draft", 1),
            ("not_version_tag", 1),
            ("unattested", 1)
        ]
    );
    assert_eq!(r.stats.proposals(), 0);
}

// Catches: a failing source crashing the poller, the wait not growing (a hammered API), not
// being capped, a rate limit's own request being ignored, and the wait not resetting after
// a success. Also that a failure keeps the old ETag, so the next poll is conditional on the
// feed that was last read.
#[tokio::test]
async fn a_failing_source_backs_off_and_recovers() {
    let mut r = rig();
    assert_eq!(r.poller.wait(), Duration::from_secs(300));
    let mut waits = Vec::new();
    for _ in 0..8 {
        r.script.say(Err(FetchError::Transport("down".to_owned())));
        assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Failed);
        waits.push(r.poller.wait().as_secs());
    }
    assert_eq!(waits, [600, 1200, 2400, 3600, 3600, 3600, 3600, 3600]);
    assert_eq!(r.stats.errors(), 8);
    // A rate limit that asks for longer than the backoff is obeyed, to a bound.
    r.script
        .say(Err(FetchError::RateLimited(Some(Duration::from_secs(
            5000,
        )))));
    r.poller.cycle(NOW, || true).await;
    assert_eq!(r.poller.wait(), Duration::from_secs(5000));
    r.script
        .say(Err(FetchError::RateLimited(Some(Duration::from_secs(
            900_000,
        )))));
    r.poller.cycle(NOW, || true).await;
    assert_eq!(r.poller.wait(), MAX_RETRY_AFTER);
    // A success clears both.
    r.script.say(feed("e1", &[]));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Nothing);
    assert_eq!(r.poller.wait(), Duration::from_secs(300));
    assert_eq!(r.stats.errors(), 10);
}

// Catches: a configured interval above the cap being cut to it.
#[tokio::test]
async fn a_long_interval_is_kept_and_the_backoff_never_shortens_it() {
    let mut r = rig();
    r.poller.config.every = Duration::from_secs(7200);
    r.script.say(Err(FetchError::Timeout));
    r.poller.cycle(NOW, || true).await;
    assert_eq!(r.poller.wait(), Duration::from_secs(7200));
}

// Catches: a body that is not a feed being read as an empty one (and its ETag kept, so the
// feed is never read again), and a rows failure being swallowed.
#[tokio::test]
async fn a_malformed_feed_is_an_error_and_is_read_again() {
    let mut r = rig();
    r.script.say(Ok(Fetched::Feed {
        etag: Some("bad".to_owned()),
        body: b"{\"message\": \"nope\"}".to_vec(),
    }));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Failed);
    r.script.say(feed("e1", &[]));
    r.poller.cycle(NOW, || true).await;
    assert_eq!(
        r.script.asked(),
        [None, None],
        "the ETag of an unread feed was kept"
    );
    assert_eq!(r.stats.errors(), 1);
}

// Catches: a poll that was out when the farm was handed over still writing; the feed being
// marked read (its ETag kept) so the new active child would never see it.
#[tokio::test]
async fn a_poll_under_way_when_the_farm_is_handed_over_writes_nothing() {
    let mut r = rig();
    let handed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flip = Arc::clone(&handed);
    *r.script.during.lock().unwrap() = Some(Box::new(move || flip.store(true, Ordering::SeqCst)));
    r.script
        .say(feed("e1", &[release("v1", A, "2026-10-20T01:02:03Z")]));
    let active = || !handed.load(Ordering::SeqCst);
    assert_eq!(r.poller.cycle(NOW, active).await, Cycle::Dropped);
    assert!(keys(&r.store).is_empty(), "wrote {:?}", keys(&r.store));
    assert_eq!(r.stats.proposals(), 0);
    r.script.say(Ok(Fetched::NotModified));
    r.poller.cycle(NOW, || true).await;
    assert_eq!(r.script.asked()[1], None);
}

// Catches: a store handle a newer writer superseded still being written through (the fence
// is the store's, but the poller must see its refusal and stop rather than retry forever).
#[tokio::test]
async fn a_superseded_store_handle_ends_the_poller_and_writes_nothing() {
    let whole: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let old: Arc<dyn Store> = Arc::new(Fenced::take(Arc::clone(&whole), None).unwrap());
    let mut r = rig_over(old);
    let _newer = Fenced::take(Arc::clone(&whole), None).unwrap();
    r.script
        .say(feed("e1", &[release("v1", A, "2026-10-20T01:02:03Z")]));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Superseded);
    assert_eq!(
        keys(&whole)
            .iter()
            .filter(|k| k.starts_with("update/"))
            .count(),
        0
    );
    assert_eq!(r.stats.proposals(), 0);
}

// Catches: the poller writing anything but its own rows: it proposes with a run desired, a
// policy written and a host mid-run, and every one of those rows is byte for byte what it
// was, with no `update/desired` created when there was none.
#[tokio::test]
async fn the_poller_never_writes_the_desired_release_or_the_policy() {
    let mut r = rig();
    r.script
        .say(feed("e1", &[release("v1", A, "2026-10-20T01:02:03Z")]));
    r.poller.cycle(NOW, || true).await;
    assert_eq!(keys(&r.store), [crate::update::proposal_key(A)]);
    assert!(r.rows.desired().unwrap().value.is_none());
    assert!(r.rows.policy_if_set().unwrap().is_none());
    r.rows
        .offer(&crate::update::Release::new("e".repeat(40)), NOW)
        .unwrap();
    r.rows
        .set_policy(&crate::update::Policy::default())
        .unwrap();
    let snapshot = |s: &Arc<dyn Store>| {
        s.scan_prefix("")
            .unwrap()
            .into_iter()
            .filter(|(k, _)| !k.starts_with("update/proposed/"))
            .map(|(k, v)| (k, v.value))
            .collect::<Vec<_>>()
    };
    let before = snapshot(&r.store);
    r.script
        .say(feed("e2", &[release("v2", B, "2026-10-21T00:00:00Z")]));
    assert!(matches!(
        r.poller.cycle(NOW, || true).await,
        Cycle::Proposed(_)
    ));
    assert_eq!(snapshot(&r.store), before);
}

// Catches: a store that fails (not fenced) ending the poller as a superseded handle does, or
// counting as a success.
#[tokio::test]
async fn a_store_failure_is_an_error_not_the_end() {
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    store
        .txn(kbf_state::Txn::new().put(
            "update/desired",
            b"not a row".to_vec(),
            kbf_state::Cond::Absent,
        ))
        .unwrap()
        .unwrap();
    let mut r = rig_over(store);
    r.script
        .say(feed("e1", &[release("v1", A, "2026-10-20T01:02:03Z")]));
    assert_eq!(r.poller.cycle(NOW, || true).await, Cycle::Failed);
    assert_eq!(r.stats.errors(), 1);
    assert_eq!(r.stats.proposals(), 0);
}

// Catches: the loop asking while the farm is handed over (a standby or old child polling),
// not asking once it is active again, and the wait between asks being shorter than the
// interval.
#[tokio::test(start_paused = true)]
async fn a_handed_over_farm_makes_no_request_and_an_active_one_waits_the_interval() {
    let r = rig();
    let active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&active);
    let script = Arc::clone(&r.script);
    let task = tokio::spawn(run(r.poller, || NOW, move || flag.load(Ordering::SeqCst)));
    tokio::time::sleep(Duration::from_secs(3 * 300 + 1)).await;
    assert_eq!(script.calls(), 0, "a handed-over farm polled");
    assert!(keys(&r.store).is_empty());
    script.say(Ok(Fetched::NotModified));
    script.say(Ok(Fetched::NotModified));
    active.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(script.calls(), 1);
    tokio::time::sleep(Duration::from_secs(299)).await;
    assert_eq!(script.calls(), 1, "polled before the interval");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(script.calls(), 2);
    task.abort();
}

// Catches: the loop going on after its store handle was superseded.
#[tokio::test(start_paused = true)]
async fn the_loop_ends_when_the_store_handle_is_superseded() {
    let whole: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let old: Arc<dyn Store> = Arc::new(Fenced::take(Arc::clone(&whole), None).unwrap());
    let r = rig_over(old);
    let _newer = Fenced::take(Arc::clone(&whole), None).unwrap();
    r.script
        .say(feed("e1", &[release("v1", A, "2026-10-20T01:02:03Z")]));
    let task = tokio::spawn(run(r.poller, || NOW, || true));
    tokio::time::sleep(Duration::from_secs(301)).await;
    assert!(task.is_finished());
}

#[derive(clap::Parser)]
struct Cli {
    #[command(flatten)]
    poll: PollFlags,
}

fn flags(args: &[&str]) -> Result<PollFlags, clap::Error> {
    use clap::Parser as _;
    Cli::try_parse_from(std::iter::once("t").chain(args.iter().copied())).map(|c| c.poll)
}

// Catches: the defaults moving (300 s, komira-ai/kbf, main), 0 not turning the poller off,
// and a repository that is not owner/name (a path or a URL in the request line) being taken.
#[test]
fn the_flags_default_to_five_minutes_and_zero_turns_it_off() {
    let on = flags(&[]).unwrap().config().expect("on by default");
    assert_eq!(on.every, Duration::from_secs(300));
    assert_eq!(on.repo, "komira-ai/kbf");
    assert_eq!(on.refs, ["main"]);
    assert_eq!(on.api_url, GITHUB_API);
    let off = flags(&["--release-poll-every-secs", "0"]).unwrap();
    assert!(off.config().is_none());
    let two = flags(&["--release-poll-refs", "main,release"]).unwrap();
    assert_eq!(two.release_poll_refs, ["main", "release"]);
    for bad in [
        "x",
        "a/b/c",
        "/b",
        "a/",
        "a/.hidden",
        "a b/c",
        "a/b?x=1",
        "",
    ] {
        assert!(flags(&["--release-poll-repo", bad]).is_err(), "{bad:?}");
    }
    assert!(flags(&["--release-poll-repo", "my-org/repo_1.x"]).is_ok());
}

// Catches: a poller started with nothing to keep proposals in, or with the poller off, and a
// source URL that is not one being started anyway.
#[tokio::test]
async fn spawn_needs_a_store_and_a_config_and_a_url() {
    let farm = Arc::new(Farm::new(
        Arc::new(kbf_front::Cache::<
            kbf_front::MemoryMetaLog,
            kbf_objstore::MemoryStore,
        >::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let stats = Arc::new(Stats::default());
    let go = |store: Option<Arc<dyn Store>>, config: Option<Config>| {
        spawn(Arc::clone(&farm), store, config, Arc::clone(&stats))
    };
    assert!(go(None, Some(config())).is_none());
    assert!(go(Some(Arc::clone(&store)), None).is_none());
    let mut bad = config();
    bad.api_url = "ftp://x".to_owned();
    assert!(go(Some(Arc::clone(&store)), Some(bad)).is_none());
    let task = go(Some(store), Some(config())).expect("a poller");
    task.abort();
}

// Catches: a counter missing from the metrics, mislabelled, or a reason not listed.
#[test]
fn the_metrics_list_every_counter_and_reason() {
    let stats = Stats::default();
    stats.polls.fetch_add(3, Ordering::Relaxed);
    stats.not_modified.fetch_add(2, Ordering::Relaxed);
    stats.proposals.fetch_add(1, Ordering::Relaxed);
    stats.errors.fetch_add(4, Ordering::Relaxed);
    stats.ignored[Ignore::Draft.index()].fetch_add(5, Ordering::Relaxed);
    let mut text = String::new();
    stats.render(&mut text);
    for line in [
        "kbf_release_poll_polls_total 3",
        "kbf_release_poll_not_modified_total 2",
        "kbf_release_poll_proposals_total 1",
        "kbf_release_poll_errors_total 4",
        "kbf_release_poll_ignored_total{reason=\"draft\"} 5",
        "kbf_release_poll_ignored_total{reason=\"unattested\"} 0",
    ] {
        assert!(text.lines().any(|l| l == line), "no {line:?} in\n{text}");
    }
    assert_eq!(text.matches("# TYPE").count(), 5);
}

// Catches: the choice of what to propose: the newest by time (ties by commit), nothing from
// an empty list.
#[test]
fn decide_picks_the_newest_and_none_from_nothing() {
    let c = |commit: &str, at| Candidate {
        tag: "v1".to_owned(),
        commit: commit.to_owned(),
        published_at_secs: at,
    };
    assert_eq!(decide(vec![], &[], None, RUNNING), None);
    assert_eq!(
        decide(vec![c(A, 1), c(B, 2)], &[], None, RUNNING)
            .unwrap()
            .commit,
        B
    );
    assert_eq!(
        decide(vec![c(B, 2), c(A, 2)], &[], None, RUNNING)
            .unwrap()
            .commit,
        B
    );
    // A running commit shorter than a short hash names nothing.
    assert!(decide(vec![c(A, 1)], &[], None, "a").is_some());
    assert!(decide(vec![c(A, 1)], &[], None, "aaaaaaa").is_none());
}
