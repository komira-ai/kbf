//! The orders the root part takes, over a world in a scratch directory with the hook run directly:
//! what an order finds before anything changes (a verdict that stands, an unfinished update of
//! another release, a record it cannot read), the declaration it decides on as it is at that
//! moment, and what a verdict leaves.

mod root_support;

use std::fs;
use std::sync::{Arc, Mutex, mpsc};

use kbf_step_runner::{Clock, CrashPoint};
use kbf_updater::root::RootPart;
use kbf_updater::root::socket::Answer;
use root_support::{FakeClock, SUCCEEDED, World, dir, pending_cutover_world, put, verdict};
use update_hook::{Class, Params, Phase, Verdict};

/// The step record's generation.
fn generation(world: &World) -> u64 {
    world.record("svc")["generation"].as_u64().unwrap()
}

/// Catches a committed release rolled back in the books: an update to 2.0.0 whose `cleanup` fails
/// through its retries ends `Failed(after_commit)`, from which nothing rolls back, so 2.0.0 is
/// `current` and the declaration it carried stays, with nothing kept to put back.
#[test]
fn a_failure_after_the_commit_keeps_the_release_and_its_declaration() {
    let world = World::new("after-commit");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    world.stage("svc", "1.0.0", None);
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    let narrower = active.replace("prepare = 600", "prepare = 500");
    world.stage("svc", "2.0.0", Some(&narrower));
    world.tell("svc", "cleanup", "1 {}");
    let answer = world.root_part().update("svc", "2.0.0");
    assert_eq!(
        verdict(&answer),
        Verdict::Failed {
            phase: Phase::AfterCommit,
            class: Class::Release
        }
    );
    assert_eq!(
        fs::read_link(world.path("releases/svc/current")).unwrap(),
        std::path::Path::new("2.0.0")
    );
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        narrower
    );
    assert!(!world.path("state/svc/declaration.previous").exists());
}

/// Catches a completed upgrade run again: once the update from 1.0.0 to 2.0.0 succeeded (and
/// 2.0.0 is `current`), the same order is answered with that verdict, runs no step and starts no
/// generation.
#[test]
fn the_same_order_after_an_upgrade_is_answered_with_its_verdict() {
    let world = World::new("again");
    world.declare("svc", &world.tierstore_shaped());
    world.stage("svc", "1.0.0", None);
    world.stage("svc", "2.0.0", None);
    let root = world.root_part();
    assert_eq!(verdict(&root.update("svc", "1.0.0")), SUCCEEDED);
    assert_eq!(verdict(&root.update("svc", "2.0.0")), SUCCEEDED);
    let (steps, made) = (world.steps("svc"), generation(&world));
    assert_eq!(verdict(&root.update("svc", "2.0.0")), SUCCEEDED);
    assert_eq!(world.steps("svc"), steps);
    assert_eq!(generation(&world), made);
}

/// Catches a kept declaration lost to an order that is refused: an update to 2.0.0 that installed
/// a narrower declaration is cut off after its record began; an order for 3.0.0, carrying yet
/// another, is refused as busy before anything changes, so the declaration kept for 2.0.0 is the
/// one that was active, and when 2.0.0 then fails, that one is back.
#[test]
fn an_order_refused_while_an_update_is_unfinished_changes_no_declaration() {
    let world = World::new("busy-kept");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    let first = active.replace("prepare = 600", "prepare = 500");
    world.stage("svc", "2.0.0", Some(&first));
    let killed = world
        .root_part_gated(Arc::new(|point: CrashPoint| {
            point == CrashPoint::AfterWrite(1)
        }))
        .update("svc", "2.0.0");
    assert_eq!(killed.token(), Some("runner"), "{killed:?}");
    let second = active.replace("prepare = 600", "prepare = 400");
    world.stage("svc", "3.0.0", Some(&second));
    let busy = world.root_part().update("svc", "3.0.0");
    assert_eq!(busy.token(), Some("busy"), "{busy:?}");
    let kept = world.path("state/svc/declaration.previous");
    assert_eq!(fs::read_to_string(kept).unwrap(), active);
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        first
    );
    world.tell("svc", "verify", "1 {}");
    let started = world.root_part().start("svc");
    assert!(
        matches!(started.verdict, Some(Verdict::Failed { .. })),
        "{started:?}"
    );
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        active
    );
}

/// Catches a kept declaration that cannot be read taken for none: the update that installed a
/// narrower declaration fails, and the kept one cannot be read (a directory stands at its name);
/// the start says so (`internal`), and the installed declaration and what stands at the kept name
/// stay for an operator.
#[test]
fn a_kept_declaration_that_cannot_be_read_is_an_error_not_none() {
    let world = World::new("kept-unreadable");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    let narrower = active.replace("prepare = 600", "prepare = 500");
    world.stage("svc", "2.0.0", Some(&narrower));
    let killed = world
        .root_part_gated(Arc::new(|point: CrashPoint| {
            point == CrashPoint::AfterWrite(1)
        }))
        .update("svc", "2.0.0");
    assert_eq!(killed.token(), Some("runner"), "{killed:?}");
    let kept = world.path("state/svc/declaration.previous");
    fs::remove_file(&kept).unwrap();
    dir(&kept, 0o700);
    world.tell("svc", "verify", "1 {}");
    let started = world.root_part().start("svc");
    assert_eq!(started.token(), Some("internal"), "{started:?}");
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        narrower
    );
    assert!(kept.is_dir());
}

/// Catches a decision made on a declaration the operator has since narrowed: the root part loaded
/// one with three capabilities, the operator narrowed the file to one, and a release carries one
/// with two; against the file as it is, that asks for more, so it is `Deferred` (`rights_change`)
/// and the narrowed file stays.
#[test]
fn a_carried_declaration_is_decided_on_against_the_active_file_as_it_is() {
    let world = World::new("narrowed");
    let caps = |list: &str| {
        world.declaration(
            "svc",
            "",
            &format!("user = \"root\"\ngroup = \"root\"\ncapabilities = [{list}]\n"),
        )
    };
    world.declare(
        "svc",
        &caps(r#""CAP_DAC_OVERRIDE", "CAP_NET_ADMIN", "CAP_KILL""#),
    );
    let root = world.root_part();
    let narrowed = caps(r#""CAP_DAC_OVERRIDE""#);
    world.declare("svc", &narrowed);
    world.stage(
        "svc",
        "2.0.0",
        Some(&caps(r#""CAP_DAC_OVERRIDE", "CAP_NET_ADMIN""#)),
    );
    let answer = root.update("svc", "2.0.0");
    assert!(
        matches!(answer.verdict, Some(Verdict::Deferred { .. })),
        "{answer:?}"
    );
    assert_eq!(answer.reason.as_deref(), Some("rights_change"));
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        narrowed
    );
}

/// Catches an order run past a halt, or one that changes a declaration on its way to the halt: a
/// service halted (its release changed under an unfinished update) answers an order for another
/// release, carrying another declaration, with the halt; nothing runs and nothing is installed.
#[test]
fn an_order_after_a_halt_is_answered_with_the_halt_and_changes_nothing() {
    let world = pending_cutover_world("halted");
    put(&world.path("releases/svc/2.0.0/extra"), b"x", 0o644);
    let halted = world.root_part().start("svc");
    assert_eq!(halted.verdict, Some(Verdict::Halt), "{halted:?}");
    let active = fs::read_to_string(world.path("updater.d/svc.toml")).unwrap();
    world.stage(
        "svc",
        "3.0.0",
        Some(&active.replace("prepare = 600", "prepare = 400")),
    );
    let steps = world.steps("svc");
    let answer = world.root_part().update("svc", "3.0.0");
    assert_eq!(answer.verdict, Some(Verdict::Halt), "{answer:?}");
    assert_eq!(world.steps("svc"), steps);
    assert_eq!(
        fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
        active
    );
}

/// Catches a record of a newer updater read as none, or one that cannot be parsed taken as none:
/// with either, an order and the start are refused (`record_newer`, `record`), and nothing is
/// launched, not even the boot entry of the release that is `current`.
#[test]
fn a_record_it_cannot_read_stops_orders_and_the_start() {
    let world = World::new("newer");
    world.declare("svc", &world.tierstore_shaped());
    world.stage("svc", "1.0.0", None);
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    let launched = world.launched().len();
    let record = world.path("state/svc/step-record.json");
    for (bytes, token) in [
        (&br#"{"schema": 999}"#[..], "record_newer"),
        (&b"{"[..], "record"),
    ] {
        put(&record, bytes, 0o600);
        let root = world.root_part();
        assert_eq!(root.update("svc", "1.0.0").token(), Some(token));
        assert_eq!(root.start("svc").token(), Some(token));
        assert_eq!(world.launched().len(), launched, "{token}");
    }
}

/// Catches a post-success watcher that holds every later order, or a replacement that rolls back
/// past the release that watcher committed: the update to 2.0.0 succeeded with its promotion
/// pending and the root part stopped while it watched; an order for 3.0.0 is taken (a new order
/// stops the watcher, 3.2), 2.0.0 is made `current` first, so when 3.0.0 fails its `verify` the
/// rollback is told 2.0.0 as the previous release and 2.0.0 stays `current`.
#[test]
fn an_order_while_the_watcher_runs_is_taken_and_rolls_back_to_the_committed_release() {
    let world = World::new("watching");
    world.declare("svc", &world.tierstore_shaped());
    for version in ["1.0.0", "2.0.0", "3.0.0"] {
        world.stage("svc", version, None);
    }
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    world.tell("svc", "verify", "0 {\"promotion_pending\":true}");
    let record = world.path("state/svc/step-record.json");
    let gate = move |point: CrashPoint| {
        let watching = fs::read(&record)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|rec| rec["progress"]["watching"].is_object());
        matches!(point, CrashPoint::AfterWrite(_)) && watching
    };
    let killed = world.root_part_gated(Arc::new(gate)).update("svc", "2.0.0");
    assert_eq!(killed.token(), Some("runner"), "{killed:?}");
    world.tell("svc", "verify", "1 {}");
    let answer = world.root_part().update("svc", "3.0.0");
    assert!(
        matches!(answer.verdict, Some(Verdict::Failed { .. })),
        "{answer:?}"
    );
    let seen = fs::read_to_string(world.hook_dir("svc").join("params.seen")).unwrap();
    assert_eq!(
        Params::parse(&seen).unwrap().previous_version(),
        Some("2.0.0")
    );
    assert_eq!(
        fs::read_link(world.path("releases/svc/current")).unwrap(),
        std::path::Path::new("2.0.0")
    );
}

/// What a gated clock and the updates beside it tell the test, in the order it happens.
enum Event {
    Slept(u64),
    First(Answer),
    Second(Answer),
}

/// A clock whose every sleep is told (`Event::Slept`) and lasts until the test says go.
struct GatedClock {
    fake: Arc<FakeClock>,
    told: Mutex<mpsc::Sender<Event>>,
    go: Mutex<mpsc::Receiver<()>>,
}

impl Clock for GatedClock {
    fn mono_secs(&self) -> u64 {
        self.fake.mono_secs()
    }

    fn boot_id(&self) -> String {
        self.fake.boot_id()
    }

    fn sleep_secs(&self, secs: u64) {
        let _ = self.told.lock().unwrap().send(Event::Slept(secs));
        let _ = self.go.lock().unwrap().recv();
    }
}

/// Catches a live watcher that holds the service for good, or an order let in beside a running
/// step: while the update to 2.0.0 waits out a `verify` that deferred, an order for 3.0.0 is
/// busy; once 2.0.0 is in its post-success watcher (no promotion yet), an order for 3.0.0 asks it
/// to yield and waits; the watcher stops at its next poll (2.0.0 stands, `superseded`), and 3.0.0
/// runs, from 2.0.0 as its previous release, to its verdict.
#[test]
fn a_live_watcher_yields_to_a_new_order_and_nothing_else_does() {
    let world = World::new("watch-live");
    world.declare("svc", &world.tierstore_shaped());
    for version in ["1.0.0", "2.0.0", "3.0.0"] {
        world.stage("svc", version, None);
    }
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    let (told, events) = mpsc::channel();
    let (go, gate) = mpsc::channel();
    let mut parts = world.parts(None);
    parts.clock = Arc::new(GatedClock {
        fake: FakeClock::new(),
        told: Mutex::new(told.clone()),
        go: Mutex::new(gate),
    });
    let root = Arc::new(RootPart::new(world.config(), parts).unwrap());
    let order = |version: &'static str, done: fn(Answer) -> Event| {
        let (root, told) = (Arc::clone(&root), told.clone());
        std::thread::spawn(move || told.send(done(root.update("svc", version))).unwrap())
    };
    world.tell("svc", "verify", "75 {\"retry_after_secs\":7}");
    let first = order("2.0.0", Event::First);
    assert!(matches!(events.recv().unwrap(), Event::Slept(7)));
    assert_eq!(root.update("svc", "3.0.0").token(), Some("busy"));
    world.tell("svc", "verify", "0 {\"promotion_pending\":true}");
    go.send(()).unwrap();
    assert!(matches!(events.recv().unwrap(), Event::Slept(30)));
    let second = order("3.0.0", Event::Second);
    // The watcher polls until the new order's request to yield reaches it.
    let superseded = loop {
        go.send(()).unwrap();
        match events.recv().unwrap() {
            Event::Slept(_) => {}
            Event::First(answer) => break answer,
            Event::Second(answer) => panic!("3.0.0 before 2.0.0 yielded: {answer:?}"),
        }
    };
    assert_eq!(superseded.verdict, Some(committed()), "{superseded:?}");
    assert_eq!(superseded.reason.as_deref(), Some("superseded"));
    world.tell("svc", "verify", "0 {}");
    let replaced = loop {
        match events.recv().unwrap() {
            Event::Slept(_) => go.send(()).unwrap(),
            Event::First(answer) => panic!("2.0.0 answered twice: {answer:?}"),
            Event::Second(answer) => break answer,
        }
    };
    assert_eq!(verdict(&replaced), SUCCEEDED, "{replaced:?}");
    first.join().unwrap();
    second.join().unwrap();
    let seen = fs::read_to_string(world.hook_dir("svc").join("params.seen")).unwrap();
    assert_eq!(
        Params::parse(&seen).unwrap().previous_version(),
        Some("2.0.0")
    );
}

/// The verdict of a committed update whose promotion is still pending.
fn committed() -> Verdict {
    Verdict::Succeeded {
        promotion_pending: true,
    }
}

/// Catches a carried declaration installed under which the previous release's hook could not
/// recover (its fallback `rollback`, its boot entry after a halt): one that renames the binary,
/// takes away the hook-wide capability, or shortens `rollback` is refused even approved, and
/// nothing is installed or run.
#[test]
fn a_carried_declaration_that_would_strand_the_previous_hook_is_refused_even_approved() {
    let world = World::new("strand");
    let active = world.tierstore_shaped();
    world.declare("svc", &active);
    world.stage("svc", "1.0.0", None);
    assert_eq!(
        verdict(&world.root_part().update("svc", "1.0.0")),
        SUCCEEDED
    );
    let steps = world.steps("svc");
    for (version, from, to, field) in [
        ("2.0.0", "binary = \"hook\"", "binary = \"hook2\"", "binary"),
        (
            "3.0.0",
            "capabilities = [\"CAP_NET_ADMIN\"]",
            "capabilities = []",
            "hook.capabilities",
        ),
        (
            "4.0.0",
            "rollback = 600",
            "rollback = 300",
            "hook.timeout_secs.rollback",
        ),
    ] {
        assert!(active.contains(from), "{from}");
        let changed = active.replace(from, to);
        world.approve("svc", changed.as_bytes());
        world.stage("svc", version, Some(&changed));
        let answer = world.root_part().update("svc", version);
        assert_eq!(answer.token(), Some("declaration_refused"), "{answer:?}");
        assert!(
            answer.error.as_deref().unwrap().contains(field),
            "{answer:?}"
        );
        assert_eq!(
            fs::read_to_string(world.path("updater.d/svc.toml")).unwrap(),
            active
        );
        assert_eq!(world.steps("svc"), steps);
    }
}
