//! What a host keeps across many updates: current, previous, the pinned releases, the
//! coordinator's last good one and the newest `keep` others, and nothing pruned while a swap
//! is on trial. Children of the release tests, which hold the scratch hosts.

use std::collections::BTreeSet;
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::retain::{self, RetainError, Retained};

/// The Linux host and the macOS host of the release tests.
fn platforms(name: &str) -> Vec<(ScratchHost, Layout)> {
    vec![host(&format!("ret-{name}")), mac(&format!("ret-{name}"))]
}

fn id(i: u64) -> String {
    format!("{i:040x}")
}

fn releases_dir(root: &Layout) -> PathBuf {
    root.join(&root.releases())
}

/// The releases on the host, by commit.
fn held(root: &Layout) -> BTreeSet<String> {
    fs::read_dir(releases_dir(root))
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.len() == 40)
        .collect()
}

fn ids(range: impl IntoIterator<Item = u64>) -> BTreeSet<String> {
    range.into_iter().map(id).collect()
}

/// The `n`th update as the helper does one: install release `n` (its directory dated `n`
/// seconds after the epoch, so the order is not the clock's), record its version as the
/// stage does, then collect after the confirm.
fn update(tag: &str, exec: &mut ScratchHost, root: &Layout, n: u64, keep: usize) -> String {
    let from = source(&format!("{tag}-{n}"), &format!("kbf {n}"), None);
    install_on(exec, root, &id(n), &from, Mode::Apply).expect("install");
    fs::File::open(releases_dir(root).join(id(n)))
        .expect("open")
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_000 + n))
        .expect("mtime");
    Retained::update(&retain::path(root), |r| {
        r.versions.insert(c(&id(n)), format!("v{n}"));
    })
    .expect("version");
    gc_on(exec, root, keep).expect("gc")
}

fn retained(root: &Layout) -> Retained {
    Retained::read(&retain::path(root)).expect("retain.json")
}

fn edit(root: &Layout, change: impl FnOnce(&mut Retained)) {
    Retained::update(&retain::path(root), change).expect("retain.json");
}

/// Catches a host that keeps too many or too few releases across many updates: after each of
/// N+3 updates the host holds exactly the newest N+2 (current, previous and the N newest
/// others), on each platform, with N the default four and with N the floor of two.
///
/// Catches: the surplus off by one in either direction; the newest others picked oldest
/// first; `previous` or `current` pruned.
#[test]
fn after_many_updates_a_host_holds_current_previous_and_the_newest_keep_others() {
    for keep in [retain::DEFAULT_KEEP, retain::MIN_KEEP] {
        for (mut exec, root) in platforms(&format!("many-{keep}")) {
            for n in 1..=(keep as u64 + 3) {
                update(&format!("many-{keep}"), &mut exec, &root, n, keep);
                let from = n.saturating_sub(keep as u64 + 1).max(1);
                assert_eq!(
                    held(&root),
                    ids(from..=n),
                    "keep {keep}, after update {n}: the newest {} releases",
                    keep + 2
                );
                assert_eq!(state_of(&root).current, Some(c(&id(n))));
            }
            let state = state_of(&root);
            assert_eq!(state.previous, Some(c(&id(keep as u64 + 2))));
        }
    }
}

/// Catches a pinned release or the coordinator's last good one being pruned by later updates,
/// and one that is never let go: the pinned release 1 and the last good release 2 survive
/// every update, the versions of the others pruned are forgotten, and once the pin and the
/// last good are cleared the next collection removes them.
///
/// Catches: `protected` left out of the kept set of `gc`; the versions of removed releases
/// not dropped; a pin that outlives its record.
#[test]
fn a_pinned_and_a_last_good_release_survive_every_update_until_they_are_let_go() {
    for (mut exec, root) in platforms("pinned") {
        update("pinned", &mut exec, &root, 1, 2);
        update("pinned", &mut exec, &root, 2, 2);
        edit(&root, |r| {
            r.pinned.insert(c(&id(1)));
            r.last_good = Some(c(&id(2)));
        });
        for n in 3..=10 {
            update("pinned", &mut exec, &root, n, 2);
            let mut want = ids(n.saturating_sub(3).max(1)..=n);
            want.extend(ids([1, 2]));
            assert_eq!(held(&root), want, "after update {n}");
        }
        // The pins are still on file, and so are the versions of what is held, and only those.
        let now = retained(&root);
        assert_eq!(now.pinned, [c(&id(1))].into());
        assert_eq!(now.last_good, Some(c(&id(2))));
        let versions: BTreeSet<String> = now.versions.keys().map(ToString::to_string).collect();
        assert_eq!(versions, held(&root));
        assert_eq!(now.versions[&c(&id(1))], "v1");

        // Unpinned, release 1 goes at the next collection; 2 is still the last good.
        edit(&root, |r| r.pinned.clear());
        gc_on(&mut exec, &root, 2).expect("gc");
        let mut want = ids(7..=10);
        want.extend(ids([2]));
        assert_eq!(held(&root), want);
        assert!(!retained(&root).versions.contains_key(&c(&id(1))));

        // A new last good lets the old one go.
        edit(&root, |r| r.last_good = Some(c(&id(9))));
        gc_on(&mut exec, &root, 2).expect("gc");
        assert_eq!(held(&root), ids(7..=10));
        edit(&root, |r| r.last_good = None);
        gc_on(&mut exec, &root, 2).expect("gc");
        assert_eq!(held(&root), ids(7..=10), "nothing more to let go");
    }
}

/// Catches a collection while a swap is on trial: with `pending_confirm` set every release
/// stays (the one on trial, the one it came from, all the surplus), the stage directory a
/// killed install left stays too, `retain.json` is not touched, and the output says why; once
/// the swap is confirmed the surplus and the stage directory go.
///
/// Catches: the pending check removed from `gc` (the surplus and the stage directory go);
/// `retain.json` rewritten.
#[test]
fn nothing_is_pruned_while_a_swap_is_on_trial_and_the_next_collection_prunes() {
    for (mut exec, root) in platforms("trial") {
        // Eight updates with a huge keep: nothing goes yet.
        for n in 1..=8 {
            update("trial", &mut exec, &root, n, 100);
        }
        assert_eq!(held(&root), ids(1..=8));
        let stage = releases_dir(&root).join(format!(".stage-{}.abc", id(9)));
        fs::create_dir(&stage).expect("stage");
        fs::write(stage.join("kbf"), "half").expect("half");

        let mut state = state_of(&root);
        state.pending_confirm = Some(PendingConfirm {
            commit: c(&id(8)),
            previous: Some(c(&id(7))),
            activation: "1-1".to_owned(),
            deadline_wall: Some(1),
            deadline_mono_s: None,
            starts_left: None,
        });
        Locked::acquire(&root)
            .expect("lock")
            .write(&state)
            .expect("state");
        let on_file = fs::read(retain::path(&root)).expect("retain.json");

        for keep in [0, 2, 4] {
            let out = gc_on(&mut exec, &root, keep).expect("gc on trial");
            assert_eq!(
                out,
                format!(
                    "removing nothing: {} is on trial (confirm it or let it go back first)\n",
                    id(8)
                )
            );
            assert_eq!(held(&root), ids(1..=8), "keep {keep}");
            assert!(stage.exists(), "the stage sweep waits too");
        }
        let mut dry = Vec::new();
        gc(
            &mut exec,
            &root,
            2,
            &BTreeSet::new(),
            Mode::DryRun,
            &mut dry,
        )
        .expect("dry");
        assert!(
            String::from_utf8(dry)
                .expect("utf-8")
                .starts_with("removing nothing")
        );
        assert_eq!(fs::read(retain::path(&root)).expect("same"), on_file);

        // The confirm: no trial, and the next collection prunes.
        state.pending_confirm = None;
        Locked::acquire(&root)
            .expect("lock")
            .write(&state)
            .expect("state");
        let out = gc_on(&mut exec, &root, 2).expect("gc after the confirm");
        assert!(out.contains("and the 2 newest others; removing: "), "{out}");
        assert_eq!(held(&root), ids(5..=8));
        assert!(!stage.exists());
    }
}

/// Catches a count below the floor being honoured by `gc` itself (a caller other than the
/// command line, such as the helper or a test, passes any count): 0 and 1 are raised to 2,
/// 3 is kept as asked, and the output names the count used.
///
/// Catches: `at_least` not applied in `gc`.
#[test]
fn a_keep_below_the_floor_is_raised_inside_gc() {
    for asked in [0, 1] {
        let (mut exec, root) = host(&format!("ret-floor-{asked}"));
        for n in 1..=6 {
            update("floor", &mut exec, &root, n, 100);
        }
        let out = gc_on(&mut exec, &root, asked).expect("gc");
        assert!(
            out.contains(&format!(
                "and the 2 newest others; removing: {} {}\n",
                id(2),
                id(1)
            )),
            "{out}"
        );
        assert_eq!(
            held(&root),
            ids(3..=6),
            "asked {asked}: current, previous and two"
        );
    }
    let (mut exec, root) = host("ret-floor-three");
    for n in 1..=6 {
        update("floor3", &mut exec, &root, n, 100);
    }
    gc_on(&mut exec, &root, 3).expect("gc");
    assert_eq!(held(&root), ids(2..=6));
}

/// Catches a `retain.json` that cannot be understood letting a collection go on: it fails
/// with the file named, removes no release and no stage directory, and leaves the file as it
/// was; the same for a `retain.json` that cannot be read.
///
/// Catches: the file read after the sweep; a parse error treated as empty; the error
/// swallowed.
#[test]
fn a_retain_file_that_cannot_be_parsed_or_read_stops_the_collection_and_removes_nothing() {
    for (mut exec, root) in platforms("garbled") {
        for n in 1..=6 {
            update("garbled", &mut exec, &root, n, 100);
        }
        let stage = releases_dir(&root).join(format!(".stage-{}.abc", id(9)));
        fs::create_dir(&stage).expect("stage");
        let file = retain::path(&root);
        fs::write(&file, r#"{"pinned": ["#).expect("garble");

        let err = gc_on(&mut exec, &root, 2).expect_err("garbled");
        assert!(
            matches!(err, ReleaseError::Retain(RetainError::Parse { .. })),
            "{err:?}"
        );
        assert!(err.to_string().contains("nothing is removed"), "{err}");
        assert!(
            err.to_string().contains(&file.display().to_string()),
            "{err}"
        );
        assert_eq!(held(&root), ids(1..=6));
        assert!(stage.exists());
        assert_eq!(fs::read_to_string(&file).expect("same"), r#"{"pinned": ["#);

        fs::remove_file(&file).expect("rm");
        fs::create_dir(&file).expect("a directory");
        let err = gc_on(&mut exec, &root, 2).expect_err("unreadable");
        assert!(
            matches!(err, ReleaseError::Retain(RetainError::Read { .. })),
            "{err:?}"
        );
        assert_eq!(held(&root), ids(1..=6));
        assert!(stage.exists());
    }
}

/// Catches a file with a key from a newer writer making `gc` refuse: the key is ignored and
/// the pin beside it is honoured.
///
/// Catches: `deny_unknown_fields` on the file.
#[test]
fn a_key_from_another_release_does_not_stop_a_collection_and_a_pin_beside_it_holds() {
    let (mut exec, root) = host("ret-unknown-key");
    for n in 1..=6 {
        update("unknown", &mut exec, &root, n, 100);
    }
    fs::write(
        retain::path(&root),
        format!(
            r#"{{"pinned": ["{}"], "a_key_of_a_later_release": [1, 2]}}"#,
            id(1)
        ),
    )
    .expect("seed");
    gc_on(&mut exec, &root, 2).expect("gc");
    assert_eq!(held(&root), ids([1, 3, 4, 5, 6]));
}

/// Catches a dry run that writes `retain.json`, and a collection that rewrites it when
/// nothing went: a dry run that would remove a release leaves the file's bytes and the
/// releases as they were; a collection with nothing to remove leaves the bytes as they were;
/// one that removes drops the versions of what it removed and writes the file whole.
///
/// Catches: the write done in `Mode::DryRun`; the write done when nothing was removed.
#[test]
fn a_dry_run_and_a_collection_that_removes_nothing_leave_retain_json_as_it_was() {
    let (mut exec, root) = host("ret-dry");
    for n in 1..=6 {
        update("dry", &mut exec, &root, n, 100);
    }
    // Written by hand, so a rewrite shows in its bytes.
    let by_hand = format!(r#"{{"versions":{{"{}":"v1","{}":"v6"}}}}"#, id(1), id(6));
    let file = retain::path(&root);
    fs::write(&file, &by_hand).expect("seed");

    let mut out = Vec::new();
    gc(
        &mut exec,
        &root,
        2,
        &BTreeSet::new(),
        Mode::DryRun,
        &mut out,
    )
    .expect("dry");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(
        out.contains("DRY-RUN: remove ") && out.contains(&id(1)),
        "the dry run names what it would remove: {out}"
    );
    assert_eq!(fs::read_to_string(&file).expect("same"), by_hand);
    assert_eq!(held(&root), ids(1..=6));

    // Nothing to remove with a keep that covers it all.
    gc_on(&mut exec, &root, 10).expect("gc");
    assert_eq!(fs::read_to_string(&file).expect("same"), by_hand);

    // A removal drops the version of what went and keeps the rest.
    gc_on(&mut exec, &root, 2).expect("gc");
    assert_eq!(held(&root), ids(3..=6));
    let now = retained(&root);
    assert_eq!(
        now.versions,
        [(c(&id(6)), "v6".to_owned())].into_iter().collect()
    );
}

/// Catches the pinned release being forgotten when the file has no version for it, and a
/// last good named in the file that is not on the host breaking the collection: a pin or a
/// last good with no directory is only a name, and the others are collected as usual.
///
/// Catches: `gc` failing on a protected commit it does not hold.
#[test]
fn a_pin_or_a_last_good_that_is_not_on_the_host_is_only_a_name() {
    let (mut exec, root) = host("ret-absent");
    for n in 1..=6 {
        update("absent", &mut exec, &root, n, 100);
    }
    edit(&root, |r| {
        r.pinned.insert(c(&id(77)));
        r.last_good = Some(c(&id(78)));
    });
    gc_on(&mut exec, &root, 2).expect("gc");
    assert_eq!(held(&root), ids(3..=6));
    let now = retained(&root);
    assert_eq!(now.pinned, [c(&id(77))].into());
    assert_eq!(now.last_good, Some(c(&id(78))));
}

/// Catches a `kept` listing that is wrong about what it holds: each release with its commit,
/// version and reasons, newest first, the one on trial named as such, and a failing
/// `retain.json` an error rather than an empty list.
///
/// Catches: `kept` reading the wrong state field; the versions not joined; the error
/// swallowed.
#[test]
fn the_kept_listing_names_each_release_with_its_version_and_why() {
    for (mut exec, root) in platforms("kept") {
        for n in 1..=5 {
            update("kept", &mut exec, &root, n, 100);
        }
        edit(&root, |r| {
            r.pinned.insert(c(&id(1)));
            r.last_good = Some(c(&id(3)));
            r.versions.remove(&c(&id(2)));
        });
        let listed = kept(&root).expect("listing");
        let shape: Vec<_> = listed
            .iter()
            .map(|k| (k.commit.to_string(), k.version.clone(), k.why.clone()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (id(5), Some("v5".to_owned()), vec!["current"]),
                (id(4), Some("v4".to_owned()), vec!["previous"]),
                (id(3), Some("v3".to_owned()), vec!["last_good"]),
                (id(2), None, vec![]),
                (id(1), Some("v1".to_owned()), vec!["pinned"]),
            ]
        );

        let mut state = state_of(&root);
        state.pending_confirm = Some(PendingConfirm {
            commit: c(&id(5)),
            previous: Some(c(&id(4))),
            activation: "1-1".to_owned(),
            deadline_wall: Some(1),
            deadline_mono_s: None,
            starts_left: None,
        });
        Locked::acquire(&root)
            .expect("lock")
            .write(&state)
            .expect("state");
        assert_eq!(
            kept(&root).expect("listing")[0].why,
            ["current", "on_trial"]
        );

        fs::write(retain::path(&root), "{").expect("garble");
        assert!(matches!(
            kept(&root),
            Err(ReleaseError::Retain(RetainError::Parse { .. }))
        ));
    }
}

/// Catches a listing of a host that has no state or no releases directory contents: nothing
/// is kept, and nothing is an error.
///
/// Catches: a missing state taken for an error.
#[test]
fn a_host_with_no_state_keeps_nothing_and_says_so() {
    let (_exec, root) = host("ret-empty");
    assert_eq!(kept(&root).expect("listing"), vec![]);
}

/// Catches the note a prune leaves after every confirm calling the layout's own `current` and
/// `previous` links strays: they live in `releases/`, and only what is neither a release nor
/// one of those links is told as left alone.
#[test]
fn the_links_in_releases_are_not_told_as_strays_and_a_stray_is() {
    for (mut exec, root) in platforms("links-quiet") {
        update("links-quiet", &mut exec, &root, 1, retain::MIN_KEEP);
        let out = update("links-quiet", &mut exec, &root, 2, retain::MIN_KEEP);
        assert!(!out.contains("left alone"), "{out}");
        fs::write(releases_dir(&root).join("notes.txt"), "x").expect("stray");
        let out = gc_on(&mut exec, &root, retain::MIN_KEEP).expect("gc");
        assert_eq!(
            out.matches("left alone (not a release): ").count(),
            1,
            "{out}"
        );
        assert!(out.contains("notes.txt"), "{out}");
    }
}
