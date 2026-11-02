//! The operator's verbs on a scratch host: the real installer verbs over a real file tree,
//! the service manager and the process table played by a recorder.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::time::Duration;

use kbf_host::Root;
use kbf_host::testkit::{ScratchHost, reply};

use super::*;
use crate::fixtures::version_json;
use crate::helper::fetcher::fake::FakeFetcher;
use crate::helper::operator;
use crate::helper::service::fake::ServiceRecorder;
use crate::layout::Layout;
use crate::state::PendingConfirm;
use crate::testkit::{A, B, C, c};

const UNIT: &str = "kbf.service";
const ACTIVATION: &str = "1-5";

struct Rig {
    exec: ScratchHost,
    layout: Layout,
    service: ServiceRecorder,
    fetcher: FakeFetcher,
    restart: Restart,
    out: Vec<u8>,
}

fn release(root: &std::path::Path, commit: &str) -> PathBuf {
    let dir = root.join("var/lib/kbf-installer/releases").join(commit);
    fs::create_dir_all(&dir).expect("release");
    fs::write(dir.join("kbf"), commit).expect("kbf");
    dir
}

impl Rig {
    /// A host on B with A before it; B is on trial (unconfirmed) when `pending`, and its
    /// service is running B's file as process 4242.
    fn new(name: &str, pending: bool) -> Rig {
        let mut exec = ScratchHost::new(&format!("operator-{name}"));
        for sub in ["run", "var/lib/kbf-installer"] {
            fs::create_dir_all(exec.root.join(sub)).expect("seed");
        }
        release(&exec.root, A);
        let b = release(&exec.root, B);
        let layout = Layout::linux(Root::new(&exec.root).expect("absolute"));
        let service = ServiceRecorder::running(4242, b.join("kbf"));
        exec.answer = service.answer();
        crate::state::Locked::acquire(&layout)
            .expect("lock")
            .write(&State {
                current: Some(c(B)),
                previous: Some(c(A)),
                pending_confirm: pending.then(|| PendingConfirm {
                    commit: c(B),
                    previous: Some(c(A)),
                    activation: ACTIVATION.to_owned(),
                    deadline_wall: Some(2_000_000_000),
                    deadline_mono_s: None,
                    starts_left: Some(3),
                }),
                ..State::default()
            })
            .expect("state");
        // The links follow the state: a host with links and no state is not provisioned.
        let kbf = exec.root.join("var/lib/kbf-installer/releases");
        symlink(B, kbf.join("current")).expect("current");
        symlink(A, kbf.join("previous")).expect("previous");
        let fetcher = FakeFetcher::new(&layout);
        Rig {
            exec,
            layout,
            service,
            fetcher,
            restart: Restart::Systemd(UNIT.to_owned()),
            out: Vec::new(),
        }
    }

    fn op(&mut self, apply: bool) -> Operator<'_, ScratchHost> {
        Operator {
            host: &self.layout,
            exec: &mut self.exec,
            restart: &self.restart,
            procs: &self.service,
            fetcher: &self.fetcher,
            out: &mut self.out,
            apply,
            wait: Duration::ZERO,
        }
    }

    fn state(&self) -> State {
        crate::state::read_unlocked(&self.layout)
            .expect("reads")
            .expect("there")
    }

    fn restarts(&self) -> usize {
        self.service
            .asked()
            .iter()
            .filter(|l| l.ends_with(&format!("restart -- {UNIT}")))
            .count()
    }

    fn marker(&self) -> bool {
        std::fs::symlink_metadata(self.layout.join(&restart::owed_marker(&self.layout))).is_ok()
    }

    fn link(&self, name: &str) -> String {
        fs::read_link(
            self.exec
                .root
                .join("var/lib/kbf-installer/releases")
                .join(name),
        )
        .expect("link")
        .display()
        .to_string()
    }

    fn said(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }
}

/// Catches a confirm that keeps a swap it was not asked about or that is not running, and
/// a dry run that changes something: a confirm naming another activation is refused (exit
/// 3), so is one while the main process runs the old release, a dry run keeps nothing, and
/// the right one with the new release running keeps it. With nothing pending it needs the
/// operator (exit 5).
///
/// Catches: the activation compared with the wrong field (any name is accepted);
/// `accept_confirm` skipped; `apply` ignored (the dry run keeps the swap).
#[test]
fn confirm_names_the_pending_activation_and_needs_the_new_release_running() {
    let mut rig = Rig::new("confirm", true);
    let err = rig.op(true).confirm("1-9").expect_err("another activation");
    assert_eq!(err.exit_code(), 3, "{err}");
    assert!(rig.state().pending_confirm.is_some());

    rig.service.0.borrow_mut().exes = vec![(
        4242,
        rig.exec
            .root
            .join("var/lib/kbf-installer/releases")
            .join(A)
            .join("kbf"),
    )];
    let err = rig
        .op(true)
        .confirm(ACTIVATION)
        .expect_err("old release runs");
    assert_eq!(err.exit_code(), 3, "{err}");
    assert!(rig.state().pending_confirm.is_some());

    rig.service.0.borrow_mut().exes = vec![(
        4242,
        rig.exec
            .root
            .join("var/lib/kbf-installer/releases")
            .join(B)
            .join("kbf"),
    )];
    rig.op(false).confirm(ACTIVATION).expect("dry run");
    assert!(
        rig.state().pending_confirm.is_some(),
        "a dry run keeps nothing"
    );
    assert!(rig.said().contains("would confirm"), "{}", rig.said());

    rig.op(true).confirm(ACTIVATION).expect("confirmed");
    assert_eq!(rig.state().pending_confirm, None);
    assert_eq!(rig.state().current, Some(c(B)));

    let err = rig
        .op(true)
        .confirm(ACTIVATION)
        .expect_err("nothing pending");
    assert_eq!(err.exit_code(), 5, "{err}");
}

/// Catches a revert that leaves the host on the new release, one that forgets to restart
/// the service, and a restart marker left behind: the state goes first, then the links,
/// the unit is restarted once under the lock, and the marker is gone. A dry run does none
/// of it.
///
/// Catches: the restart skipped; the marker not removed; `apply` ignored.
#[test]
fn revert_goes_back_restarts_once_and_leaves_no_marker() {
    let mut rig = Rig::new("revert", true);
    rig.op(false).revert(ACTIVATION).expect("dry run");
    assert_eq!(rig.state().current, Some(c(B)));
    assert_eq!(rig.restarts(), 0);
    assert!(!rig.marker());

    rig.op(true).revert(ACTIVATION).expect("reverted");
    let state = rig.state();
    assert_eq!((state.current, state.pending_confirm), (Some(c(A)), None));
    assert_eq!(rig.link("current"), A);
    assert_eq!(rig.restarts(), 1);
    assert!(!rig.marker());
}

/// Catches a restart lost when the verb dies or the restart fails: the links have moved,
/// the failure is reported (exit 1), and the marker stays for the service's next pass,
/// which restarts the unit and removes it.
///
/// Catches: the marker written after the change instead of before; the marker
/// removed even when the restart failed.
#[test]
fn a_restart_that_fails_stays_owed_to_the_next_pass() {
    let mut rig = Rig::new("owed", true);
    let base = rig.service.answer();
    rig.exec.answer = Box::new(move |cmd| {
        if cmd.args.first().map(String::as_str) == Some("restart") {
            return Ok(reply(1, "", "no"));
        }
        base(cmd)
    });
    let err = rig.op(true).revert(ACTIVATION).expect_err("restart fails");
    assert_eq!(err.exit_code(), 1, "{err}");
    assert_eq!(rig.state().current, Some(c(A)), "the links moved");
    assert!(rig.marker(), "the restart is owed");
}

/// Catches a rollback that goes below a trial, with no target, or loses the release it left:
/// with a swap pending it is refused (exit 5), it moves `current` to `previous` and clears
/// `previous` (only a confirmed release is ever previous), and a second one has nothing to
/// go back to (exit 5).
///
/// Catches: the pending check removed; `previous` left as it was.
#[test]
fn rollback_goes_to_previous_and_only_when_nothing_is_on_trial() {
    let mut rig = Rig::new("rollback", true);
    let err = rig.op(true).rollback().expect_err("a swap is pending");
    assert_eq!(err.exit_code(), 5, "{err}");
    assert_eq!(rig.state().current, Some(c(B)));

    let mut rig = Rig::new("rollback-ok", false);
    rig.op(false).rollback().expect("dry run");
    assert_eq!(rig.state().current, Some(c(B)));
    rig.op(true).rollback().expect("rolled back");
    let state = rig.state();
    assert_eq!((state.current, state.previous), (Some(c(A)), None));
    assert_eq!(rig.restarts(), 1);
    assert!(!rig.marker());
    let err = rig.op(true).rollback().expect_err("no previous");
    assert_eq!(err.exit_code(), 5, "{err}");
}

/// What a kbf that declares `relay` level 1 prints for `kbf --version --json`.
fn relays(commit: &str) -> String {
    version_json(commit, "release").replace('}', ",\"relay\":1}")
}

impl Rig {
    /// The host's `installer.toml` says `source`, and the kbf of release `A` (the previous one)
    /// prints each of `said` (the last one is what the fake keeps).
    fn previous_prints(&self, source: Option<&'static str>, said: &[String]) {
        *self.fetcher.source.lock().expect("lock") = source;
        for text in said {
            self.fetcher
                .versions
                .lock()
                .expect("lock")
                .insert(c(A), text.clone());
        }
    }

    fn state_bytes(&self) -> Vec<u8> {
        fs::read(self.layout.join(&self.layout.state_file())).expect("state.json")
    }

    fn probed(&self) -> Vec<crate::CommitId> {
        self.fetcher.probes.lock().expect("lock").clone()
    }
}

/// Catches a rollback that puts a bundled host on a kbf that cannot relay a staged order: such a
/// host fetches nothing, a new kbf reaches it only as a staged order, and a kbf that does not
/// relay is the one that could not hand it on. Whatever the previous release's kbf prints
/// without `relay` (no field, level 0, no JSON, or nothing to ask) is refused (exit 3) with a
/// dry run too, before anything is written: `state.json` is the same bytes, `current` stays, no
/// restart is owed or made. The message says why and the way forward.
///
/// Catches: the bundled check removed (the rollback goes ahead); an unknown relay read
/// as relaying (the no-field and no-JSON cases go ahead); the check made after the state is
/// written (the bytes differ).
#[test]
fn a_bundled_host_is_not_rolled_back_to_a_kbf_that_cannot_relay() {
    let level_0 = version_json(A, "release").replace('}', ",\"relay\":0}");
    let cases = [
        ("no-field", vec![version_json(A, "release")]),
        ("level-0", vec![level_0]),
        ("no-json", vec!["it prints no JSON".to_owned()]),
        ("nothing-to-ask", Vec::new()),
    ];
    for (name, said) in cases {
        for apply in [false, true] {
            let mut rig = Rig::new(&format!("bundled-{name}-{apply}"), false);
            rig.previous_prints(Some("bundled"), &said);
            let before = rig.state_bytes();
            let err = rig.op(apply).rollback().expect_err("cannot relay");
            assert_eq!(err.exit_code(), 3, "{name} {apply}: {err}");
            let text = err.to_string();
            let wants = [
                "the previous release's kbf cannot relay a staged order",
                "no way to update",
                "update --source staged",
                "release_source back to url",
            ];
            for want in wants {
                assert!(text.contains(want), "{name} {apply}: {text}");
            }
            assert_eq!(rig.state_bytes(), before, "{name} {apply}");
            assert_eq!(rig.state().current, Some(c(B)), "{name} {apply}");
            assert_eq!(rig.link("current"), B, "{name} {apply}");
            assert_eq!(rig.restarts(), 0, "{name} {apply}");
            assert!(!rig.marker(), "{name} {apply}");
        }
    }
}

/// Catches the relay check asking the wrong release, or refusing a kbf that relays: on a bundled
/// host whose previous release declares `relay`, a dry run changes nothing and the rollback
/// goes ahead as it always did, and the release asked is the one rolled back to (the current
/// release has no answer in the fake, so asking it would refuse).
///
/// Catches: the current release asked instead of `previous`; the answer inverted.
#[test]
fn a_bundled_host_is_rolled_back_to_a_kbf_that_relays() {
    let mut rig = Rig::new("bundled-relays", false);
    rig.previous_prints(Some("bundled"), &[relays(A)]);
    rig.op(false).rollback().expect("dry run");
    assert_eq!(rig.state().current, Some(c(B)));
    rig.op(true).rollback().expect("rolled back");
    let state = rig.state();
    assert_eq!((state.current, state.previous), (Some(c(A)), None));
    assert_eq!(rig.link("current"), A);
    assert_eq!(rig.restarts(), 1);
    assert_eq!(rig.probed(), vec![c(A), c(A)]);
}

/// Catches the check applied to a host that fetches: a url or frontend host (and one whose
/// `installer.toml` cannot be read) rolls back to a kbf that does not relay as it always did,
/// and the release is not even asked.
///
/// Catches: the source word not compared (every host is checked); the unknown source
/// treated as bundled.
#[test]
fn a_host_that_fetches_is_rolled_back_whatever_the_previous_kbf_relays() {
    let hosts = [
        ("url", Some("url")),
        ("frontend", Some("frontend")),
        ("none", None),
    ];
    for (name, source) in hosts {
        let mut rig = Rig::new(&format!("fetching-{name}"), false);
        rig.previous_prints(source, &[version_json(A, "release")]);
        rig.op(true).rollback().expect("rolled back");
        assert_eq!(rig.state().current, Some(c(A)), "{name}");
        assert_eq!(rig.restarts(), 1, "{name}");
        assert!(rig.probed().is_empty(), "{name}");
    }
}

/// Catches a revert or a rollback that puts a release back as `current` while `staged` still
/// names it. The proxy can stage the release that is `previous` (`stage_frontend` refuses
/// only the current one), and the host then showed a running release as "staged", refused
/// every other frontend update as busy, and would let the frontend's confirm of it write
/// `previous` = `current`. A `staged` that names another release is another decision's and
/// stays.
///
/// Catches: `staged` carried over by `revert_in`; the same in `rollback_in`;
/// `staged` cleared whatever it names in either.
#[test]
fn a_revert_or_rollback_drops_a_staged_release_it_puts_back_and_keeps_another() {
    for (pending, verb) in [(true, "revert"), (false, "rollback")] {
        for (staged, want) in [(A, None), (C, Some(c(C)))] {
            let mut rig = Rig::new(&format!("staged-{verb}-{staged}"), pending);
            release(&rig.exec.root, C);
            crate::activate::stage_frontend(&rig.exec, &rig.layout, &c(staged)).expect("staged");
            assert_eq!(rig.state().staged, Some(c(staged)), "{verb} {staged}");

            let done = if pending {
                rig.op(true).revert(ACTIVATION)
            } else {
                rig.op(true).rollback()
            };
            done.expect("moved");
            let state = rig.state();
            assert_eq!(state.current, Some(c(A)), "{verb} {staged}");
            assert_eq!(state.staged, want, "{verb} {staged}");
        }
    }
}

/// Catches `gc` removing what must stay and a dry run that removes: current, previous, a
/// release a process runs and the two newest others (the floor of `--keep 0`) stay; the
/// oldest goes only with `apply`; and a host whose processes cannot be listed removes
/// nothing (exit 1).
///
/// Catches: in-use ignored; the dry run applying; the blind host treated as empty; the floor
/// of two not applied (the newest others would go too).
#[test]
fn gc_keeps_what_runs_and_removes_nothing_when_it_cannot_tell() {
    let mut rig = Rig::new("gc", false);
    let releases = rig.exec.root.join("var/lib/kbf-installer/releases");
    let running = release(&rig.exec.root, C);
    let stray = release(&rig.exec.root, "dddddddddddddddddddddddddddddddddddddddd");
    let newer = [
        release(&rig.exec.root, "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
        release(&rig.exec.root, "ffffffffffffffffffffffffffffffffffffffff"),
    ];
    for (d, secs) in [
        (&running, 1000),
        (&stray, 1000),
        (&newer[0], 2000),
        (&newer[1], 3000),
    ] {
        fs::File::open(d)
            .expect("dir")
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
            .expect("mtime");
    }
    rig.service
        .0
        .borrow_mut()
        .exes
        .push((9, running.join("kbf")));

    let mut out = Vec::new();
    operator::gc(&mut rig.exec, &rig.layout, &rig.service, 0, false, &mut out).expect("dry run");
    assert!(stray.exists(), "a dry run removes nothing");

    operator::gc(&mut rig.exec, &rig.layout, &rig.service, 0, true, &mut out).expect("gc");
    assert!(!stray.exists());
    assert!(running.exists() && releases.join(A).exists() && releases.join(B).exists());
    assert!(
        newer.iter().all(|d| d.exists()),
        "the floor keeps two others"
    );

    rig.service.0.borrow_mut().blind = true;
    let again = release(&rig.exec.root, "dddddddddddddddddddddddddddddddddddddddd");
    let err = operator::gc(&mut rig.exec, &rig.layout, &rig.service, 0, true, &mut out)
        .expect_err("blind");
    assert_eq!(err.exit_code(), 1, "{err}");
    assert!(again.exists());
}

/// Catches a verb that queues behind a pass and one that runs beside it: with the lock held
/// every verb is busy (exit 4) and changes nothing; `quiesce` is exit 5 while a swap is
/// pending and ok when the host is at rest.
///
/// Catches: the lock not taken by `confirm`; `quiesce` ignoring the pending swap.
#[test]
fn verbs_are_busy_while_the_lock_is_held_and_quiesce_wants_rest() {
    let mut rig = Rig::new("busy", true);
    {
        let _held = crate::state::Locked::acquire(&rig.layout).expect("lock");
        for err in [
            rig.op(true).confirm(ACTIVATION).expect_err("busy"),
            rig.op(true).revert(ACTIVATION).expect_err("busy"),
            rig.op(true).rollback().expect_err("busy"),
            operator::quiesce(&rig.layout, Duration::ZERO).expect_err("busy"),
        ] {
            assert_eq!(err.exit_code(), 4, "{err}");
        }
    }
    assert_eq!(rig.state().current, Some(c(B)));
    assert!(rig.state().pending_confirm.is_some());
    let err = operator::quiesce(&rig.layout, Duration::ZERO).expect_err("pending");
    assert_eq!(err.exit_code(), 5, "{err}");

    let rig = Rig::new("rest", false);
    operator::quiesce(&rig.layout, Duration::ZERO).expect("at rest");
}

/// The previous release (A) loses its binary on a scratch host.
fn lose_previous_binary(rig: &Rig) {
    let binary = rig
        .exec
        .root
        .join("var/lib/kbf-installer/releases")
        .join(A)
        .join("kbf");
    fs::remove_file(binary).expect("the previous release loses its binary");
}

/// Catches a verb whose change fails leaving the restart marker behind (the next pass would
/// restart a service nothing changed for), or reporting success: with the previous release's
/// binary gone, `revert` is refused as a failure (exit 1), nothing moves and no marker stays;
/// a `rollback` of a host with no trial is refused the same way before anything moves.
///
/// Catches: the marker removal after a failed change dropped; the binary of the
/// release rolled back to not looked for.
#[test]
fn a_change_that_fails_leaves_no_marker_and_moves_nothing() {
    let mut rig = Rig::new("change-fails", true);
    lose_previous_binary(&rig);
    let err = rig
        .op(true)
        .revert(ACTIVATION)
        .expect_err("nothing to go back to");
    assert_eq!(err.exit_code(), 1, "{err}");
    assert_eq!(rig.state().current, Some(c(B)));
    assert!(!rig.marker());
    assert_eq!(rig.restarts(), 0);

    // A change that fails after the state moved keeps the marker: the links are behind the
    // state, and the service's next pass completes them and restarts.
    let mut rig = Rig::new("change-fails-late", true);
    let releases = rig.exec.root.join("var/lib/kbf-installer/releases");
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o555)).expect("close");
    let err = rig.op(true).revert(ACTIVATION);
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o755)).expect("open");
    let err = err.expect_err("the links cannot be flipped");
    assert_eq!(err.exit_code(), 1, "{err}");
    assert_eq!(rig.state().current, Some(c(A)), "the state moved");
    assert!(rig.marker(), "and the restart is owed");

    let mut rig = Rig::new("rollback-gone", false);
    lose_previous_binary(&rig);
    let err = rig.op(true).rollback().expect_err("nothing to go back to");
    assert_eq!(err.exit_code(), 1, "{err}");
    assert_eq!(rig.state().current, Some(c(B)));
    assert!(!rig.marker());
}

/// Catches the restart marker being skipped when it cannot be written (a verb that moves
/// links with no record that the service owes a restart), and a marker that cannot be
/// removed after the restart being passed over: the first is exit 1 with nothing moved, the
/// second is exit 1 after the restart, the marker still there for the next pass.
///
/// Catches: the write error ignored (the links move); the removal error ignored.
#[test]
fn a_marker_that_cannot_be_written_or_removed_is_reported() {
    let mut rig = Rig::new("marker-write", true);
    let marker = rig.layout.join(&restart::owed_marker(&rig.layout));
    fs::create_dir(&marker).expect("a directory where the marker goes");
    let err = rig.op(true).revert(ACTIVATION).expect_err("cannot write");
    assert_eq!(err.exit_code(), 1, "{err}");
    assert!(
        err.to_string().contains("cannot write the restart marker"),
        "{err}"
    );
    assert_eq!(rig.state().current, Some(c(B)), "nothing moved");

    let mut rig = Rig::new("marker-remove", true);
    let marker = rig.layout.join(&restart::owed_marker(&rig.layout));
    let base = rig.service.answer();
    let dir = marker.parent().expect("a directory").to_owned();
    rig.exec.answer = Box::new(move |cmd| {
        if cmd.args.first().map(String::as_str) == Some("restart") {
            // The directory the marker is in closes: a file cannot be removed from it.
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).expect("close");
        }
        base(cmd)
    });
    let err = rig.op(true).revert(ACTIVATION);
    fs::set_permissions(
        marker.parent().expect("a directory"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("open");
    let err = err.expect_err("cannot remove");
    assert_eq!(err.exit_code(), 1, "{err}");
    assert!(
        err.to_string().contains("cannot remove the restart marker"),
        "{err}"
    );
}

/// Catches an error landing on the wrong exit code (contract section 2): a lock another run
/// holds is busy (4), both state files unusable and an activation that is not there or a
/// release with nothing before it need the operator (5), a verb naming another activation is
/// refused (3), and every other state or release error failed (1).
///
/// Catches: the busy arm folded into failed; the unusable state a plain failure.
#[test]
fn errors_land_on_the_exit_of_the_contract() {
    use crate::state::StateError;
    let busy = || StateError::Busy {
        path: PathBuf::from("/x"),
    };
    let unusable = || StateError::Unusable {
        primary: "p".to_owned(),
        backup: "b".to_owned(),
    };
    let other = || StateError::NoSchema {
        path: PathBuf::from("/x"),
    };
    let code = |f: Failure| f.exit_code();
    assert_eq!(code(busy().into()), 4);
    assert_eq!(code(unusable().into()), 5);
    assert_eq!(code(other().into()), 1);
    assert_eq!(code(ReleaseError::State(busy()).into()), 4);
    assert_eq!(code(ReleaseError::State(unusable()).into()), 5);
    assert_eq!(code(ReleaseError::NotPending(c(A)).into()), 5);
    assert_eq!(code(ReleaseError::NoPrevious.into()), 5);
    assert_eq!(code(ReleaseError::Pending(c(A)).into()), 5);
    let other_order = ReleaseError::OtherOrder {
        commit: c(A),
        pending: "1-1".to_owned(),
        got: "1-2".to_owned(),
    };
    assert_eq!(code(other_order.into()), 3);
    assert_eq!(code(ReleaseError::NotUtf8(PathBuf::from("/x")).into()), 1);
}

/// Catches a `rollback` whose links cannot be flipped reporting success or dropping the
/// marker (the state has moved, so the service's next pass owes the restart), and a
/// `quiesce` that calls a host at rest when its state cannot be read: with the releases
/// directory closed, `rollback` is exit 1 with the state moved and the marker kept; with a
/// state file of another schema `quiesce` is a failure.
///
/// Catches: the rollback error swallowed (success reported); the read error of
/// `quiesce` treated as an empty state.
#[test]
fn a_rollback_that_fails_late_keeps_the_marker_and_quiesce_needs_a_readable_state() {
    let mut rig = Rig::new("rollback-late", false);
    let releases = rig.exec.root.join("var/lib/kbf-installer/releases");
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o555)).expect("close");
    let err = rig.op(true).rollback();
    fs::set_permissions(&releases, fs::Permissions::from_mode(0o755)).expect("open");
    let err = err.expect_err("the links cannot be flipped");
    assert_eq!(err.exit_code(), 1, "{err}");
    assert_eq!(rig.state().current, Some(c(A)), "the state moved");
    assert!(rig.marker(), "and the restart is owed");

    let rig = Rig::new("quiesce-schema", false);
    fs::write(
        rig.exec.root.join("var/lib/kbf-installer/state.json"),
        "{\"schema\": 99}",
    )
    .expect("another schema");
    let err = operator::quiesce(&rig.layout, Duration::ZERO).expect_err("unreadable");
    assert_eq!(err.exit_code(), 1, "{err}");
}
