//! The migration, interval, confirm and collection passes on a scratch host: the database
//! step before the swap, the minimum interval between swaps, a confirm and the collection of
//! old releases, the owed restart, the binary watch, the frontend and node floors, and the
//! refusals before the fetch.

use super::*;

/// A schema step that records what it was asked and answers as told.
struct FakeStep {
    asked: std::rc::Rc<std::cell::RefCell<Vec<u32>>>,
    answer: Result<String, String>,
}

impl crate::database::SchemaStep for FakeStep {
    fn before_start(&mut self, release_schema: u32) -> Result<String, String> {
        self.asked.borrow_mut().push(release_schema);
        self.answer.clone()
    }
}

/// Catches a release being activated before its database is ready, and a refused migration
/// being ignored. The step is asked for the schema the fetched release reports (3 here)
/// before the links move; when it refuses, the links, the restart count and the state stay
/// as they were and the status says why; an order for the release that is already current
/// (nothing staged for it) asks nothing.
///
/// Catches: the step's error dropped (the swap went through); the staged-commit
/// filter removed (the current release asked for a schema); the skip's log line removed.
#[test]
fn the_database_is_migrated_before_the_links_move_and_a_refusal_stops_the_swap() {
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut rig = Rig::new("schema-step");
    rig.helper.set_schema_step(Box::new(FakeStep {
        asked: asked.clone(),
        answer: Err("migration 3 is a contract step".to_owned()),
    }));
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 3);
    assert_eq!(
        *asked.borrow(),
        vec![3],
        "asked for the fetched release's schema"
    );
    assert_eq!(rig.state().current, Some(c(A)), "the links did not move");
    assert_eq!(rig.restarts(), 0);
    assert_eq!(rig.status()["state"], "refused");
    let why = rig.status()["refusal"].as_str().expect("text").to_owned();
    assert!(
        why.contains("state schema: migration 3 is a contract step"),
        "{why}"
    );

    rig.helper.set_schema_step(Box::new(FakeStep {
        asked: asked.clone(),
        answer: Ok("migrated".to_owned()),
    }));
    rig.order(6, Verb::Activate, B);
    rig.cycle(T0 + 6);
    assert_eq!(*asked.borrow(), vec![3, 3]);
    assert_eq!(rig.state().current, Some(c(B)));
    assert_eq!(rig.restarts(), 1);

    rig.order(7, Verb::Revert, B);
    rig.cycle(T0 + 9);
    rig.order(8, Verb::Activate, A);
    rig.cycle(T0 + 12);
    assert_eq!(
        *asked.borrow(),
        vec![3, 3],
        "a release that was not fetched for this order asks nothing"
    );
    // ... and the log says it was skipped, and why, instead of saying nothing.
    let mut log = Vec::new();
    rig.helper.drain_log(&mut log);
    let log = String::from_utf8(log).expect("text");
    let skipped = format!(
        "state schema: no migration for order 8: {} was not fetched for it (the staged release is {})",
        &A[..12],
        &B[..12]
    );
    assert!(log.contains(&skipped), "{log}");
    assert_eq!(
        log.matches("state schema: no migration").count(),
        1,
        "only the order that skipped says so: {log}"
    );
}

/// Pins the rule the activate filter really has: "the staged commit equals the order's". The
/// staged release stays staged after its swap, so a repeat order for it asks the step again
/// (the same schema, idempotent) and does not skip with the "no migration" line.
#[test]
fn a_repeat_order_for_the_staged_release_asks_the_step_again() {
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut rig = Rig::new("schema-repeat");
    rig.helper.set_schema_step(Box::new(FakeStep {
        asked: asked.clone(),
        answer: Ok("migrated".to_owned()),
    }));
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 3);
    rig.first_start(B, T0 + 4);
    rig.order(6, Verb::Confirm, B);
    rig.cycle(T0 + 6);
    assert_eq!(rig.state().current, Some(c(B)));
    assert_eq!(rig.state().pending_confirm, None);
    rig.order(7, Verb::Activate, B);
    rig.cycle(T0 + 9);
    assert_eq!(*asked.borrow(), vec![3, 3], "{}", rig.status());
    let mut log = Vec::new();
    rig.helper.drain_log(&mut log);
    let log = String::from_utf8(log).expect("text");
    assert!(!log.contains("state schema: no migration"), "{log}");
}

/// Catches two swaps in quick succession (S13): with a 60 s minimum, an activation 10 s
/// after the last one is refused before the fetch and moves nothing, and one 61 s after it
/// goes through.
///
/// Catches: the interval check removed (the second swap goes through at +10);
/// `last_activation_at` never recorded (same); `<` turned into `>` (the +61 swap refused).
#[test]
fn a_second_activation_inside_the_minimum_interval_is_refused() {
    let mut rig = Rig::build("interval", Platform::Linux, 60, false);
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    rig.first_start(B, T0 + 1);
    rig.order(6, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.state().pending_confirm, None);
    let fetched = rig.fetcher.count();

    rig.put(&rig.rec(7, 1, 2, Verb::Activate, C));
    rig.cycle(T0 + 10);
    assert_eq!(rig.fetcher.count(), fetched, "nothing fetched");
    assert_eq!(rig.current(), Some(c(B)));
    assert!(
        rig.refusal().contains("at least 60 s between activations"),
        "{}",
        rig.refusal()
    );

    rig.put(&rig.rec(8, 1, 3, Verb::Activate, C));
    rig.cycle(T0 + 61);
    assert_eq!(rig.current(), Some(c(C)));
}

/// Catches a clock that stepped back holding an activation up: the last swap is "in the
/// future" of the pass, and the order is not held by the minimum interval.
///
/// Catches: the `last <= now` test dropped (the future swap holds every order for
/// the difference).
#[test]
fn a_clock_that_stepped_back_does_not_hold_an_activation_up() {
    let mut rig = Rig::build("interval-back", Platform::Linux, 60, false);
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 100);
    rig.first_start(B, T0 + 101);
    rig.order(6, Verb::Confirm, B);
    rig.cycle(T0 + 103);
    assert_eq!(rig.state().pending_confirm, None);
    rig.put(&rig.rec(7, 1, 2, Verb::Activate, C));
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(C)));
}

/// Catches a confirm kept on the service user's word alone: a confirm order is refused
/// while the unit's main process still runs the old release, and while the first boot
/// check has not run; it is kept once the new release runs.
///
/// Catches: `accept_confirm` not called; the executable compared with the old
/// release's directory; the boot-check condition dropped (the not-started confirm is kept).
#[test]
fn a_confirm_order_is_kept_only_when_the_new_release_runs() {
    let mut rig = Rig::new("confirm-accept");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);

    // Not started yet: no boot check has run.
    rig.order(6, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert!(rig.state().pending_confirm.is_some());
    assert!(
        rig.refusal().contains("first boot check has not run"),
        "{}",
        rig.refusal()
    );

    // Started by the boot check, but the main process still runs the old release.
    let layout = rig.layout.clone();
    let mut out = Vec::new();
    activate::boot_check(rig.helper.exec_mut(), &layout, T0 + 4, 600, &mut out).expect("boot");
    rig.order(7, Verb::Confirm, B);
    rig.cycle(T0 + 6);
    assert!(rig.state().pending_confirm.is_some());
    assert!(rig.refusal().contains("not a file in"), "{}", rig.refusal());

    // The unit is not active.
    rig.service.0.borrow_mut().active = "failed".to_owned();
    rig.order(8, Verb::Confirm, B);
    rig.cycle(T0 + 9);
    assert!(rig.state().pending_confirm.is_some());
    assert!(rig.refusal().contains("not active"), "{}", rig.refusal());

    // The new release runs.
    rig.service.0.borrow_mut().active = "active".to_owned();
    rig.first_start(B, T0 + 10);
    rig.order(9, Verb::Confirm, B);
    rig.cycle(T0 + 12);
    assert_eq!(rig.state().pending_confirm, None);
}

/// Catches `gc` removing what must stay: after a confirm the releases nothing needs go,
/// and current, previous, the four newest others and every release a process executes from
/// stay. A confirm that is refused collects nothing.
///
/// Catches: the in-use set ignored (the oldest release, run by a process, is
/// removed); `gc_due` set by a refused confirm; previous not kept; a keep below four.
#[test]
fn a_confirm_collects_old_releases_but_not_one_a_process_runs() {
    let mut rig = Rig::new("gc-after-confirm");
    let others = [
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
        "3333333333333333333333333333333333333333",
        "4444444444444444444444444444444444444444",
        "5555555555555555555555555555555555555555",
        "6666666666666666666666666666666666666666",
        "7777777777777777777777777777777777777777",
    ];
    let releases = rig.root.join("var/lib/kbf-installer/releases");
    for (i, o) in others.into_iter().enumerate() {
        rig.install(o);
        // The first is the oldest by far, and the rest follow in order.
        fs::File::open(releases.join(o))
            .expect("dir")
            .set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000 + 10 * i as u64),
            )
            .expect("mtime");
    }
    // A stray process runs the oldest.
    let old = releases.join(others[0]);
    rig.service.0.borrow_mut().exes.push((77, old.join("kbf")));

    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    let there = |c: &str| releases.join(c).exists();
    assert!(there(others[1]), "nothing is collected before the confirm");

    rig.first_start(B, T0 + 1);
    rig.service.0.borrow_mut().exes.push((77, old.join("kbf")));
    rig.order(6, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert!(there(A) && there(B), "previous and current stay");
    assert!(there(others[0]), "a release a process runs stays");
    assert!(
        !there(others[1]) && !there(others[2]),
        "the two oldest others go"
    );
    assert!(others[3..].iter().all(|o| there(o)), "the four newest stay");
}

/// Catches a collection that cannot run ending the pass or refusing the confirm: when the
/// process table cannot be read, and when the releases directory cannot be listed, the
/// confirm is still kept, nothing is removed, and the reason is in the log.
///
/// Catches: the `in_use` error treated as an empty set (everything unneeded goes);
/// the collection's error returned from the pass.
#[test]
fn a_collection_that_cannot_run_is_logged_and_the_confirm_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    for (name, why) in [
        ("blind", "cannot tell which releases run"),
        ("unlistable", "gc: "),
    ] {
        let mut rig = Rig::new(&format!("gc-fails-{name}"));
        rig.install("1111111111111111111111111111111111111111");
        rig.order(5, Verb::Activate, B);
        rig.cycle(T0);
        rig.first_start(B, T0 + 1);
        let releases = rig.root.join("var/lib/kbf-installer/releases");
        if name == "blind" {
            rig.service.0.borrow_mut().blind = true;
        } else {
            fs::set_permissions(&releases, fs::Permissions::from_mode(0o100)).expect("close");
        }
        rig.order(6, Verb::Confirm, B);
        rig.cycle(T0 + 3);
        fs::set_permissions(&releases, fs::Permissions::from_mode(0o755)).expect("open");
        let mut log = Vec::new();
        rig.helper.drain_log(&mut log);
        let log = String::from_utf8_lossy(&log).into_owned();
        assert!(log.contains(why), "{name}: {log}");
        assert_eq!(
            rig.state().pending_confirm,
            None,
            "{name}: the confirm is kept"
        );
        assert!(
            releases
                .join("1111111111111111111111111111111111111111")
                .exists(),
            "{name}: nothing was removed"
        );
    }
}

/// Catches a restart owed by an operator verb being lost, or done under that verb's lock: the
/// marker is there and a verb holds the lock when the pass starts, so the pass first leaves
/// the restart; once the verb lets go the same pass restarts the service once and removes the
/// marker. A lock file that cannot be opened ends the pass with the error.
///
/// Catches: the busy arm restarting anyway; the marker left after the restart.
#[test]
fn a_restart_an_operator_left_owed_waits_for_its_lock_then_is_done_and_cleared() {
    let mut rig = Rig::new("owed-marker");
    let marker = rig
        .layout
        .join(&crate::helper::restart::owed_marker(&rig.layout));
    fs::write(&marker, b"owed\n").expect("marker");
    let held = Locked::acquire(&rig.layout).expect("a verb holds the lock");
    let verb = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(held);
    });
    rig.cycle(T0);
    verb.join().expect("the verb ends");
    assert_eq!(rig.restarts(), 1, "done once the verb let go");
    assert!(!marker.exists(), "and cleared");

    let mut rig = Rig::new("owed-marker-no-lock");
    let marker = rig
        .layout
        .join(&crate::helper::restart::owed_marker(&rig.layout));
    fs::write(&marker, b"owed\n").expect("marker");
    let lock_dir = rig.layout.join(&rig.layout.lock());
    fs::remove_dir_all(lock_dir.parent().expect("a directory")).expect("no lock directory");
    assert!(rig.helper.cycle(T0).is_err());
}

struct Flag(std::rc::Rc<std::cell::Cell<bool>>);

impl crate::helper::serve::BinaryWatch for Flag {
    fn replaced(&self) -> bool {
        self.0.get()
    }
}

/// Catches a replaced program going unnoticed and a pass made by the old code after it:
/// while the file is the one that started, a turn makes a pass and acts on the order; once
/// replaced, the turn says re-exec and the next order is left in the spool for the new
/// program.
///
/// Catches: the watch consulted after the pass (the order is acted on);
/// `Flow::Reexec` never returned.
#[test]
fn a_replaced_binary_is_re_executed_before_the_next_order_is_read() {
    use crate::helper::serve::{Flow, Serve};
    let rig = Rig::new("serve");
    let replaced = std::rc::Rc::new(std::cell::Cell::new(false));
    rig.order(5, Verb::Activate, B);
    let orders = rig.orders();
    let layout = rig.layout.clone();
    let Rig { helper, .. } = rig;
    let mut serve = Serve::new(helper, Box::new(Flag(replaced.clone())));
    assert_eq!(serve.turn(T0).expect("a pass"), Flow::Continue);
    assert_eq!(fs::read_dir(&orders).expect("dir").count(), 0, "acted on");
    let first = crate::state::read_unlocked(&layout)
        .expect("reads")
        .expect("there");
    assert_eq!(first.current, Some(c(B)));

    replaced.set(true);
    fs::write(
        orders.join(kbf_update::record::file_name(1, 6)),
        serde_json::to_vec(&record(6, 1, 2, Verb::Confirm, B, Platform::Linux)).expect("json"),
    )
    .expect("record");
    assert_eq!(serve.turn(T0 + 5).expect("no pass"), Flow::Reexec);
    assert_eq!(
        fs::read_dir(&orders).expect("dir").count(),
        1,
        "left for the new program"
    );
}

/// Catches a `frontend_update` taken on a host that runs no frontend (KI section 6: only a
/// frontend or all-in-one host accepts it), or moving a floor: the record parses (it is a
/// locator: no components, no facts), is refused in status.json with the word `denied`,
/// changes nothing, is removed, and does not advance the acknowledged id or epoch, so a real
/// order of a lower id in the same epoch still runs.
///
/// Catches: the `denied` arm in `take` removed (the order is staged); the arm placed
/// after the floors with the floor moved (the lower real order is then dropped as a replay).
#[test]
fn a_frontend_update_record_on_a_host_without_a_frontend_is_denied_and_moves_no_floor() {
    let mut rig = Rig::new("frontend-update");
    let r = rig.rec(9, 1, 1, Verb::FrontendUpdate, B);
    assert!(r.components.is_empty(), "a locator");
    assert!(r.expected.is_none(), "a locator");
    rig.put(&r);
    rig.cycle(T0);
    assert_eq!(rig.status()["state"], "refused");
    assert!(rig.refusal().starts_with("denied: "), "{}", rig.refusal());
    assert_eq!(rig.status()["acked_order_id"], 0, "the floor did not move");
    assert_eq!(rig.current(), Some(c(A)));
    assert_eq!(rig.state().staged, None);
    assert_eq!(rig.fetcher.count(), 0);
    assert_eq!(rig.restarts(), 0);
    assert_eq!(rig.left(), 0);
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 3);
    assert_eq!(
        rig.current(),
        Some(c(B)),
        "order 5 is below 9 but 9 was never taken"
    );
}

/// Catches the two kinds of order sharing a floor. On a frontend host the node floor is at
/// (1, 7) (an activation); a frontend record of the same epoch and a lower id is not a replay
/// of it (it is staged), and with the frontend floor seeded at (1, 7) a node order of id 3 is
/// not dropped as a replay of the frontend's: each kind is ordered against its own pair.
///
/// Catches: the floor taken from the node pair whatever the kind (the frontend
/// record is dropped silently, and the node order after the seeded frontend floor too).
#[test]
fn node_and_frontend_orders_have_separate_floors() {
    let mut rig = Rig::frontend("floors-per-kind");
    rig.order(7, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.status()["acked_order_id"], 7);
    rig.put(&rig.rec(3, 1, 1, Verb::FrontendUpdate, C));
    rig.cycle(T0 + 3);
    assert_eq!(
        rig.state().staged,
        Some(c(C)),
        "a lower id of another kind is not a replay: {}",
        rig.refusal()
    );

    let mut rig = Rig::new("floors-per-kind-reverse");
    let path = rig.root.join("var/lib/kbf-installer/helper.json");
    let mut memory: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("memory")).expect("json");
    memory["frontend_epoch"] = 1.into();
    memory["frontend_order_id"] = 7.into();
    fs::write(&path, serde_json::to_vec(&memory).expect("json")).expect("seed");
    rig.order(3, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(
        rig.current(),
        Some(c(B)),
        "the node order is above the node floor"
    );
    assert_eq!(rig.status()["acked_order_id"], 3);
}

/// Catches a spool that cannot be listed being read as an empty one (orders silently never
/// acted on), and a crowded spool going unsaid: a directory descriptor that cannot be read
/// is a refusal with the entry's name and the stable code, and a spool of more than 256
/// entries says so (the first 256 are looked at, the rest are not).
///
/// Catches: the listing error ignored (no refusal is recorded); the `crowded` flag
/// dropped (the spool of 257 junk files says nothing).
#[test]
fn a_spool_that_cannot_be_listed_is_refused_and_a_crowded_one_is_said() {
    let mut rig = Rig::new("scan");
    let file = rig.root.join("not-a-dir");
    fs::write(&file, b"x").expect("file");
    let mut memory = Memory::default();
    rig.helper
        .orders_in(&Entry::over_file(&file), T0, &mut memory)
        .expect("a pass");
    assert_eq!(memory.event, Event::Refused);
    assert!(
        memory.refusal.contains("not-a-directory"),
        "{}",
        memory.refusal
    );
    for n in 0..257 {
        fs::write(rig.orders().join(format!("junk-{n}")), b"").expect("junk");
    }
    rig.cycle(T0);
    assert!(
        rig.refusal().contains("more than 256 entries"),
        "{}",
        rig.refusal()
    );
}

fn plain_order(verb: OrderVerb) -> Order {
    Order {
        id: 1,
        epoch: 1,
        generation: 1,
        verb,
        commit: c(B),
        window_s: 600,
        claims: None,
        source: Source::Fetch,
    }
}

/// Catches the two defences that the parse already makes being relied on alone: an activation
/// with no release claims is refused by the vetting itself (fail closed), and a
/// `frontend_update` that reached the act step is refused, not carried out as a node verb.
///
/// Catches: the claims check removed (the vetting panics or passes); the frontend arm
/// of `act` made a revert (the links move).
#[test]
fn vetting_and_acting_refuse_what_parsing_already_refuses() {
    let mut rig = Rig::new("defence");
    let mut memory = Memory::default();
    let before = rig.state();
    let why = rig
        .helper
        .prepare(&plain_order(OrderVerb::Activate), &before, T0, &mut memory)
        .unwrap_err();
    assert!(why.contains("no release claims"), "{why}");
    rig.helper
        .act(&plain_order(OrderVerb::FrontendUpdate), T0, &mut memory)
        .expect("refused, not failed");
    assert_eq!(memory.event, Event::Refused);
    assert!(
        memory.refusal.contains("not a node order"),
        "{}",
        memory.refusal
    );
    assert_eq!(rig.current(), Some(c(A)));
}

/// Catches an activation naming a component set the platform does not take: a Linux host takes
/// exactly `kbf`; a second component is refused before the fetch, with both names in the
/// reason.
///
/// Catches: the set compared as a prefix (an extra component passes).
#[test]
fn an_activation_with_other_components_than_the_platform_takes_is_refused() {
    let mut rig = Rig::new("components");
    let mut r = rig.rec(1, 1, 1, Verb::Activate, B);
    let mut extra = r.components[0].clone();
    extra.name = "kbf-daemon".to_owned();
    r.components.push(extra);
    rig.put(&r);
    rig.cycle(T0);
    assert_eq!(rig.fetcher.count(), 0);
    assert!(
        rig.refusal().contains("names components"),
        "{}",
        rig.refusal()
    );
}

/// A fetcher that reports a good release and installs nothing, as one whose directory was
/// removed between the fetch and the swap.
pub(super) struct Hollow;

impl Fetcher for Hollow {
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        Ok(crate::helper::fetcher::fake::honest(rec))
    }

    fn fetch_activation(
        &self,
        rec: &ReleaseLocator,
        _expected: &Facts,
    ) -> Result<InstalledRelease, FetchError> {
        self.fetch(rec)
    }
}

/// Catches an activation that fails at the swap (the release is not on the host after all)
/// being reported as done: the order is refused with the installer's reason, the links stay
/// where they were, and no restart is owed. Also the fetcher of a build that has none refuses
/// every fetch.
///
/// Catches: the swap error turned into `Ok(Done::quiet(..))` (the order is
/// acknowledged without a swap); `NotWired` returning an empty release.
#[test]
fn a_swap_that_fails_is_refused_and_moves_nothing() {
    let mut rig = Rig::new("hollow");
    rig.helper.set_fetcher(Box::new(Hollow));
    rig.order(1, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.status()["state"], "refused");
    assert_eq!(rig.current(), Some(c(A)));
    assert_eq!(rig.restarts(), 0);
    let loc = ReleaseLocator {
        commit: c(B),
        components: Vec::new(),
        source: Source::Fetch,
    };
    assert!(matches!(NotWired.fetch(&loc), Err(FetchError::NotWired)));
    assert!(matches!(NotWired.seed(), Err(FetchError::NotWired)));
}

/// Catches an activation that goes on to fetch while the node cannot be told a fetch is under
/// way (status.json cannot be written): the order is refused before the fetch, nothing moves,
/// and the pass reports the failure to write instead of ending as if it had reported.
///
/// Catches: the report error ignored in the vetting (the fetch happens anyway).
#[test]
fn an_activation_stops_before_the_fetch_when_status_cannot_be_written() {
    let mut rig = Rig::new("status-unwritable");
    fs::remove_file(rig.root.join(UPDATE).join("status.json")).expect("status.json");
    fs::create_dir(rig.root.join(UPDATE).join("status.json")).expect("a directory in its place");
    rig.order(1, Verb::Activate, B);
    assert!(rig.helper.cycle(T0).is_err());
    assert_eq!(rig.fetcher.count(), 0);
    assert_eq!(rig.current(), Some(c(A)));
}

/// Catches the second timer: an activation whose swap is on trial may come again (the pass that
/// made the swap died before its acknowledgement was saved), and it is the same activation, not
/// a refusal for a trial pending and not a second trial. The vetting lets it by; `activate`
/// calls it a repeat.
///
/// Catches: the pending trial refused whatever its activation (the repeat is refused).
#[test]
fn the_same_activation_again_while_on_trial_is_a_repeat() {
    let mut rig = Rig::new("repeat");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(B)));
    let trial = rig.state().pending_confirm.expect("on trial");
    assert_eq!(trial.activation, "1-5");
    let path = rig.root.join("var/lib/kbf-installer/helper.json");
    let mut memory: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("memory")).expect("json");
    memory["acked_order_id"] = 0.into();
    memory["acked_epoch"] = 0.into();
    fs::write(&path, serde_json::to_vec(&memory).expect("json")).expect("seed");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.current(), Some(c(B)));
    assert_eq!(rig.state().pending_confirm, Some(trial));
    assert_eq!(rig.left(), 0);
}
