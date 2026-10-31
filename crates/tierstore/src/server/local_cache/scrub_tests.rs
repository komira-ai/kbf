//! The scrub: what it drops, what it keeps, how much it reads a tick, and when it runs.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt as _;
use tokio::sync::{Notify, mpsc};

use super::layout::{Sidecar, data_path, meta_path, name_of};
use super::metrics::read;
use super::scrub::{Every, ScrubPass, Scrubber, Ticker, Verdict};
use super::testing::{Rig, digest, pattern};
use crate::{Backend as _, Op};

/// A ticker that ticks when the test says so, and says when the scrub asks for the next tick (so
/// the pass before it is over).
struct Hand {
    ticks: mpsc::Receiver<()>,
    asked: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl Ticker for Hand {
    async fn tick(&mut self) -> bool {
        let _ = self.asked.send(());
        self.ticks.recv().await.is_some()
    }
}

/// A ticker, the hand that drives it, and the receiver of its questions.
fn hand() -> (Hand, mpsc::Sender<()>, mpsc::UnboundedReceiver<()>) {
    let (tick, ticks) = mpsc::channel(1);
    let (asked, asks) = mpsc::unbounded_channel();
    (Hand { ticks, asked }, tick, asks)
}

/// Changes one byte of the data file of `key` in place, as rot does.
fn rot(rig: &Rig, key: &str) {
    let path = rig.file(key);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&path, bytes).unwrap();
}

/// Rewrites the sidecar of `key` with `change` applied.
fn edit_sidecar(rig: &Rig, key: &str, change: impl FnOnce(&mut Sidecar)) {
    let path = meta_path(&rig.config.path, &name_of(key).hex);
    let mut sidecar: Sidecar = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    change(&mut sidecar);
    std::fs::write(path, serde_json::to_vec(&sidecar).unwrap()).unwrap();
}

/// Fills `count` objects of `size` bytes, under distinct names.
async fn fill(rig: &Rig, count: usize, size: usize) -> Vec<String> {
    let mut keys = Vec::new();
    for n in 0..count {
        let key = format!("cas/b/{n}");
        let mut body = pattern(size);
        body[0] = n as u8;
        rig.put(&key, &body).await;
        rig.get(&key).await.unwrap();
        keys.push(key);
    }
    keys
}

/// Asserts that an object of a key that is not named by its content, whose bytes change on the
/// disk, is dropped by one pass (index, data file and sidecar, counted as scrubbed and as a
/// mismatch), and that the next read fills it again from the upstream with the right bytes.
#[tokio::test]
async fn a_rotten_object_is_dropped_by_one_pass_and_the_next_read_refills_it() {
    let rig = Rig::new("tscache-scrub-rot");
    let body = pattern(1000);
    rig.put("cas/b/obj", &body).await;
    rig.get("cas/b/obj").await.unwrap();
    rot(&rig, "cas/b/obj");

    let mut state = Scrubber::new(1 << 20);
    let pass = rig.cache.scrub_once(&mut state);
    let expected = ScrubPass {
        scrubbed: 1,
        bytes: 1000,
        dropped: 1,
    };
    assert_eq!(pass, expected);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));
    assert!(!meta_path(&rig.config.path, &name_of("cas/b/obj").hex).exists());
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.scrubbed_objects), 1);
    assert_eq!(read(&counters.scrubbed_bytes), 1000);
    assert_eq!(read(&counters.scrub_mismatches), 1);
    let text = rig.cache.metrics();
    assert!(text.contains("tierstore_cache_scrubbed_objects_total 1\n"));
    assert!(text.contains("tierstore_cache_scrubbed_bytes_total 1000\n"));
    assert!(text.contains("tierstore_cache_scrub_mismatches_total 1\n"));

    let gets = rig.calls(Op::Get);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(rig.calls(Op::Get), gets + 1);
    assert!(rig.has_file("cas/b/obj"));
    assert_eq!(std::fs::read(rig.file("cas/b/obj")).unwrap(), body);
    assert_eq!(rig.cache.held(), (1000, 1));
}

/// Asserts that a key named by its content is judged by its name: rot is dropped, and a sidecar
/// whose recorded hash is wrong does not drop an object whose bytes hash to the name.
#[tokio::test]
async fn a_content_named_object_is_judged_by_its_name() {
    let rig = Rig::new("tscache-scrub-cas-l");
    let body = pattern(500);
    let key = format!("cas/l/{}", digest(&body));
    rig.put(&key, &body).await;
    rig.get(&key).await.unwrap();
    edit_sidecar(&rig, &key, |sidecar| sidecar.sha256 = "0".repeat(64));
    let mut state = Scrubber::new(1 << 20);
    assert_eq!(rig.cache.scrub_once(&mut state).dropped, 0);
    assert!(rig.has_file(&key));

    rot(&rig, &key);
    let pass = rig.cache.scrub_once(&mut state);
    assert_eq!((pass.scrubbed, pass.dropped), (1, 1));
    assert!(!rig.has_file(&key));
    assert_eq!(rig.get(&key).await.unwrap(), body);
    assert!(rig.has_file(&key));
}

/// Asserts that for any other key the digest is the one the sidecar recorded at the fill: a
/// sidecar that says another digest, is not there, or is not a sidecar, drops the object.
#[tokio::test]
async fn a_plain_key_is_judged_by_its_sidecar_and_an_unreadable_sidecar_drops_it() {
    let rig = Rig::new("tscache-scrub-sidecar");
    let keys = fill(&rig, 3, 300).await;
    edit_sidecar(&rig, &keys[0], |sidecar| sidecar.sha256 = "0".repeat(64));
    let other = meta_path(&rig.config.path, &name_of(&keys[1]).hex);
    std::fs::write(other, b"not json").unwrap();
    std::fs::remove_file(meta_path(&rig.config.path, &name_of(&keys[2]).hex)).unwrap();
    let mut state = Scrubber::new(1 << 20);
    let pass = rig.cache.scrub_once(&mut state);
    assert_eq!((pass.scrubbed, pass.dropped), (3, 3));
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(keys.iter().all(|key| !rig.has_file(key)));
    assert_eq!(read(&rig.cache.counters().scrub_errors), 0);
}

/// Asserts that an object whose bytes are as they were is kept, indexed and served from the disk
/// with no call to the upstream, and counted as scrubbed and not as a mismatch.
#[tokio::test]
async fn an_intact_object_is_kept() {
    let rig = Rig::new("tscache-scrub-intact");
    let body = pattern(1000);
    rig.put("cas/b/obj", &body).await;
    rig.get("cas/b/obj").await.unwrap();
    let gets = rig.calls(Op::Get);
    let mut state = Scrubber::new(1 << 20);
    let pass = rig.cache.scrub_once(&mut state);
    let expected = ScrubPass {
        scrubbed: 1,
        bytes: 1000,
        dropped: 0,
    };
    assert_eq!(pass, expected);
    assert_eq!(rig.cache.held(), (1000, 1));
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(rig.calls(Op::Get), gets);
    assert_eq!(read(&rig.cache.counters().scrub_mismatches), 0);
}

/// Asserts that an object with a fill that is still ending (its name is in the table of the fills
/// that run) is not touched even if its bytes are wrong, and is dropped by the pass after the
/// fill is gone; and that a fill that is running for another key, with its file in the stage
/// directory, is not touched either.
#[tokio::test]
async fn an_object_whose_fill_is_running_is_not_touched() {
    let rig = Rig::new("tscache-scrub-midfill");
    let keys = fill(&rig, 1, 400).await;
    rot(&rig, &keys[0]);
    let hex = name_of(&keys[0]).hex;
    let held = futures::future::pending().boxed().shared();
    super::locked(&rig.cache.inner.flights).insert(hex.clone(), held);

    let mut state = Scrubber::new(1 << 20);
    assert_eq!(rig.cache.scrub_once(&mut state), ScrubPass::default());
    assert!(rig.has_file(&keys[0]));
    assert_eq!(rig.cache.held(), (400, 1));

    drop(super::locked(&rig.cache.inner.flights).remove(&hex));
    let pass = rig.cache.scrub_once(&mut state);
    assert_eq!((pass.scrubbed, pass.dropped), (1, 1));
    assert!(!rig.has_file(&keys[0]));
}

/// Asserts that a fill that is held half way (bytes in `.stage/`) is not touched by a pass that
/// drops a rotten object beside it, and that the fill ends with its object kept.
#[tokio::test]
async fn a_fill_in_progress_survives_a_pass_that_drops_another_object() {
    let rig = Rig::new("tscache-scrub-stage");
    let keys = fill(&rig, 1, 400).await;
    rot(&rig, &keys[0]);
    rig.put("cas/b/new", &pattern(2000)).await;
    let gate = rig.tamper.hold_key("cas/b/new");
    let cache = rig.cache.clone();
    let reading = tokio::spawn(async move { crate::Backend::get(&cache, "cas/b/new", None).await });
    while rig
        .cache
        .counters()
        .staging_files
        .load(std::sync::atomic::Ordering::Relaxed)
        == 0
    {
        tokio::task::yield_now().await;
    }
    let mut state = Scrubber::new(1 << 20);
    let pass = rig.cache.scrub_once(&mut state);
    assert_eq!((pass.scrubbed, pass.dropped), (1, 1));
    assert_eq!(
        read(&rig.cache.counters().staging_files),
        1,
        "the staged file is still there"
    );
    gate.add_permits(10);
    assert_eq!(reading.await.unwrap().unwrap().len(), 2000);
    assert!(rig.has_file("cas/b/new"));
}

/// Asserts that a data file that is gone is forgotten by the scrub, with nothing counted.
#[tokio::test]
async fn a_missing_data_file_is_forgotten_by_the_scrub() {
    let rig = Rig::new("tscache-scrub-missing");
    let keys = fill(&rig, 2, 200).await;
    std::fs::remove_file(rig.file(&keys[1])).unwrap();
    let mut state = Scrubber::new(1 << 20);
    let pass = rig.cache.scrub_once(&mut state);
    assert_eq!((pass.scrubbed, pass.dropped), (1, 0));
    assert_eq!(rig.cache.held(), (200, 1));
}

/// Asserts that a rotten object whose name gets another file while the scrub holds the old one
/// (a new file renamed over it) is not dropped: the new file stays, indexed, with its bytes, and
/// nothing is counted as a mismatch.
#[tokio::test]
async fn a_file_renamed_over_the_name_while_the_scrub_holds_the_old_one_is_kept() {
    let rig = Rig::new("tscache-scrub-renamed");
    let body = pattern(300);
    rig.put("cas/b/obj", &body).await;
    rig.get("cas/b/obj").await.unwrap();
    rot(&rig, "cas/b/obj");
    let hex = name_of("cas/b/obj").hex;
    let path = rig.file("cas/b/obj");
    let verdict = rig
        .cache
        .inner
        .scrub_object_between(&hex, &Scrubber::new(1 << 20), || {
            let fresh = rig.config.path.join("replacement");
            std::fs::write(&fresh, &body).unwrap();
            std::fs::rename(&fresh, &path).unwrap();
        });
    assert_eq!(verdict, Verdict::Left);
    assert_eq!(std::fs::read(&path).unwrap(), body);
    assert_eq!(rig.cache.held(), (300, 1));
    assert_eq!(read(&rig.cache.counters().scrub_mismatches), 0);
}

/// Asserts that a sidecar that cannot be read for a reason other than being absent (a directory
/// where it is: the read fails as a failing disk would) leaves a good object in place, counted in
/// `scrub_errors` and not as a mismatch.
#[tokio::test]
async fn a_sidecar_that_cannot_be_read_leaves_the_object_and_is_counted() {
    let rig = Rig::new("tscache-scrub-sidecar-io");
    let keys = fill(&rig, 1, 300).await;
    let meta = meta_path(&rig.config.path, &name_of(&keys[0]).hex);
    std::fs::remove_file(&meta).unwrap();
    std::fs::create_dir(&meta).unwrap();
    let mut state = Scrubber::new(1 << 20);
    assert_eq!(rig.cache.scrub_once(&mut state), ScrubPass::default());
    assert!(rig.has_file(&keys[0]));
    assert_eq!(rig.cache.held(), (300, 1));
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.scrub_errors), 1);
    assert_eq!(read(&counters.scrub_mismatches), 0);
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_scrub_errors_total 1\n")
    );
}

/// Asserts that an object that cannot be read for a reason other than being gone (a directory
/// where the file is, a path through a file) is left in the index and is not counted.
#[tokio::test]
async fn an_unreadable_object_is_left_alone_and_not_counted() {
    let rig = Rig::new("tscache-scrub-unreadable");
    let keys = fill(&rig, 2, 100).await;
    let root = &rig.config.path;
    let first = data_path(root, &name_of(&keys[0]).hex);
    std::fs::remove_file(&first).unwrap();
    std::fs::create_dir(&first).unwrap();
    let second = data_path(root, &name_of(&keys[1]).hex);
    let parent = second.parent().unwrap().to_path_buf();
    std::fs::rename(&parent, parent.with_extension("moved")).unwrap();
    std::fs::write(&parent, b"a file").unwrap();
    let mut state = Scrubber::new(1 << 20);
    assert_eq!(rig.cache.scrub_once(&mut state), ScrubPass::default());
    assert_eq!(rig.cache.held(), (200, 2));
    assert_eq!(read(&rig.cache.counters().scrubbed_objects), 0);
    assert_eq!(read(&rig.cache.counters().scrub_errors), 2);
}

/// Asserts the rate by counting: with a credit of 250 bytes a tick and objects of 100 bytes, the
/// ticks scrub 3, 2 and 3 objects (the third starts a new round), and the walk visits every
/// object once a round.
#[tokio::test]
async fn a_tick_scrubs_as_many_objects_as_its_credit_pays_for() {
    let rig = Rig::new("tscache-scrub-rate");
    fill(&rig, 5, 100).await;
    let mut state = Scrubber::new(250);
    let counts: Vec<usize> = (0..3)
        .map(|_| rig.cache.scrub_once(&mut state).scrubbed)
        .collect();
    assert_eq!(counts, [3, 2, 3]);
    assert_eq!(read(&rig.cache.counters().scrubbed_bytes), 800);
}

/// Asserts that an object bigger than a tick's credit is read whole and then paid for: the next
/// ticks read nothing until the debt is paid.
#[tokio::test]
async fn an_object_larger_than_the_credit_is_paid_for_by_the_ticks_after_it() {
    let rig = Rig::new("tscache-scrub-debt");
    fill(&rig, 1, 1000).await;
    let mut state = Scrubber::new(300);
    let counts: Vec<usize> = (0..4)
        .map(|_| rig.cache.scrub_once(&mut state).scrubbed)
        .collect();
    assert_eq!(counts, [1, 0, 0, 1]);
}

/// Asserts that a cache with nothing in it is a tick that does nothing.
#[tokio::test]
async fn a_tick_of_an_empty_cache_does_nothing() {
    let rig = Rig::new("tscache-scrub-empty");
    let mut state = Scrubber::new(100);
    assert_eq!(rig.cache.scrub_once(&mut state), ScrubPass::default());
}

/// Asserts that the loop does one pass for each tick it is given, that a rotten object is dropped
/// by the first, and that it ends when the ticker does.
#[tokio::test]
async fn the_loop_scrubs_once_for_each_tick() {
    let rig = Rig::new("tscache-scrub-loop");
    fill(&rig, 2, 100).await;
    rot(&rig, "cas/b/0");
    rot(&rig, "cas/b/1");
    let (ticker, tick, mut asks) = hand();
    let cache = rig.cache.clone();
    let halt = Arc::new(Notify::new());
    let running = tokio::spawn(async move { cache.scrub(Some(100), 0, ticker, halt).await });
    asks.recv().await.unwrap();
    assert_eq!(read(&rig.cache.counters().scrubbed_objects), 0);
    tick.send(()).await.unwrap();
    asks.recv().await.unwrap();
    assert_eq!(read(&rig.cache.counters().scrubbed_objects), 1);
    tick.send(()).await.unwrap();
    asks.recv().await.unwrap();
    assert_eq!(read(&rig.cache.counters().scrub_mismatches), 2);
    drop(tick);
    running.await.unwrap();
}

/// Asserts that the loop ends when it is told to halt, with the ticker waiting.
#[tokio::test]
async fn the_loop_ends_when_it_is_halted() {
    let rig = Rig::new("tscache-scrub-halt");
    let (ticker, _tick, mut asks) = hand();
    let cache = rig.cache.clone();
    let halt = Arc::new(Notify::new());
    let halting = Arc::clone(&halt);
    let running = tokio::spawn(async move { cache.scrub(Some(100), 0, ticker, halting).await });
    asks.recv().await.unwrap();
    halt.notify_one();
    running.await.unwrap();
}

/// Asserts that a scrub that is turned off runs no pass and never asks its ticker for a tick,
/// whatever the objects hold.
#[tokio::test]
async fn a_scrub_that_is_off_runs_no_pass() {
    let rig = Rig::new("tscache-scrub-off");
    fill(&rig, 1, 100).await;
    rot(&rig, "cas/b/0");
    let (ticker, tick, mut asks) = hand();
    tick.send(()).await.unwrap();
    rig.cache
        .scrub(None, 0, ticker, Arc::new(Notify::new()))
        .await;
    assert!(asks.recv().await.is_none(), "the ticker was never asked");
    assert_eq!(read(&rig.cache.counters().scrubbed_objects), 0);
    assert!(rig.has_file("cas/b/0"));
    assert_eq!(rig.cache.held(), (100, 1));
}

/// Asserts that the timer ticker ticks each period of the runtime's clock (a paused one here).
#[tokio::test(start_paused = true)]
async fn the_timer_ticker_ticks_each_period() {
    let mut every = Every::new(Duration::from_secs(1));
    assert!(every.tick().await);
    assert!(every.tick().await);
}

/// Asserts that a halt in the middle of an object ends the pass at the next chunk: a 3 MiB object
/// is read for one chunk, held there by a gate, and after the halt only that chunk was read, the
/// loop ended, and nothing was counted or dropped. No time is measured: the gate is a channel.
#[tokio::test]
async fn a_halt_in_the_middle_of_a_big_object_ends_the_pass_at_the_next_chunk() {
    let rig = Rig::with("tscache-scrub-halt-big", |config| config.budget = 8 << 20);
    fill(&rig, 1, 3 << 20).await;
    rot(&rig, "cas/b/0");
    let (reached, mut reaches) = mpsc::unbounded_channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    let chunks = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counting = Arc::clone(&chunks);
    let mut state = Scrubber::new(1 << 30);
    let stop = Arc::clone(&state.stop);
    state.on_chunk = Some(Arc::new(move || {
        counting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        reached.send(()).unwrap();
        let _ = released.lock().unwrap().recv();
    }));
    let (ticker, tick, mut asks) = hand();
    let cache = rig.cache.clone();
    let halt = Arc::new(Notify::new());
    let halting = Arc::clone(&halt);
    let running = tokio::spawn(async move { cache.scrub_with(state, ticker, halting).await });
    asks.recv().await.unwrap();
    tick.send(()).await.unwrap();
    reaches.recv().await.unwrap();
    halt.notify_one();
    // The loop sets the stop flag when it sees the halt; the gate is not opened before.
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    release.send(()).unwrap();
    running.await.unwrap();
    assert_eq!(chunks.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(read(&rig.cache.counters().scrubbed_objects), 0);
    assert!(rig.has_file("cas/b/0"));
    assert_eq!(rig.cache.held().1, 1);
}

/// Asserts that an object whose check panics is counted as a scrub error and skipped, and that
/// the objects after it are still scrubbed in the same tick.
#[tokio::test]
async fn an_object_whose_check_panics_is_counted_and_the_walk_goes_on_past_it() {
    let rig = Rig::new("tscache-scrub-panic");
    fill(&rig, 3, 100).await;
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut state = Scrubber::new(1 << 20);
    state.on_chunk = Some(Arc::new(move || {
        assert!(
            !first.swap(false, std::sync::atomic::Ordering::SeqCst),
            "the first object's check panics"
        );
    }));
    let pass = rig.cache.scrub_once(&mut state);
    assert_eq!((pass.scrubbed, pass.dropped), (2, 0));
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.scrub_errors), 1);
    assert_eq!(read(&counters.scrubbed_objects), 2);
    assert_eq!(rig.cache.held(), (300, 3));
}

/// Asserts the minimum between rounds: with two objects and a minimum of three ticks, the ticks
/// scrub 2, 0, 0, 2 (a round starts at the first tick, and the next only three ticks later).
#[tokio::test]
async fn a_round_does_not_start_again_before_the_minimum_between_rounds() {
    let rig = Rig::new("tscache-scrub-min-round");
    fill(&rig, 2, 100).await;
    let mut state = Scrubber::new(1000).with_min_round(3);
    let counts: Vec<usize> = (0..4)
        .map(|_| rig.cache.scrub_once(&mut state).scrubbed)
        .collect();
    assert_eq!(counts, [2, 0, 0, 2]);
}

/// Asserts the same through the loop with a driven ticker: one pass per tick, and no object is read
/// on the ticks between rounds.
#[tokio::test]
async fn the_loop_waits_out_the_minimum_between_rounds() {
    let rig = Rig::new("tscache-scrub-min-loop");
    fill(&rig, 1, 100).await;
    let (ticker, tick, mut asks) = hand();
    let cache = rig.cache.clone();
    let halt = Arc::new(Notify::new());
    let running = tokio::spawn(async move { cache.scrub(Some(1000), 2, ticker, halt).await });
    let mut seen = Vec::new();
    for _ in 0..3 {
        asks.recv().await.unwrap();
        tick.send(()).await.unwrap();
        asks.recv().await.unwrap();
        seen.push(read(&rig.cache.counters().scrubbed_objects));
    }
    assert_eq!(seen, [1, 1, 2]);
    drop(tick);
    running.await.unwrap();
}

/// Asserts that a delete that runs between the scrub's hash and its drop (a write-through cache
/// forgets the key) wins: the object is gone once, nothing is counted as a mismatch, and the next
/// read fills the key again.
#[tokio::test]
async fn a_forget_racing_a_scrub_of_the_same_key_is_not_a_mismatch() {
    let rig = Rig::with("tscache-scrub-forget", |config| config.write_through = true);
    let body = pattern(200);
    rig.put("cas/b/obj", &body).await;
    rig.get("cas/b/obj").await.unwrap();
    rot(&rig, "cas/b/obj");
    let hex = name_of("cas/b/obj").hex;
    let verdict = rig
        .cache
        .inner
        .scrub_object_between(&hex, &Scrubber::new(1 << 20), || {
            rig.cache.inner.forget("cas/b/obj");
        });
    assert_eq!(verdict, Verdict::Left);
    assert_eq!(rig.cache.held(), (0, 0));
    assert_eq!(read(&rig.cache.counters().scrub_mismatches), 0);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
}

/// Asserts the window of a fill that has put its object in the index and has not yet looked for
/// the mark a forget leaves: the scrub leaves the object, rotten or not, and does not clear the
/// mark, so the fill finds it and removes its own entry.
#[tokio::test]
async fn a_scrub_does_not_touch_the_object_of_a_fill_that_a_forget_marked() {
    let rig = Rig::with("tscache-scrub-marked", |config| config.write_through = true);
    let keys = fill(&rig, 1, 100).await;
    rot(&rig, &keys[0]);
    let hex = name_of(&keys[0]).hex;
    let inner = &rig.cache.inner;
    let held = futures::future::pending().boxed().shared();
    super::locked(&inner.flights).insert(hex.clone(), held);
    super::locked(&inner.invalidated).insert(hex.clone());
    let mut state = Scrubber::new(1 << 20);
    assert_eq!(rig.cache.scrub_once(&mut state), ScrubPass::default());
    assert!(rig.has_file(&keys[0]));
    assert_eq!(rig.cache.held(), (100, 1));
    assert!(inner.is_invalidated(&hex));
}

/// Asserts that a write-through put of a key whose cache entry is stale (the object was deleted
/// behind the cache's back) drops the entry, so a scrub finds nothing to drop and counts no
/// mismatch, and the next read fills the new bytes, which the next scrub keeps.
#[tokio::test]
async fn a_write_through_put_clears_a_stale_entry_so_the_scrub_has_nothing_to_drop() {
    let rig = Rig::with("tscache-scrub-put", |config| config.write_through = true);
    rig.put("cas/b/obj", &pattern(100)).await;
    rig.get("cas/b/obj").await.unwrap();
    rot(&rig, "cas/b/obj");
    rig.tamper.delete("cas/b/obj").await.unwrap();
    let fresh = pattern(150);
    rig.cache
        .put("cas/b/obj", fresh.clone().into())
        .await
        .unwrap();
    let mut state = Scrubber::new(1 << 20);
    assert_eq!(rig.cache.scrub_once(&mut state), ScrubPass::default());
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), fresh);
    let pass = rig.cache.scrub_once(&mut state);
    assert_eq!((pass.scrubbed, pass.dropped), (1, 0));
    assert_eq!(read(&rig.cache.counters().scrub_mismatches), 0);
}

/// Asserts that neither the binary's source nor the README says any more that the scrub is not
/// done.
#[test]
fn nothing_says_the_scrub_is_not_done() {
    let cli = include_str!("../cache_cli.rs");
    let readme = include_str!("../../../../../packaging/tierstore/README.md");
    for text in [cli, readme] {
        assert!(!text.contains("does not scrub"));
        assert!(!text.contains("No scrub"));
    }
}

/// Asserts that a fill that starts between the scrub's hash and its drop (its flight is in the
/// table when the drop looks) keeps the object: nothing is dropped or counted as a mismatch.
#[tokio::test]
async fn a_fill_that_starts_before_the_drop_keeps_the_object() {
    let rig = Rig::new("tscache-scrub-late-flight");
    let keys = fill(&rig, 1, 100).await;
    rot(&rig, &keys[0]);
    let hex = name_of(&keys[0]).hex;
    let inner = &rig.cache.inner;
    let verdict = inner.scrub_object_between(&hex, &Scrubber::new(1 << 20), || {
        let held = futures::future::pending().boxed().shared();
        super::locked(&inner.flights).insert(hex.clone(), held);
    });
    assert_eq!(verdict, Verdict::Left);
    assert!(rig.has_file(&keys[0]));
    assert_eq!(read(&rig.cache.counters().scrub_mismatches), 0);
}

/// Asserts that a walk that was told to stop reads nothing.
#[tokio::test]
async fn a_walk_that_is_told_to_stop_reads_nothing() {
    let rig = Rig::new("tscache-scrub-stopped");
    fill(&rig, 2, 100).await;
    let mut state = Scrubber::new(1 << 20);
    state.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(rig.cache.scrub_once(&mut state), ScrubPass::default());
    assert_eq!(read(&rig.cache.counters().scrubbed_objects), 0);
}
