//! `unpin` on a Linux host: the pin `activate --pin` left undone, under the install lock, and
//! what an unpin does with a hold file that is not root's or not there.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::super::{A, Answer, Rig, Trust};
use super::{
    B, Borrowed, NOW, PIN, Retained, VB, activated, agent_dir, core, flagged, hold_as, id, keep,
    retain, target,
};
use crate::host::take_lock;
use crate::local::activate_cli::{self, Command};
use crate::local::answer::Code;
use crate::local::backend::Backend;
use crate::local::core::Ab;
use crate::local::production::LinuxEnv;
use crate::local::unpin_cli;
use crate::locks;

fn pinned(rig: &Rig) -> Vec<String> {
    Retained::read(&retain::path(&rig.layout))
        .unwrap()
        .pinned
        .into_iter()
        .map(|c| c.to_string())
        .collect()
}

fn unpinned(commit: Option<&str>) -> Answer {
    Answer::Unpinned {
        commit: commit.map(id),
    }
}

/// After `activate --pin`, `unpin` takes the release out of `retain.json`'s `pinned` and removes
/// the hold file: the answer names the commit. Asked again, nothing is pinned, and it says so
/// and changes nothing. The release that runs is not touched.
///
/// Catches: an unpin that leaves the hold file (the agent stays pinned) or the retain pin (the
/// release is never pruned); an unpin that fails on a host with nothing pinned.
#[test]
fn unpin_removes_the_hold_file_and_the_retain_pin() {
    let rig = Rig::new("unpin-done");
    keep(&rig, B, Some(VB), 2);
    let (ab, notes) = core(&rig);
    assert_eq!(
        ab.activate(&target(VB), PIN),
        flagged(activated(B, Some(VB), A), true, false)
    );
    let hold = agent_dir(&rig).join("pin.json");
    assert!(hold.exists());
    assert_eq!(pinned(&rig), [B]);

    let answer = ab.unpin();
    assert_eq!(answer, unpinned(Some(B)));
    assert_eq!(
        answer.json_line(),
        format!("{{\"result\":\"unpinned\",\"commit\":\"{B}\"}}")
    );
    assert_eq!(answer.audit_fields(), ("unpinned", None));
    assert_eq!(answer.log_detail(), None);
    assert!(!hold.exists());
    assert!(pinned(&rig).is_empty());
    assert_eq!(
        rig.state().current.map(|c| c.to_string()),
        Some(B.to_owned())
    );

    let again = ab.unpin();
    assert_eq!(again, unpinned(None));
    assert_eq!(
        again.json_line(),
        "{\"result\":\"unpinned\",\"commit\":null}"
    );
    assert!(notes.borrow().is_empty(), "{:?}", notes.borrow());
}

/// A hold file that is not root's regular file, or names no commit, is removed and unpins
/// nothing in `retain.json` (a link the agent's account put there is removed, never followed);
/// the unpin notes why. A pin of another commit in `retain.json` stays.
///
/// Catches: an unpin that trusts a commit the agent's account wrote; one that follows a link.
#[test]
fn a_hold_file_root_did_not_write_unpins_nothing_in_retain() {
    let rig = Rig::new("unpin-untrusted");
    keep(&rig, B, Some(VB), 2);
    let (ab, notes) = core(&rig);
    Retained::update(&retain::path(&rig.layout), |r| {
        r.pinned.insert(id(B));
    })
    .unwrap();
    let hold = agent_dir(&rig).join("pin.json");
    let elsewhere = rig.dir.join("elsewhere");
    fs::write(&elsewhere, format!("{{\"commit\":\"{B}\"}}")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &hold).unwrap();
    assert_eq!(ab.unpin(), unpinned(None));
    assert!(fs::symlink_metadata(&hold).is_err(), "the link is removed");
    assert!(elsewhere.exists(), "and not followed");
    assert_eq!(pinned(&rig), [B]);

    for bad in ["{\"commit\":\"not a commit\"}", "not json"] {
        fs::write(&hold, bad).unwrap();
        assert_eq!(ab.unpin(), unpinned(None), "{bad}");
        assert!(!hold.exists());
        assert_eq!(pinned(&rig), [B]);
    }

    // Another owner's file: the trust of an account that is not the test's.
    fs::write(&hold, format!("{{\"commit\":\"{B}\"}}")).unwrap();
    let uid = fs::metadata(&*rig.dir).unwrap().uid();
    let strict = Ab::new(
        Borrowed(&rig.backend),
        rig.dir.join("state.json"),
        Trust::owned_by(uid + 1, &rig.dir),
        600,
        Box::new(|| NOW),
    )
    .holding(hold.clone());
    assert_eq!(strict.unpin(), unpinned(None));
    assert!(!hold.exists());
    assert_eq!(pinned(&rig), [B]);

    let notes = notes.borrow();
    assert_eq!(notes.len(), 3, "{notes:?}");
    assert!(
        notes.iter().all(
            |n| n.starts_with("unpin: ") && n.ends_with(": nothing is unpinned in retain.json")
        ),
        "{notes:?}"
    );
}

/// An unpin is refused, and changes nothing, when another holds the install lock (named, with
/// what frees it), when another writer holds `retain.json`, and on a host with no hold file. A
/// hold file that cannot be removed (a directory at its name) fails, saying so.
///
/// Catches: an unpin that waits for a lock or runs beside an activate; a refusal that removes
/// the hold file anyway.
#[test]
fn unpin_is_refused_behind_a_lock_and_without_a_hold_file() {
    let rig = Rig::new("unpin-refused");
    keep(&rig, B, Some(VB), 2);
    let (ab, _) = core(&rig);
    assert!(matches!(
        ab.activate(&target(B), PIN),
        Answer::Activated { .. }
    ));
    let hold = agent_dir(&rig).join("pin.json");
    let path = rig.backend.lock_file();

    let held = take_lock(&path).unwrap();
    let answer = ab.unpin();
    assert_eq!(
        answer.json_line(),
        "{\"result\":\"refused\",\"reason\":\"busy\"}"
    );
    assert_eq!(
        answer.log_detail().unwrap(),
        format!(
            "the install lock {} is held by pid {}, which is alive: let it end or stop it",
            path.display(),
            std::process::id()
        )
    );
    drop(held);
    locks::wait_lockable(&path);
    let holder = hold_as(&path, "");
    assert_eq!(
        ab.unpin().log_detail().unwrap(),
        format!(
            "the install lock {} is held by a process that wrote no id: take it with activate \
             --force-lock",
            path.display()
        )
    );
    drop(holder);
    locks::wait_lockable(&path);

    let retain_lock = kbf_retain::RetainLock::acquire(&retain::path(&rig.layout)).unwrap();
    let answer = ab.unpin();
    assert!(
        matches!(&answer, Answer::Refused(Code::Busy, d) if d.ends_with("is held by another writer")),
        "{answer:?}"
    );
    drop(retain_lock);
    assert!(hold.exists());
    assert_eq!(pinned(&rig), [B]);

    // A `retain.json` that cannot be read fails the unpin, and the hold file stays.
    let file = retain::path(&rig.layout);
    let kept = fs::read(&file).unwrap();
    fs::write(&file, "not json").unwrap();
    let answer = ab.unpin();
    assert!(
        matches!(&answer, Answer::Failed(Code::StateUnavailable, d) if d.starts_with("the pin: ")),
        "{answer:?}"
    );
    assert!(hold.exists());
    fs::write(&file, kept).unwrap();

    let bare = Ab::new(
        Borrowed(&rig.backend),
        rig.dir.join("state.json"),
        Trust::owned_by(fs::metadata(&*rig.dir).unwrap().uid(), &rig.dir),
        600,
        Box::new(|| NOW),
    );
    assert_eq!(
        bare.unpin(),
        Answer::Refused(Code::ConfigInvalid, "this host has no hold file".to_owned())
    );

    // A directory at the name names no commit and cannot be removed: nothing is unpinned in
    // `retain.json`, and the unpin fails. (A cut after the retain change is
    // `an_unpin_cut_short_leaves_the_hold_and_is_finished_by_running_it_again`.)
    fs::remove_file(&hold).unwrap();
    fs::create_dir(&hold).unwrap();
    fs::write(hold.join("x"), "in the way").unwrap();
    let answer = ab.unpin();
    assert!(
        matches!(&answer, Answer::Failed(Code::StateUnavailable, d)
            if d.starts_with(&hold.display().to_string())),
        "{answer:?}"
    );
    // A hold file with no name of its own cannot be removed either.
    let nameless = Ab::new(
        Borrowed(&rig.backend),
        rig.dir.join("state.json"),
        Trust::owned_by(fs::metadata(&*rig.dir).unwrap().uid(), &rig.dir),
        600,
        Box::new(|| NOW),
    )
    .holding(agent_dir(&rig).join(".."));
    assert!(matches!(
        nameless.unpin(),
        Answer::Failed(Code::StateUnavailable, d) if d.ends_with("the hold file has no directory or no name")
    ));
}

/// The command as root runs it on a Linux host: it answers, exits 0 and appends its audit line
/// (verb `unpin`, the commit unpinned, then that nothing was); a state directory that cannot be
/// made fails before anything is asked.
///
/// Catches: an unpin that is not audited.
#[test]
fn the_unpin_command_audits_what_it_did_on_a_linux_host() {
    let rig = Rig::new("unpin-cli");
    keep(&rig, B, Some(VB), 2);
    let uid = fs::metadata(&*rig.dir).unwrap().uid();
    let trust = Trust::owned_by(uid, &rig.dir);
    let env = LinuxEnv {
        passwd: rig.dir.join("passwd"),
        state_dir: rig.dir.join("state"),
        state_name: "state.json".to_owned(),
        audit: rig.dir.join("audit.log"),
    };
    let hold = agent_dir(&rig).join("pin.json");
    let args: Vec<String> = format!("kbf={VB} --pin --restart-unit kbf-daemon.service")
        .split(' ')
        .map(str::to_owned)
        .collect();
    let pin = activate_cli::on_linux(
        &Command::parse(&args).unwrap(),
        || Borrowed(&rig.backend),
        &env,
        &trust,
        hold.clone(),
    );
    assert_eq!(pin, 0);
    let unpin = || unpin_cli::on_linux(|| Borrowed(&rig.backend), &env, &trust, hold.clone());
    assert_eq!(unpin(), 0);
    assert!(!hold.exists());
    assert_eq!(unpin(), 0);
    let audit: Vec<serde_json::Value> = fs::read_to_string(&env.audit)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let fields = |n: usize| {
        let mut entry = audit[n].clone();
        entry.as_object_mut().unwrap().remove("time");
        entry
    };
    assert_eq!(
        fields(1),
        serde_json::json!({
            "verb": "unpin", "version": null, "channel": null, "outcome": "unpinned",
            "reason": null, "result": format!("unpinned {B}"),
        })
    );
    assert_eq!(fields(2)["result"], "nothing unpinned");

    let blocked = LinuxEnv {
        state_dir: rig.dir.join("passwd"),
        ..env
    };
    fs::write(&blocked.state_dir, "a file").unwrap();
    fs::set_permissions(&blocked.state_dir, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        unpin_cli::on_linux(|| Borrowed(&rig.backend), &blocked, &trust, hold),
        3
    );
}

const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// A valid unpin takes out the named commit and nothing else: another pin, `last_good` and the
/// recorded versions stay. A commit already gone from `pinned`, and a host with no
/// `retain.json`, unpin all the same, and change nothing more.
///
/// Catches: an unpin that rewrites `retain.json` from scratch and loses another pin or the
/// last good release; one that fails when the retain pin is already gone.
#[test]
fn a_valid_unpin_takes_out_the_named_commit_and_nothing_else() {
    let rig = Rig::new("unpin-only");
    keep(&rig, B, Some(VB), 2);
    let (ab, _) = core(&rig);
    assert!(matches!(
        ab.activate(&target(B), PIN),
        Answer::Activated { .. }
    ));
    let file = retain::path(&rig.layout);
    Retained::update(&file, |r| {
        r.pinned.insert(id(C));
        r.last_good = Some(id(A));
    })
    .unwrap();
    let before = Retained::read(&file).unwrap();
    assert_eq!(ab.unpin(), unpinned(Some(B)));
    let mut expected = before.clone();
    expected.pinned.remove(&id(B));
    assert_eq!(Retained::read(&file).unwrap(), expected);
    assert_eq!(pinned(&rig), [C]);

    // The retain pin already gone (a run cut short after it): the hold file still goes.
    let hold = agent_dir(&rig).join("pin.json");
    super::super::super::core::activate::write_hold_for_test(
        &hold,
        &format!("{{\"commit\":\"{B}\"}}"),
        &|_| {},
    )
    .unwrap();
    assert_eq!(ab.unpin(), unpinned(Some(B)));
    assert!(!hold.exists());
    assert_eq!(Retained::read(&file).unwrap(), expected);

    // No retain.json at all.
    fs::remove_file(&file).unwrap();
    super::super::super::core::activate::write_hold_for_test(
        &hold,
        &format!("{{\"commit\":\"{B}\"}}"),
        &|_| {},
    )
    .unwrap();
    assert_eq!(ab.unpin(), unpinned(Some(B)));
    assert!(!hold.exists());
}

/// The commit is read from the first 4096 bytes only when the file ends there: a good commit
/// padded to the limit is unpinned, the same followed by one byte more names no commit and
/// unpins nothing. Root's file its group or others may write names no commit either.
///
/// Catches: a hold file cut at the limit and read as whole; a commit taken from a file the
/// agent's account could have written into.
#[test]
fn the_hold_file_is_read_whole_and_only_when_no_one_else_may_write_it() {
    let rig = Rig::new("unpin-bounds");
    keep(&rig, B, Some(VB), 2);
    let (ab, notes) = core(&rig);
    let file = retain::path(&rig.layout);
    Retained::update(&file, |r| {
        r.pinned.insert(id(B));
    })
    .unwrap();
    let hold = agent_dir(&rig).join("pin.json");
    let padded = format!("{{\"commit\":\"{B}\"}}{}", " ".repeat(4096));
    let put = |text: &str, mode: u32| {
        fs::write(&hold, text).unwrap();
        fs::set_permissions(&hold, fs::Permissions::from_mode(mode)).unwrap();
    };
    put(&format!("{}x", &padded[..4096]), 0o644);
    assert_eq!(ab.unpin(), unpinned(None));
    assert_eq!(pinned(&rig), [B]);
    for mode in [0o664, 0o646] {
        put(&padded[..4096], mode);
        assert_eq!(ab.unpin(), unpinned(None), "{mode:o}");
        assert_eq!(pinned(&rig), [B]);
    }
    put(&padded[..4096], 0o644);
    assert_eq!(ab.unpin(), unpinned(Some(B)));
    assert!(pinned(&rig).is_empty());
    let notes = notes.borrow();
    assert_eq!(
        notes.as_slice(),
        [
            format!(
                "unpin: {}: over 4096 bytes: nothing is unpinned in retain.json",
                hold.display()
            ),
            format!(
                "unpin: {}: writable by its group or by others: nothing is unpinned in \
                 retain.json",
                hold.display()
            ),
            format!(
                "unpin: {}: writable by its group or by others: nothing is unpinned in \
                 retain.json",
                hold.display()
            ),
        ]
    );
}

/// An unpin cut short after `retain.json` changed and before the hold file went (here the hold
/// file cannot be removed) fails and leaves the hold file, so the agent stays pinned; run again
/// once the name can be removed, it finishes. A file the agent's account puts at the name in
/// that moment is removed with it, and the commit unpinned is still the one root's file named.
///
/// Catches: an unpin that removes the hold file first (a cut leaves the release unprotected
/// and unpinned in retain but the agent free); one whose retain change follows what is at the
/// name when it removes it.
#[test]
fn an_unpin_cut_short_leaves_the_hold_and_is_finished_by_running_it_again() {
    let rig = Rig::new("unpin-cut");
    keep(&rig, B, Some(VB), 2);
    let (ab, _) = core(&rig);
    assert!(matches!(
        ab.activate(&target(B), PIN),
        Answer::Activated { .. }
    ));
    let hold = agent_dir(&rig).join("pin.json");
    let in_the_way = || {
        fs::remove_file(&hold).unwrap();
        fs::create_dir(&hold).unwrap();
        fs::write(hold.join("x"), "in the way").unwrap();
    };
    let answer = ab.unpin_interrupted(&in_the_way);
    assert!(
        matches!(&answer, Answer::Failed(Code::StateUnavailable, d)
            if d.starts_with(&hold.display().to_string())),
        "{answer:?}"
    );
    assert!(pinned(&rig).is_empty(), "retain.json changed first");
    assert!(hold.exists(), "the hold stays: the agent is still pinned");

    fs::remove_dir_all(&hold).unwrap();
    super::super::super::core::activate::write_hold_for_test(
        &hold,
        &format!("{{\"commit\":\"{B}\"}}"),
        &|_| {},
    )
    .unwrap();
    assert_eq!(ab.unpin(), unpinned(Some(B)), "run again, it finishes");
    assert!(!hold.exists());

    // Root's pin of B; in the moment, the agent's account swaps in a file naming C.
    assert!(matches!(
        ab.activate(&target(B), PIN),
        Answer::Activated { .. }
    ));
    Retained::update(&retain::path(&rig.layout), |r| {
        r.pinned.insert(id(C));
    })
    .unwrap();
    let swap = || {
        fs::remove_file(&hold).unwrap();
        fs::write(&hold, format!("{{\"commit\":\"{C}\"}}")).unwrap();
    };
    assert_eq!(ab.unpin_interrupted(&swap), unpinned(Some(B)));
    assert!(!hold.exists());
    assert_eq!(pinned(&rig), [C]);
}
