//! The binary watch ([`super`]): what it does with the file at the package's path, poll by
//! poll. The kbf is a file whose content says what it prints for `--version --json` (a
//! [`FakeRunner`] reads the program it is asked to run), its modification time is set by the
//! test so that a change of the file is exact, time is a fake clock that the test advances, and
//! the swap is a fake that records the start it is given, except in the last test, where it is
//! the real [`Handover`] over the swap rig.
//!
//! Each "Asserts" line says in plain words what the test checks about the behaviour.

use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Instant, UNIX_EPOCH};

use kbf_state::postgres::schema::Outcome;

use super::*;
use crate::fixtures::{FakeRunner, ran, version_json, version_json_full};
use crate::proxy::child_http::Headers;
use crate::proxy::handover::{HandoverConfig, parts_shared};
use crate::proxy::handover_swap::{ApplyHook, GateSlot, Host, NOT_BOUND, Seams, SlotAddrs};
use crate::proxy::model::{Halt, Phase, Slot};
use crate::proxy::ports::ChildAdmin;
use crate::proxy::releases::STATUS;
use crate::proxy::runtime::SystemClock;
use crate::proxy::supervisor_swap::rig::{Rig, quick};
use crate::testkit::{A, B, C, c};

const D: &str = "dddddddddddddddddddddddddddddddddddddddd";

/// The time of the fake clock when a bench starts, in seconds.
const START_S: u64 = 1_000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A clock that moves only when the test advances it, or a sleep does. The `stop_at`th sleep
/// (counting from 1) ends with `Interrupted`, as a clock that is stopped does.
struct FakeClock {
    now: Mutex<Duration>,
    sleeps: Mutex<Vec<Duration>>,
    stop_at: usize,
}

impl FakeClock {
    fn new(stop_at: usize) -> Arc<FakeClock> {
        Arc::new(FakeClock {
            now: Mutex::new(Duration::from_secs(START_S)),
            sleeps: Mutex::new(Vec::new()),
            stop_at,
        })
    }

    fn advance(&self, by: Duration) {
        *lock(&self.now) += by;
    }

    /// Set the clock back, as a correction of the wall clock does.
    fn rewind(&self, by: Duration) {
        *lock(&self.now) -= by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *lock(&self.now)
    }

    fn sleep(&self, d: Duration) -> Result<(), Halt> {
        let mut sleeps = lock(&self.sleeps);
        sleeps.push(d);
        self.advance(d);
        if sleeps.len() >= self.stop_at {
            return Err(Halt::Interrupted);
        }
        Ok(())
    }
}

/// What the test sets on the fake swap and what it was asked.
#[derive(Default)]
struct Calls {
    busy: Option<String>,
    refuse: Option<SupervisorError>,
    started: Vec<UpdateSpec>,
}

/// A swap that starts nothing: it says it is busy when told to, refuses when told to, and
/// otherwise records the update it is asked for. A clone is the same swap.
#[derive(Clone, Default)]
struct FakeSwap(Arc<Mutex<Calls>>);

impl Swapper for FakeSwap {
    fn busy(&self) -> Option<String> {
        lock(&self.0).busy.clone()
    }

    fn start(&mut self, spec: &UpdateSpec) -> Result<(), SupervisorError> {
        let mut calls = lock(&self.0);
        if let Some(refused) = calls.refuse.clone() {
            return Err(refused);
        }
        calls.started.push(spec.clone());
        Ok(())
    }
}

/// What a build that names a "full commit" that is not one prints.
fn odd_json(commit: &str) -> String {
    version_json(commit, "release").replacen('}', ",\"commit_full\":\"ABC\"}", 1)
}

/// A runner whose program prints what its own file says: `<commit>` is a release build that
/// names its full commit, `short <commit>` one that does not, `odd <commit>` one that names
/// something that is not a commit, `full <commit>` is `<commit>` again (with a size of its
/// own), and anything else is a program that fails.
fn by_content() -> FakeRunner {
    FakeRunner::new(|spawn| {
        let said = fs::read_to_string(&spawn.program)?;
        let (how, commit) = said.split_once(' ').unwrap_or(("full", said.as_str()));
        Ok(match how {
            "full" => ran(0, &version_json_full(commit, "release"), ""),
            "short" => ran(0, &version_json(commit, "release"), ""),
            "odd" => ran(0, &odd_json(commit), ""),
            _ => ran(1, "", "boom"),
        })
    })
}

/// A file of `content` at `path`, last modified `stamp` after the epoch.
fn install(path: &Path, content: &str, stamp: Duration) {
    fs::write(path, content).expect("write the kbf");
    let opened = OpenOptions::new().write(true).open(path);
    let file = opened.expect("open the kbf");
    let modified = UNIX_EPOCH + stamp;
    file.set_modified(modified).expect("set the time");
}

fn failure(pass: Pass) -> String {
    match pass {
        Pass::Failed(text) => text,
        other => panic!("not a failure: {other:?}"),
    }
}

fn until(what: &str, done: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// A proxy's copies and a watch on the package's kbf, over a scratch directory.
struct Bench {
    root: PathBuf,
    local: Local,
    watch: Watch,
    swap: FakeSwap,
    clock: Arc<FakeClock>,
    runner: FakeRunner,
    in_use: Arc<Mutex<Vec<CommitId>>>,
}

/// The scratch directory goes with the test: the copies in it are 0755 files named `kbf`,
/// which the coverage report takes for the binary if they are left under the target directory.
impl Drop for Bench {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Bench {
    /// [`Bench::with_fake_runner`] over a runner that reads the program's content.
    fn new(name: &str, serving: Option<&str>, stop_at: usize) -> Bench {
        Bench::with_fake_runner(name, serving, stop_at, by_content())
    }

    /// A proxy that serves `serving` (the package's kbf was that and the proxy adopted it) or
    /// has recorded nothing. The clock stops the run loop at its `stop_at`th sleep.
    fn with_fake_runner(
        name: &str,
        serving: Option<&str>,
        stop_at: usize,
        runner: FakeRunner,
    ) -> Bench {
        let root = kbf_host::testkit::scratch(&format!("binary-watch-{name}"));
        let state = root.join("state");
        fs::create_dir(&state).expect("state dir");
        let local = Local::open(root.join("package-kbf"), &state).expect("copies");
        if let Some(commit) = serving {
            install(&local.binary, commit, Duration::from_secs(1));
            local.adopt(&runner).expect("adopted");
        }
        let swap = FakeSwap::default();
        let clock = FakeClock::new(stop_at);
        let in_use = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&in_use);
        let kept: InUse = Box::new(move || lock(&shared).clone());
        let watch = Watch::new(local.clone(), Box::new(swap.clone()), clock.clone(), kept);
        Bench {
            root,
            local,
            watch,
            swap,
            clock,
            runner,
            in_use,
        }
    }

    fn pass(&mut self) -> Pass {
        self.watch.pass(&self.runner)
    }

    /// Two polls; the second one's pass.
    fn two_polls(&mut self) -> Pass {
        self.pass();
        self.pass()
    }

    /// The package's kbf is now `content`, last modified at `secs` and `nanos` after the epoch.
    fn install(&self, content: &str, secs: u64, nanos: u32) {
        install(&self.local.binary, content, Duration::new(secs, nanos));
    }

    /// As [`install`](Self::install), as a new file renamed over the old one.
    fn replace(&self, content: &str, secs: u64, nanos: u32) {
        let temp = self.root.join("package-kbf.new");
        install(&temp, content, Duration::new(secs, nanos));
        fs::rename(&temp, &self.local.binary).expect("replace the kbf");
    }

    /// The proxy's `releases.json`.
    fn status_file(&self) -> PathBuf {
        self.root.join("state").join(STATUS)
    }

    /// The proxy's record says `commit` serves, with no copy of it.
    fn record_serving(&self, commit: &str) {
        let text = format!("{{\"installed_commit\":\"{commit}\"}}\n");
        fs::write(self.status_file(), text).expect("releases.json");
    }

    fn started(&self) -> Vec<UpdateSpec> {
        lock(&self.swap.0).started.clone()
    }

    fn staged(&self) -> Option<StagedFacts> {
        self.local.orders.staged().expect("releases.json reads")
    }

    /// How many times a kbf has been run.
    fn calls(&self) -> usize {
        self.runner.calls().len()
    }

    /// The names in the copies' root, sorted.
    fn copies(&self) -> Vec<String> {
        let entries = fs::read_dir(self.local.copies.root()).expect("list");
        let mut names: Vec<String> = entries
            .map(|entry| entry.expect("entry").file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A verified copy of `commit` that nothing refers to.
    fn copy_of(&self, commit: &str) {
        let src = self.root.join(format!("src-{commit}"));
        fs::write(&src, commit).expect("source");
        let made = self.local.copies.ensure(&self.runner, &src, &c(commit));
        made.expect("a copy");
    }

    /// Advance the clock by one poll at a time until a pass does something; the time that took
    /// and what it came to.
    fn until_next_try(&mut self) -> (Duration, Pass) {
        let mut waited = Duration::ZERO;
        loop {
            self.clock.advance(POLL);
            waited += POLL;
            let pass = self.pass();
            if pass != Pass::Quiet {
                return (waited, pass);
            }
            assert!(waited <= RETRY_CAP * 2, "it never tried again");
        }
    }
}

/// Catches a watch that acts on a file the first time it sees it, one that never acts, and one
/// that acts twice: a new kbf (`B`, while `A` serves) is left alone by the first poll; the
/// second, which reads the same file, copies it, stages the facts its build printed and starts
/// one swap, `A` to `B`, with `nodes` off, the source `staged` and the update id of the commit
/// and the second; every later poll does nothing.
///
/// Asserts that nothing happens on the first poll, that the action comes on the second, that
/// the identity is consumed (no second start), that the facts staged are those of the probe,
/// that `nodes` is off, that the source is not `fetch`, that the update id is made of the
/// short commit and the time, and that the copy is made.
#[test]
fn a_new_binary_starts_exactly_one_swap_after_two_equal_polls() {
    let mut bench = Bench::new("start", Some(A), usize::MAX);
    bench.install(B, 1, 0);

    assert_eq!(bench.pass(), Pass::Quiet);
    assert!(bench.started().is_empty(), "acted on the first poll");
    assert_eq!(bench.staged(), None);

    let id = format!("pkg-{}-{START_S}", &B[..12]);
    assert_eq!(bench.pass(), Pass::Started(id.clone()));

    let facts = StagedFacts {
        commit: c(B),
        protocol: 4,
        min_protocol: 3,
        state_schema: 7,
        profile: Some("release".to_owned()),
    };
    let started = bench.started();
    assert_eq!(started.len(), 1);
    let spec = &started[0];
    assert_eq!(spec.update_id, UpdateId(id.clone()));
    assert_eq!(spec.request_id, id);
    assert_eq!((spec.from.clone(), spec.to.clone()), (Some(c(A)), c(B)));
    assert_eq!((spec.nodes, spec.rollback), (false, false));
    assert_eq!(spec.source, Source::Staged);
    assert_eq!(spec.facts, facts);
    assert_eq!(spec.order.commit, c(B));
    assert_eq!(spec.order.verb, Verb::FrontendUpdate);
    assert_eq!(bench.staged(), Some(facts));
    assert!(bench.local.copies.verified(&c(B)));

    for _ in 0..3 {
        assert_eq!(bench.pass(), Pass::Quiet);
    }
    assert_eq!(bench.started().len(), 1, "a second swap was started");
}

type Change = fn(&Bench);

/// Catches a half-unpacked file being acted on: a file that changes between two polls is not the
/// same file, whichever of its modification time (seconds, or only the fraction), its size or
/// its inode is what changed, so the poll after the change waits and the one after that acts.
///
/// Asserts that the identity holds the seconds, the nanoseconds, the size and the inode, and
/// that each poll is compared with the last poll, not the first.
#[test]
fn a_change_of_any_part_of_the_identity_makes_the_next_poll_wait() {
    let changes: [(&str, Change); 4] = [
        ("mtime", |bench| bench.install(B, 2, 0)),
        ("mtime_nsec", |bench| bench.install(B, 1, 7)),
        ("size", |bench| bench.install(&format!("full {B}"), 1, 0)),
        ("inode", |bench| bench.replace(B, 1, 0)),
    ];
    for (name, change) in changes {
        let mut bench = Bench::new(&format!("identity-{name}"), Some(A), usize::MAX);
        bench.install(B, 1, 0);
        assert_eq!(bench.pass(), Pass::Quiet, "{name}");

        change(&bench);

        assert_eq!(bench.pass(), Pass::Quiet, "{name} was acted on at once");
        assert!(bench.started().is_empty(), "{name}");
        assert!(matches!(bench.pass(), Pass::Started(_)), "{name}");
    }
}

/// Catches a file that is written for several polls being acted on before it holds still, and
/// a file that is gone being remembered: four polls in a row see a file that changes each time
/// and none acts; the fifth sees the fourth's file and acts. A file that is moved away and put
/// back is new to the watch, though it is the very same file (the same inode and time).
///
/// Asserts that nothing is done while the identity changes, and that a file that cannot be
/// read leaves no identity behind.
#[test]
fn a_file_still_being_written_or_gone_starts_the_two_polls_over() {
    let mut bench = Bench::new("half", Some(A), usize::MAX);
    for secs in 1..=4 {
        bench.install(B, secs, 0);
        assert_eq!(bench.pass(), Pass::Quiet);
    }
    assert!(bench.started().is_empty());
    assert!(matches!(bench.pass(), Pass::Started(_)));

    let mut bench = Bench::new("gone", Some(A), usize::MAX);
    bench.install(B, 1, 0);
    assert_eq!(bench.pass(), Pass::Quiet);
    let away = bench.root.join("away");
    fs::rename(&bench.local.binary, &away).expect("move the kbf away");
    assert_eq!(bench.two_polls(), Pass::Quiet);
    fs::rename(&away, &bench.local.binary).expect("move the kbf back");
    assert_eq!(bench.pass(), Pass::Quiet, "the file is new to the watch");
    assert!(bench.started().is_empty());
    assert!(matches!(bench.pass(), Pass::Started(_)));
}

/// Catches a change that is lost, or acted on too early, while a swap runs or the proxy cannot
/// take one: while the swap says it is busy every poll reports why and nothing is run, copied,
/// staged or started; when it is not busy any more the very next poll starts the swap, which
/// it could not if the identity had been consumed meanwhile.
///
/// Asserts that the identity is kept while the swap is busy, that the busy answer is asked, and
/// that the kbf is neither run nor copied before it is asked.
#[test]
fn a_swap_in_flight_leaves_the_change_pending_until_it_can_start() {
    let mut bench = Bench::new("busy", Some(A), usize::MAX);
    let why = "update u1 is Baking".to_owned();
    lock(&bench.swap.0).busy = Some(why.clone());
    let before = bench.calls();
    bench.install(B, 1, 0);

    assert_eq!(bench.pass(), Pass::Quiet);
    for _ in 0..3 {
        assert_eq!(bench.pass(), Pass::Busy(why.clone()));
    }

    assert_eq!(bench.calls(), before, "the new kbf was run while busy");
    assert!(bench.started().is_empty());
    assert_eq!(bench.staged(), None);
    assert!(!bench.local.copies.verified(&c(B)));

    lock(&bench.swap.0).busy = None;
    assert!(matches!(bench.pass(), Pass::Started(_)));
    assert_eq!(bench.started().len(), 1);
}

/// Catches a swap to the release that already serves, and a file that is run again at every
/// poll: a kbf that is a build of `A` while `A` serves is run once (to learn that), nothing is
/// staged, copied or started, and the later polls do not run it again. A new file that is
/// still `A` is read once more.
///
/// Asserts that no swap is started to the serving commit, that the identity is consumed (the
/// kbf is not run at every poll), and that the same-commit answer comes before the copy and
/// the stage.
#[test]
fn the_release_that_serves_is_not_swapped_to() {
    let mut bench = Bench::new("same", Some(A), usize::MAX);
    let before = bench.calls();
    bench.install(A, 2, 0);

    assert_eq!(bench.two_polls(), Pass::Same(c(A)));
    for _ in 0..3 {
        assert_eq!(bench.pass(), Pass::Quiet);
    }

    assert_eq!(bench.calls(), before + 1, "the kbf was run again");
    assert!(bench.started().is_empty());
    assert_eq!(bench.staged(), None);
    assert_eq!(bench.copies(), [A]);

    bench.install(A, 3, 0);
    assert_eq!(bench.two_polls(), Pass::Same(c(A)));
    assert_eq!(bench.calls(), before + 2);
}

/// Catches a failure that is hidden, that changes something, or that is not told apart: a kbf
/// that fails, one that prints no `commit_full`, one whose `commit_full` is not a commit, no
/// installed release recorded, a record that cannot be read, a copy that is another build than
/// the kbf said, and a record that cannot be written are each a failed pass that says which,
/// start no swap, stage nothing and leave no copy that is not whole.
///
/// Asserts that each of these is a failure, that a failure leaves no partial copy, and that the
/// reason of a failure is kept.
#[test]
fn a_kbf_that_cannot_be_used_is_a_failed_pass_that_says_why_and_changes_nothing() {
    let short = format!("short {B}");
    let odd = format!("odd {B}");
    let cases = [
        ("exit", "exit 1", "probe: "),
        ("short", short.as_str(), "the kbf prints no commit_full"),
        ("odd", odd.as_str(), "commit_full: "),
    ];
    for (name, content, want) in cases {
        let mut bench = Bench::new(name, Some(A), usize::MAX);
        bench.install(content, 1, 0);
        let text = failure(bench.two_polls());
        assert!(text.starts_with(want), "{name}: {text}");
        assert!(bench.started().is_empty(), "{name}");
        assert_eq!(bench.staged(), None, "{name}");
        assert_eq!(bench.copies(), [A], "{name}");
    }

    let mut bench = Bench::new("unserved", None, usize::MAX);
    bench.install(B, 1, 0);
    let text = failure(bench.two_polls());
    assert_eq!(text, "no installed release is recorded");
    assert!(bench.copies().is_empty());

    let mut bench = Bench::new("unreadable", Some(A), usize::MAX);
    fs::write(bench.status_file(), b"{").expect("torn releases.json");
    bench.install(B, 1, 0);
    let text = failure(bench.two_polls());
    assert!(text.starts_with("status: "), "{text}");

    let first = AtomicBool::new(true);
    let liar = FakeRunner::new(move |_| {
        let stdout = if first.swap(false, Ordering::SeqCst) {
            version_json_full(B, "release")
        } else {
            version_json(C, "release")
        };
        Ok(ran(0, &stdout, ""))
    });
    let mut bench = Bench::with_fake_runner("mismatch", None, usize::MAX, liar);
    bench.record_serving(A);
    bench.install(B, 1, 0);
    let text = failure(bench.two_polls());
    assert!(text.starts_with("copy: commit_mismatch: "), "{text}");
    assert!(bench.copies().is_empty(), "a copy of another build is left");
    assert!(bench.started().is_empty());

    let mut bench = Bench::new("unwritable", Some(A), usize::MAX);
    let in_the_way = bench.status_file().with_extension("json.new");
    fs::create_dir(in_the_way).expect("a directory where the new file goes");
    bench.install(B, 1, 0);
    let text = failure(bench.two_polls());
    assert!(text.starts_with("stage: cannot write "), "{text}");
    assert!(bench.started().is_empty());
}

/// Catches a failure that is tried again in a loop, never, or at a pace that does not grow: a
/// kbf that stops running is run again 10 s after the first failure, then 20, 40, 80, 160 and
/// 300 s later, and every 300 s after that, and nothing is run in between; when it works again
/// the very file that failed is acted on, with no change to it.
///
/// Asserts that there is a wait (the kbf is not run at every poll), that the wait doubles and
/// has a cap, and that a failure does not consume the identity (it is tried again).
#[test]
fn a_failed_identity_is_tried_again_after_a_doubling_wait_up_to_the_cap() {
    let broken = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&broken);
    let runner = FakeRunner::new(move |spawn| {
        if flag.load(Ordering::SeqCst) {
            return Ok(ran(1, "", "boom"));
        }
        let said = fs::read_to_string(&spawn.program)?;
        Ok(ran(0, &version_json_full(&said, "release"), ""))
    });
    let mut bench = Bench::with_fake_runner("backoff", Some(A), usize::MAX, runner);
    bench.install(B, 1, 0);
    broken.store(true, Ordering::SeqCst);
    let before = bench.calls();

    assert_eq!(bench.pass(), Pass::Quiet);
    assert!(matches!(bench.pass(), Pass::Failed(_)));
    let mut waits = Vec::new();
    for _ in 0..7 {
        let (waited, pass) = bench.until_next_try();
        assert!(matches!(pass, Pass::Failed(_)), "{pass:?}");
        waits.push(waited.as_secs());
    }

    assert_eq!(waits, [10, 20, 40, 80, 160, 300, 300]);
    assert_eq!(bench.calls(), before + 8, "run between the tries");
    broken.store(false, Ordering::SeqCst);
    let (waited, pass) = bench.until_next_try();
    assert_eq!(waited, RETRY_CAP);
    assert!(matches!(pass, Pass::Started(_)), "{pass:?}");
    assert_eq!(bench.started().len(), 1);
}

/// Catches a wait that outlives the file it was for: after a failure, a different file (its
/// modification time moved) is read at its second poll without waiting out the old wait, and
/// its own first failure waits the first wait again (10 s), not the second; a third file that
/// works is started at its second poll.
///
/// Asserts that the wait is reset when the identity changes, both its time and the count of
/// failures (the second file does not wait 20 s).
#[test]
fn a_different_file_is_tried_at_once_even_after_failures() {
    let mut bench = Bench::new("fresh", Some(A), usize::MAX);
    bench.install("exit 1", 1, 0);
    assert!(matches!(bench.two_polls(), Pass::Failed(_)));
    assert_eq!(bench.pass(), Pass::Quiet, "waiting out the first failure");

    bench.install("exit 1", 2, 0);

    assert_eq!(bench.pass(), Pass::Quiet);
    let second = bench.pass();
    assert!(matches!(second, Pass::Failed(_)), "{second:?}");
    let (waited, pass) = bench.until_next_try();
    assert_eq!(waited, POLL * 2, "the count of failures was kept");
    assert!(matches!(pass, Pass::Failed(_)), "{pass:?}");
    let before = bench.calls();

    bench.install(B, 3, 0);

    assert_eq!(bench.pass(), Pass::Quiet);
    assert!(matches!(bench.pass(), Pass::Started(_)));
    assert_eq!(bench.calls(), before + 2, "the kbf and its copy");
}

/// Catches a wait that a clock set back stretches past the cap: after a failure at 1000 s (a
/// wait of 10 s) the clock is set back to 500 s, and the file is tried again 305 s later (the
/// first poll after the step, 505 s, puts the end of the wait at the cap from then), not at
/// 1010 s, which is 510 s after the step.
///
/// Asserts that the end of a wait is recomputed when the clock goes back.
#[test]
fn a_clock_set_back_does_not_stretch_a_wait_past_the_cap() {
    let mut bench = Bench::new("rewind", Some(A), usize::MAX);
    bench.install("exit 1", 1, 0);
    assert!(matches!(bench.two_polls(), Pass::Failed(_)));
    bench.clock.rewind(Duration::from_secs(500));

    let (waited, pass) = bench.until_next_try();

    assert_eq!(waited, RETRY_CAP + POLL);
    assert!(matches!(pass, Pass::Failed(_)), "{pass:?}");
}

/// Catches a swap that refuses to start being forgotten, or leaving its staging: a refusal is a
/// failed pass that says the swap refused and why, the release staged for it is dropped from
/// `releases.json` (nothing is in flight to confirm or revert it), and the same file is
/// tried again after the wait and starts the swap once it is accepted.
///
/// Asserts that the staged release is cleared after a refusal, and that a refusal is not a
/// start.
#[test]
fn a_swap_that_refuses_to_start_leaves_nothing_staged_and_is_tried_again() {
    let mut bench = Bench::new("refused", Some(A), usize::MAX);
    let refusal = SupervisorError::Refused("update u0 is still open".to_owned());
    lock(&bench.swap.0).refuse = Some(refusal);
    bench.install(B, 1, 0);

    let text = failure(bench.two_polls());

    assert_eq!(text, "swap: refused: update u0 is still open");
    assert_eq!(bench.staged(), None);
    assert!(bench.started().is_empty());
    lock(&bench.swap.0).refuse = None;
    let (waited, pass) = bench.until_next_try();
    assert_eq!(waited, POLL * 2);
    assert!(matches!(pass, Pass::Started(_)), "{pass:?}");
    assert_eq!(bench.staged().map(|facts| facts.commit), Some(c(B)));
}

/// Catches a prune that removes a copy that is needed, or none: with `A` serving, `C` an old
/// release and `D` the release a slot's process may still run, a swap to `B` leaves the copies
/// of `A` (serving), `B` (the swap's) and `D` (a slot's), and `C` is gone. A poll that finds the
/// serving release again prunes by the same rule, with nothing to go to.
///
/// Asserts that the serving copy, the swap's copy and the slots' copies are kept, and that the
/// others are pruned.
#[test]
fn the_copies_kept_are_the_serving_one_the_swaps_and_each_slots() {
    let mut bench = Bench::new("prune", Some(A), usize::MAX);
    bench.copy_of(C);
    bench.copy_of(D);
    *lock(&bench.in_use) = vec![c(D)];
    assert_eq!(bench.copies(), [A, C, D]);
    bench.install(B, 1, 0);

    assert!(matches!(bench.two_polls(), Pass::Started(_)));

    assert_eq!(bench.copies(), [B, A, D]);

    let mut bench = Bench::new("prune-same", Some(A), usize::MAX);
    bench.copy_of(C);
    bench.copy_of(D);
    *lock(&bench.in_use) = vec![c(D)];
    bench.install(A, 2, 0);

    assert_eq!(bench.two_polls(), Pass::Same(c(A)));

    assert_eq!(bench.copies(), [A, D]);
}

/// Catches a prune that fails the pass: the copies' root is gone, which the prune cannot list,
/// and the pass still comes to what it found (the serving release), with the failure only
/// logged.
///
/// Asserts that the prune's error is not the pass's result.
#[test]
fn a_prune_that_fails_does_not_fail_the_pass() {
    let mut bench = Bench::new("prune-fails", Some(A), usize::MAX);
    fs::remove_dir_all(bench.local.copies.root()).expect("remove the copies");
    bench.install(A, 2, 0);

    assert_eq!(bench.two_polls(), Pass::Same(c(A)));
}

/// Catches a loop that does not poll every five seconds, does not stop with its clock, or that
/// gets stuck on a pass: with a clock that stops at the fourth sleep, the loop polls four
/// times, sleeps [`POLL`] after each, starts the one swap the file asks for, and returns. A new
/// kbf that keeps waiting for a swap (the logging of that is once; what is seen here is that
/// the loop goes on) starts nothing.
///
/// Asserts that the sleep is five seconds, that the loop stops on `Interrupted`, and that the
/// loop goes on after a pass that does something.
#[test]
fn run_polls_every_five_seconds_until_the_clock_stops() {
    let mut bench = Bench::new("run", Some(A), 4);
    bench.install(B, 1, 0);

    bench.watch.run(&bench.runner);

    assert_eq!(*lock(&bench.clock.sleeps), [POLL; 4]);
    assert_eq!(bench.started().len(), 1);

    let mut bench = Bench::new("run-busy", Some(A), 5);
    lock(&bench.swap.0).busy = Some("update u1 is Baking".to_owned());
    bench.install(B, 1, 0);

    bench.watch.run(&bench.runner);

    assert_eq!(*lock(&bench.clock.sleeps), [POLL; 5]);
    assert!(bench.started().is_empty());
}

/// Catches words of the log that drifted from what they say, and a wait that overflows: each
/// pass has its text, and a count of failures past what a shift can hold still waits the cap.
///
/// Asserts that the wait is computed with a bounded shift of the failure count.
#[test]
fn passes_have_their_words_and_a_wait_never_overflows() {
    assert_eq!(Pass::Quiet.to_string(), "nothing to do");
    let busy = Pass::Busy("why".to_owned());
    assert_eq!(busy.to_string(), "a new kbf waits for the swap: why");
    let same = Pass::Same(c(A));
    let want = format!("{A} is the release that serves");
    assert_eq!(same.to_string(), want);
    assert_eq!(Pass::Started("x".into()).to_string(), "swap x started");
    let failed = Pass::Failed("probe: boom".to_owned());
    assert_eq!(failed.to_string(), "probe: boom; trying again later");

    let worn = Retry {
        failures: u32::MAX,
        after: Duration::ZERO,
    };
    let then = worn.failed(Duration::from_secs(7));
    assert_eq!(then.after, Duration::from_secs(7) + RETRY_CAP);
    assert_eq!(then.failures, u32::MAX);
}

/// Catches facts that wrap: a number a build prints that does not fit the swap's is the
/// largest one, which its checks refuse, not a small one that passes; the others and the
/// profile are as printed.
///
/// Asserts that a number too big is saturated, not truncated by a cast.
#[test]
fn a_number_too_big_for_the_facts_is_the_biggest_not_a_wrapped_one() {
    let json = version_json(B, "release");
    let json = json.replace("\"protocol\":4", "\"protocol\":5000000000");
    let version = Version::parse(json.as_bytes()).expect("a version");

    let facts = facts_of(&version, &c(B));

    assert_eq!(facts.protocol, u32::MAX);
    assert_eq!((facts.min_protocol, facts.state_schema), (3, 7));
    assert_eq!(facts.profile.as_deref(), Some("release"));
}

// ----- over the real handover -----

/// A configuration for a proxy that keeps its own copies, over `root`'s `state/` directory.
fn config_for(local: &Local, root: &Path) -> HandoverConfig {
    let state_dir = root.join("state");
    let addr = SocketAddr::from(([127, 0, 0, 1], 1));
    let cfg = HandoverConfig {
        state_dir: state_dir.clone(),
        orders_dir: root.join("orders"),
        status_file: state_dir.join(STATUS),
        local: Some(local.clone()),
        confirm_window_s: 900,
        fetch_timeout_s: 900,
        slots: SlotAddrs {
            admin: [addr, addr],
            operator: [addr, addr],
        },
        installer_toml: root.join("installer.toml"),
        gate: GateSlot::new(),
    };
    fs::write(
        &cfg.installer_toml,
        "database = \"postgres:///kbf?host=/nonexistent-kbf-test-socket\"\n",
    )
    .expect("installer.toml");
    cfg
}

/// The host as the verb reports it, over the rig's children, with `releases` as the copies.
fn host_over(rig: &Arc<Rig>, releases: PathBuf) -> Host {
    let by_slot = Arc::clone(rig);
    Host {
        gate: rig.clone(),
        child: rig.clone(),
        recorded: Arc::new(move |slot| by_slot.ident(slot)),
        active: Slot::A,
        held: None,
        releases_dir: releases,
        secret: Headers::new(),
        on_death: Arc::new(|_| {}),
    }
}

/// The schemas a migration was asked for.
type Schemas = Arc<Mutex<Vec<u32>>>;

/// A migration that records the schema it is asked for and waits for the test to send on the
/// returned sender before it answers.
fn blocking_apply() -> (ApplyHook, Schemas, mpsc::Sender<()>) {
    let (release, wait) = mpsc::channel::<()>();
    let wait = Mutex::new(wait);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let hook: ApplyHook = Arc::new(move |_db, schema, _wait| {
        lock(&sink).push(schema);
        let _ = lock(&wait).recv();
        Ok(Outcome {
            from: Some(2),
            to: 3,
            ran: vec![3],
        })
    });
    (hook, seen, release)
}

/// The watch over the real [`Handover`] and the swap rig, as the verb wires it: before the proxy
/// is bound the new kbf waits ("proxy not bound yet"); once it is, one poll copies it, stages
/// it and starts the swap, which the clone the API holds sees at once (one swap, not two); while
/// the migration runs, a changed file waits for the swap to be over and a second start with the
/// same update id is a no-op; when the swap is done `B` is installed in `releases.json` and the
/// rig saw exactly one candidate start, and the file, still `B`, is the release that serves.
///
/// Asserts that the handover is shared with the API's clone (the clone shows the update), that
/// `busy` looks at the update going on (a second swap is neither started nor refused), and
/// that the migration is asked for the schema the kbf printed.
#[test]
fn the_real_handover_runs_the_swap_the_watch_starts_and_the_apis_clone_sees_it() {
    let mut bench = Bench::new("real", Some(A), usize::MAX);
    let cfg = config_for(&bench.local, &bench.root);
    let rig = Rig::new(None);
    let admin: Arc<dyn ChildAdmin + Send + Sync> = rig.clone();
    let (hook, schemas, release) = blocking_apply();
    let seams = Seams {
        apply: Some(hook),
        admin: Some(admin),
        timing: Some(quick()),
    };
    let orders = Arc::clone(&bench.local.orders);
    let mut api = Handover::local_with_seams(&cfg, orders, seams);
    let in_use: InUse = Box::new(Vec::new);
    let clock = Arc::clone(&bench.clock);
    bench.watch = Watch::new(bench.local.clone(), Box::new(api.clone()), clock, in_use);
    bench.install(B, 1, 0);

    assert_eq!(Swapper::busy(&api).as_deref(), Some(NOT_BOUND));
    assert_eq!(bench.pass(), Pass::Quiet);
    assert_eq!(bench.pass(), Pass::Busy(NOT_BOUND.to_owned()));
    assert!(!bench.local.copies.verified(&c(B)));

    cfg.gate
        .fill(host_over(&rig, bench.local.copies.root().to_owned()));
    let id = format!("pkg-{}-{START_S}", &B[..12]);
    assert_eq!(bench.pass(), Pass::Started(id.clone()));

    let shown = api.status().update.expect("the update is shown");
    assert_eq!(shown.update_id, UpdateId(id.clone()));
    let busy = Swapper::busy(&api).expect("an update is going on");
    assert!(busy.contains(&id), "{busy}");
    bench.install(B, 2, 0);
    assert_eq!(bench.pass(), Pass::Quiet);
    let waiting = bench.pass();
    assert!(
        matches!(&waiting, Pass::Busy(why) if why.contains(&id)),
        "{waiting:?}"
    );

    let facts = bench.staged().expect("staged for the migration");
    let again = bench.watch.spec(&c(A), facts);
    assert_eq!(again.update_id, UpdateId(id));
    assert_eq!(Swapper::start(&mut api, &again), Ok(()));

    release.send(()).expect("the migration is waiting");
    until("the update to end", || {
        api.status()
            .update
            .is_some_and(|u| matches!(u.phase, Phase::Done | Phase::Failed))
    });

    let update = api.status().update.expect("shown");
    assert_eq!(update.phase, Phase::Done, "{:?}", update.failure);
    assert_eq!(*lock(&schemas), [7]);
    assert_eq!(bench.local.orders.installed(), Ok(Some(c(B))));
    assert_eq!(rig.count("spawn"), 1, "one candidate was started");
    until("the swap to be over", || Swapper::busy(&api).is_none());
    assert_eq!(bench.pass(), Pass::Same(c(B)));
    assert_eq!(rig.count("spawn"), 1);
}

/// Catches a watch whose swap is not the one the update API holds: with the wiring and the
/// supervisor that `parts_shared` makes, an update started through the wiring is shown by the
/// supervisor of the API's parts (the same swap, not one built beside it). The swap runs on
/// the defaults, so only what `start` records at once is looked at, not how the update ends.
///
/// Asserts that the wiring uses the one `Handover` of the API's supervisor (the supervisor
/// shows the update).
#[test]
fn the_wiring_and_the_apis_supervisor_are_one_swap() {
    let bench = Bench::new("shared", Some(A), usize::MAX);
    let cfg = config_for(&bench.local, &bench.root);
    let rig = Rig::new(None);
    let releases = bench.local.copies.root().to_owned();
    assert!(cfg.gate.fill(host_over(&rig, releases)));
    let (api, wiring) = parts_shared(&cfg).expect("the seams");
    let mut swap = wiring.expect("a proxy with copies is watched").swap;
    let facts = StagedFacts {
        commit: c(B),
        protocol: 4,
        min_protocol: 3,
        state_schema: 7,
        profile: None,
    };
    let spec = bench.watch.spec(&c(A), facts);
    assert!(api.supervisor.status().update.is_none());

    assert_eq!(Supervisor::start(&mut swap, &spec), Ok(()));

    let update = api.supervisor.status().update;
    let shown = update.expect("the API's supervisor shows the update");
    assert_eq!(shown.update_id, spec.update_id);
}

/// Catches a thread that is started without the flag, or one that does not end with its clock:
/// no wiring spawns nothing, and a wiring spawns a thread that polls once, finds no file at the
/// package's path, and returns because the clock it sleeps on is stopped.
///
/// Asserts that a thread is spawned only with a wiring, and that the loop ends with its clock
/// (the join returns).
#[test]
fn nothing_is_spawned_without_a_wiring_and_the_thread_ends_with_its_clock() {
    let none = spawn(None, Arc::new(SystemClock::new()), Box::new(Vec::new));
    assert!(none.is_none());

    let bench = Bench::new("spawn", None, usize::MAX);
    let cfg = config_for(&bench.local, &bench.root);
    let swap = Handover::local(&cfg, Arc::clone(&bench.local.orders));
    let wiring = Wiring {
        local: bench.local.clone(),
        swap,
    };
    let clock = Arc::new(SystemClock::new());
    clock.stop();

    let thread = spawn(Some(wiring), clock, Box::new(Vec::new)).expect("a thread");

    thread.join().expect("the watch ended with its clock");
}
