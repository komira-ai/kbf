//! [`select`]: the operator's swap to a release the host keeps, with no trial.

use super::*;

/// A swap to a kept release writes the state first and moves both links: `current` is the
/// release named, `previous` the release that ran, nothing is on trial and a `staged` that
/// names the release is cleared (one that names another release stays).
///
/// Catches: `previous` not moved to the release left; a trial armed by the swap; `staged`
/// left naming the release that now runs, or cleared when it names another.
#[test]
fn select_makes_a_kept_release_current_for_good_and_the_one_left_previous() {
    for (staged, want) in [(B, None), (C, Some(c(C)))] {
        let (mut exec, layout) = host(&format!("select-{staged}"));
        running(&exec, &layout, A, &[B, C]);
        let mut state = state_of(&layout);
        state.staged = Some(c(staged));
        Locked::acquire(&layout)
            .expect("lock")
            .write(&state)
            .expect("state");
        let selected = select(&mut exec, &layout, &c(B), &mut Vec::new()).expect("select");
        assert_eq!(selected, Selected { from: Some(c(A)) });
        let state = state_of(&layout);
        assert_eq!(state.current, Some(c(B)));
        assert_eq!(state.previous, Some(c(A)));
        assert_eq!(state.pending_confirm, None);
        assert_eq!(state.staged, want, "{staged}");
        assert_eq!(link(&exec, "current"), at(B));
        assert_eq!(link(&exec, "previous"), at(A));
    }
}

/// A swap on trial ends at a select: going back to the release before the trial leaves the one
/// on trial as `previous`; naming the release on trial keeps it, and `previous` stays the one
/// before it.
///
/// Catches: a `pending_confirm` that outlives the select, so the deadline or the boot check
/// later reverts what the operator chose.
#[test]
fn select_ends_a_swap_on_trial_whichever_release_it_names() {
    for (target, want_previous) in [(A, B), (B, A)] {
        let (mut exec, layout) = host(&format!("select-trial-{target}"));
        running(&exec, &layout, A, &[B]);
        go(&mut exec, &layout, &order(B, "o1", 1), START).expect("activate");
        assert!(state_of(&layout).pending_confirm.is_some());
        let selected = select(&mut exec, &layout, &c(target), &mut Vec::new()).expect("select");
        assert_eq!(selected.from, Some(c(B)), "{target}");
        let state = state_of(&layout);
        assert_eq!(state.current, Some(c(target)), "{target}");
        assert_eq!(state.previous, Some(c(want_previous)), "{target}");
        assert_eq!(state.pending_confirm, None, "{target}");
        assert_eq!(link(&exec, "current"), at(target));
        assert_eq!(link(&exec, "previous"), at(want_previous));
    }
}

/// The release that runs, named again, keeps `previous` and makes the links agree with the
/// state; a host with no release yet takes the one named with no `previous`.
///
/// Catches: `previous` set to the release itself; a select refused on a host with no current.
#[test]
fn select_of_the_release_that_runs_or_on_an_empty_host_keeps_previous_as_it_is() {
    let (mut exec, layout) = host("select-same");
    running(&exec, &layout, A, &[B]);
    select(&mut exec, &layout, &c(B), &mut Vec::new()).expect("to B");
    fs::remove_file(exec.at("/var/lib/kbf-installer/releases/current")).expect("unlink");
    let selected = select(&mut exec, &layout, &c(B), &mut Vec::new()).expect("again");
    assert_eq!(selected.from, Some(c(B)));
    let state = state_of(&layout);
    assert_eq!((state.current, state.previous), (Some(c(B)), Some(c(A))));
    assert_eq!(link(&exec, "current"), at(B));

    let (mut exec, layout) = host("select-empty");
    seed(&exec, A, 1_000);
    let selected = select(&mut exec, &layout, &c(A), &mut Vec::new()).expect("first");
    assert_eq!(selected.from, None);
    let state = state_of(&layout);
    assert_eq!((state.current, state.previous), (Some(c(A)), None));
}

/// A release that is not installed is refused and nothing changes: no state is written and no
/// link moves.
///
/// Catches: a select that moves `current` to a directory with no binary.
#[test]
fn select_refuses_a_release_that_is_not_installed_and_changes_nothing() {
    let (mut exec, layout) = host("select-missing");
    running(&exec, &layout, A, &[]);
    let before = state_of(&layout);
    let err = select(&mut exec, &layout, &c(D), &mut Vec::new()).expect_err("not installed");
    assert!(
        matches!(err, ReleaseError::NotInstalled(ref id) if *id == c(D)),
        "{err}"
    );
    assert_eq!(state_of(&layout), before);
    assert_eq!(link(&exec, "current"), at(A));
    assert_eq!(link_steps(&exec), 0);
}

/// Another run that holds the state's lock is told at once (busy, naming the lock), and nothing
/// changes: select does not wait behind it.
///
/// Catches: an emergency swap left hanging behind a run that does not let go.
#[test]
fn select_refuses_at_once_while_another_run_holds_the_state() {
    let (mut exec, layout) = host("select-busy");
    running(&exec, &layout, A, &[B]);
    let held = Locked::acquire(&layout).expect("lock");
    let err = select(&mut exec, &layout, &c(B), &mut Vec::new()).expect_err("busy");
    assert!(
        matches!(
            &err,
            ReleaseError::State(crate::state::StateError::Busy { .. })
        ),
        "{err}"
    );
    drop(held);
    assert_eq!(state_of(&layout).current, Some(c(A)));
    assert_eq!(link(&exec, "current"), at(A));
}

/// Links that cannot follow the state put the state back as it was: a select that failed
/// leaves `current` naming the release that still runs, so the next select (to another
/// release) leaves that one, and `previous` names it.
///
/// Catches: a failed select whose state names a release that never ran, which the next select
/// then reports as left and makes `previous`.
#[test]
fn a_select_whose_links_cannot_follow_leaves_the_state_as_it_was() {
    let (mut exec, layout) = host("select-stuck");
    running(&exec, &layout, A, &[B, C]);
    let before = state_of(&layout);
    // The links' directory takes no new link (its links can still be read): the state written
    // for B is put back.
    {
        use std::os::unix::fs::PermissionsExt;
        let releases = exec.at("/var/lib/kbf-installer/releases");
        fs::set_permissions(&releases, fs::Permissions::from_mode(0o555)).expect("close");
        let err = select(&mut exec, &layout, &c(B), &mut Vec::new());
        fs::set_permissions(&releases, fs::Permissions::from_mode(0o755)).expect("open");
        err.expect_err("cannot link");
    }
    assert_eq!(state_of(&layout), before, "the state is as it was");
    assert_eq!(link(&exec, "current"), at(A));
    // A host where every link step fails puts the state back the same way.
    let mut killed = Killed {
        host: &mut exec,
        left: 0,
    };
    select(&mut killed, &layout, &c(B), &mut Vec::new()).expect_err("cannot link");
    assert_eq!(state_of(&layout), before, "the state is as it was");
    // A directory where `previous` goes is no history to read: refused before anything is
    // written.
    let previous = exec.at("/var/lib/kbf-installer/releases/previous");
    fs::create_dir(&previous).expect("dir");
    fs::write(previous.join("x"), "in the way").expect("file");
    let err = select(&mut exec, &layout, &c(B), &mut Vec::new()).expect_err("unreadable");
    assert!(err.to_string().ends_with(": not a link"), "{err}");
    assert_eq!(state_of(&layout), before, "the state is as it was");
    fs::remove_dir_all(&previous).expect("clear");
    let selected = select(&mut exec, &layout, &c(C), &mut Vec::new()).expect("to C");
    assert_eq!(selected.from, Some(c(A)));
    let state = state_of(&layout);
    assert_eq!((state.current, state.previous), (Some(c(C)), Some(c(A))));
    assert_eq!(link(&exec, "previous"), at(A));
}

/// A host whose state directory turns read-only at the first link step: the select's state is
/// written, its links fail, and the state cannot be put back.
struct Sealing<'a> {
    host: &'a mut ScratchHost,
    dir: std::path::PathBuf,
    /// What else happens to the host at that step.
    also: &'a dyn Fn(),
}

impl Exec for Sealing<'_> {
    fn euid(&self) -> u32 {
        self.host.euid()
    }
    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        self.host.inspect(path)
    }
    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>> {
        self.host.stat(path)
    }
    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.host.query(cmd)
    }
    fn http_status(&self, url: &HttpUrl) -> io::Result<u16> {
        self.host.http_status(url)
    }
    fn apply(&mut self, _step: &Step) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        (self.also)();
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o555))?;
        Err(io::Error::other("the link step failed"))
    }
    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// Links that cannot follow and a state that cannot be put back: the failure says both and what
/// `current` names; the state still names the release that never ran, and the next select (to
/// another release) takes the release it leaves from the links, so `previous` is the release that
/// ran.
///
/// Catches: a recovery failure swallowed, and the history of the next select taken from a state
/// that names a release that never ran.
#[test]
fn a_select_that_cannot_put_the_state_back_says_so_and_the_next_one_follows_the_links() {
    use std::os::unix::fs::PermissionsExt;
    let (mut exec, layout) = host("select-sealed");
    running(&exec, &layout, A, &[B, C, D]);
    // A runs and C is `previous`, in the state and the links.
    let mut state = state_of(&layout);
    state.previous = Some(c(C));
    Locked::acquire(&layout)
        .expect("lock")
        .write(&state)
        .expect("state");
    symlink(C, exec.at("/var/lib/kbf-installer/releases/previous")).expect("link");
    let dir = exec.at("/var/lib/kbf-installer");
    let err = select(
        &mut Sealing {
            host: &mut exec,
            dir: dir.clone(),
            also: &|| {},
        },
        &layout,
        &c(B),
        &mut Vec::new(),
    )
    .expect_err("cannot link");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("open again");
    assert!(
        matches!(&err, ReleaseError::NotPutBack { current, .. } if *current == format!("names {A}")),
        "{err}"
    );
    let text = err.to_string();
    assert!(
        text.contains("the link step failed; and the state could not be put back ("),
        "{text}"
    );
    assert!(
        text.ends_with(&format!(
            "`current` names {A}, run `reconcile` or select again"
        )),
        "{text}"
    );
    assert_eq!(
        state_of(&layout).current,
        Some(c(B)),
        "the state was not put back"
    );
    assert_eq!(link(&exec, "current"), at(A));
    // Selecting the release that stayed current keeps the `previous` the links name (C), not the
    // release the state says was left.
    let selected = select(&mut exec, &layout, &c(A), &mut Vec::new()).expect("to A");
    assert_eq!(selected.from, Some(c(A)));
    let state = state_of(&layout);
    assert_eq!((state.current, state.previous), (Some(c(A)), Some(c(C))));
    assert_eq!(link(&exec, "previous"), at(C));
    let selected = select(&mut exec, &layout, &c(D), &mut Vec::new()).expect("to D");
    assert_eq!(selected.from, Some(c(A)));
    let state = state_of(&layout);
    assert_eq!((state.current, state.previous), (Some(c(D)), Some(c(A))));
}

/// A host whose first look at `current` fails, and every later one works.
struct Flaky<'a> {
    host: &'a mut ScratchHost,
    current: HostPath,
    failed: std::cell::Cell<bool>,
}

impl Exec for Flaky<'_> {
    fn euid(&self) -> u32 {
        self.host.euid()
    }
    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        if *path == self.current && !self.failed.replace(true) {
            return Err(io::Error::other("cannot look"));
        }
        self.host.inspect(path)
    }
    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>> {
        self.host.stat(path)
    }
    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.host.query(cmd)
    }
    fn http_status(&self, url: &HttpUrl) -> io::Result<u16> {
        self.host.http_status(url)
    }
    fn apply(&mut self, step: &Step) -> io::Result<()> {
        self.host.apply(step)
    }
    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// With the state naming a release that never ran (B) and `current` linking to A, a look at
/// `current` that fails refuses the select before anything is written, so the state's B is never
/// taken for the release left; asked again, the select leaves A. A `current` that links to no
/// release is refused the same way.
///
/// Catches: a failed look at the links read as "no link", so the history comes from the state.
#[test]
fn a_select_that_cannot_read_current_refuses_before_writing() {
    let (mut exec, layout) = host("select-flaky");
    running(&exec, &layout, A, &[B, C]);
    let mut state = state_of(&layout);
    state.current = Some(c(B));
    Locked::acquire(&layout)
        .expect("lock")
        .write(&state)
        .expect("state");
    let current = layout.current();
    let err = select(
        &mut Flaky {
            host: &mut exec,
            current,
            failed: std::cell::Cell::new(false),
        },
        &layout,
        &c(C),
        &mut Vec::new(),
    )
    .expect_err("cannot look");
    assert!(err.to_string().ends_with("cannot look"), "{err}");
    assert_eq!(state_of(&layout), state, "nothing written");
    let selected = select(&mut exec, &layout, &c(C), &mut Vec::new()).expect("to C");
    assert_eq!(selected.from, Some(c(A)));
    assert_eq!(state_of(&layout).previous, Some(c(A)));

    let link_path = exec.at("/var/lib/kbf-installer/releases/current");
    fs::remove_file(&link_path).expect("unlink");
    symlink("not-a-release", &link_path).expect("link");
    let before = state_of(&layout);
    let err = select(&mut exec, &layout, &c(A), &mut Vec::new()).expect_err("no release");
    assert!(
        err.to_string()
            .ends_with("a link to not-a-release, which is no release"),
        "{err}"
    );
    assert_eq!(state_of(&layout), before);
}

/// When the state cannot be put back, the failure says what `current` is even when it is no
/// link (removed meanwhile) or cannot be looked at.
///
/// Catches: a `current` that cannot be read reported as "no release".
#[test]
fn a_select_that_cannot_put_the_state_back_tells_a_current_it_cannot_read() {
    use std::os::unix::fs::PermissionsExt;
    for case in ["gone", "unreadable"] {
        let (mut exec, layout) = host(&format!("select-sealed-{case}"));
        running(&exec, &layout, A, &[B]);
        let dir = exec.at("/var/lib/kbf-installer");
        let releases = exec.at("/var/lib/kbf-installer/releases");
        let current = releases.join("current");
        let also = || match case {
            // Run at each link step: the swap's and the putting back's.
            "gone" => drop(fs::remove_file(&current)),
            _ => fs::set_permissions(&releases, fs::Permissions::from_mode(0o000)).expect("close"),
        };
        let err = select(
            &mut Sealing {
                host: &mut exec,
                dir: dir.clone(),
                also: &also,
            },
            &layout,
            &c(B),
            &mut Vec::new(),
        )
        .expect_err("cannot link");
        fs::set_permissions(&releases, fs::Permissions::from_mode(0o755)).expect("open");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("open");
        let ReleaseError::NotPutBack { current, .. } = &err else {
            panic!("{err}");
        };
        if case == "gone" {
            assert_eq!(current, "is no link");
        } else {
            assert!(current.starts_with("cannot be read ("), "{current}");
        }
    }
}
