use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use kbf_host::path::Root;
use kbf_host::testkit::ScratchHost;

use super::*;
use crate::activate::{self, Arm, Boot, Order};
use crate::state::{Locked, State};
use crate::testkit::{A, B, C, c};

const LOG_DIR: &str = "var/lib/kbf-meta";
/// What an older binary needs to see in the log to start, and what the new one leaves.
const OLD: &[u8] = b"machine version 4\n";
const RAISED: &[u8] = b"machine version 5\n";
const START: Arm = Arm::FromFirstStart { starts: 3 };

/// A host that runs `A` with `B` installed beside it, on a storage node's `kbf.toml` that names
/// its metadata log, which holds a record, a segment in a directory and a file only its owner
/// reads.
struct Rig {
    exec: ScratchHost,
    layout: Layout,
}

fn rig(name: &str) -> Rig {
    let exec = ScratchHost::new(&format!("metalog-{name}"));
    for sub in [
        "run",
        "etc/kbf",
        "var/lib/kbf-installer/releases",
        LOG_DIR,
        &format!("{LOG_DIR}/raft"),
    ] {
        fs::create_dir_all(exec.root.join(sub)).expect("seed");
    }
    for release in [A, B, C] {
        let dir = exec
            .root
            .join("var/lib/kbf-installer/releases")
            .join(release);
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), release).expect("kbf");
    }
    let layout = Layout::linux(Root::new(&exec.root).expect("absolute"));
    Locked::acquire(&layout)
        .expect("lock")
        .write(&State {
            current: Some(c(A)),
            ..State::default()
        })
        .expect("state");
    // The state first: a link with no state is a lost state.
    symlink(A, exec.root.join("var/lib/kbf-installer/releases/current")).expect("link");
    let rig = Rig { exec, layout };
    rig.kbf_toml("mode = \"storage\"\n[storage]\nmeta_dir = \"/var/lib/kbf-meta\"\n");
    rig.put("RECORD", OLD, 0o640);
    rig.put("raft/segment-1", b"entry one\nentry two\n", 0o600);
    rig.put("raft/LOCK", b"", 0o600);
    // Longer than the buffer the comparison reads with.
    rig.put("raft/big", &vec![7u8; 150_000], 0o600);
    rig
}

impl Rig {
    fn kbf_toml(&self, text: &str) {
        fs::write(self.exec.root.join("etc/kbf/kbf.toml"), text).expect("kbf.toml");
    }

    fn log(&self) -> PathBuf {
        self.exec.root.join(LOG_DIR)
    }

    fn put(&self, name: &str, bytes: &[u8], mode: u32) {
        let path = self.log().join(name);
        fs::write(&path, bytes).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("mode");
    }

    /// The log as the new release leaves it: the record raised, an entry more, a file new.
    fn run_new_release(&self) {
        self.put("RECORD", RAISED, 0o640);
        self.put(
            "raft/segment-1",
            b"entry one\nentry two\nentry three\n",
            0o600,
        );
        self.put("raft/segment-2", b"new\n", 0o600);
    }

    /// Whether a binary that runs only machine version 4 can open the log.
    fn old_binary_starts(&self) -> bool {
        fs::read(self.log().join("RECORD")).is_ok_and(|b| b == OLD)
    }

    fn activate_b(&mut self) {
        let order = Order {
            commit: c(B),
            order_id: "o1".to_owned(),
        };
        activate::activate(
            &mut self.exec,
            &self.layout,
            &order,
            START,
            1_000,
            &mut Vec::new(),
        )
        .expect("activate");
    }

    fn boot(&mut self, now: u64) -> Boot {
        activate::boot_check(&mut self.exec, &self.layout, now, 600, &mut Vec::new())
            .expect("boot check")
    }

    fn backup(&self, holder: &str) -> PathBuf {
        backup_of(&self.layout, &c(holder))
    }

    fn state(&self) -> State {
        crate::state::read_unlocked(&self.layout)
            .expect("read")
            .expect("a state")
    }
}

/// Every file under `dir` by relative path, with its bytes and mode; directories by path with
/// their mode and no bytes.
fn tree(dir: &Path) -> BTreeMap<String, (Option<Vec<u8>>, u32)> {
    fn walk(root: &Path, at: &Path, into: &mut BTreeMap<String, (Option<Vec<u8>>, u32)>) {
        for item in fs::read_dir(at).expect("list") {
            let path = item.expect("item").path();
            let meta = fs::symlink_metadata(&path).expect("meta");
            let name = path
                .strip_prefix(root)
                .expect("below")
                .display()
                .to_string();
            let mode = meta.mode() & 0o7777;
            if meta.is_dir() {
                into.insert(name.clone(), (None, mode));
                walk(root, &path, into);
            } else if meta.is_symlink() {
                into.insert(name, (None, mode));
            } else {
                into.insert(name, (Some(fs::read(&path).expect("read")), mode));
            }
        }
    }
    let mut into = BTreeMap::new();
    walk(dir, dir, &mut into);
    into
}

/// Start the release on trial until the start check gives it up.
fn run_out_the_starts(rig: &mut Rig) -> Boot {
    for now in 2_000..2_010 {
        let done = rig.boot(now);
        if matches!(done, Boot::Reverted(_)) {
            return done;
        }
    }
    panic!("the release on trial was never reverted");
}

/// Catches a release that starts on a log nobody saved: the first start of a release on trial
/// saves the log as the release that ran left it, in that release's directory, with the
/// release it was saved for, and every file with its mode.
///
/// Catches: the save not made at the start; the modes not carried; the copy made
/// from the wrong directory.
#[test]
fn the_first_start_saves_the_log_in_the_older_releases_directory() {
    let mut rig = rig("first");
    let before = tree(&rig.log());
    rig.activate_b();
    assert!(
        !rig.backup(A).exists(),
        "the activation moves links; the log is saved at the start"
    );
    assert_eq!(
        rig.boot(1_100),
        Boot::Start {
            starts_left: Some(2)
        }
    );
    let backup = rig.backup(A);
    assert_eq!(
        fs::read_to_string(backup.join("for")).expect("for"),
        format!("{B}\n")
    );
    assert_eq!(tree(&backup.join("log")), before);
    assert_eq!(
        before["RECORD"],
        (Some(OLD.to_vec()), 0o640),
        "the fixture has the modes the test checks"
    );
    assert!(!backup.with_file_name(BACKUP_TMP).exists());
}

/// Catches a log saved after the release has already changed it: a later start of the same
/// release (its log raised, the copy lost) must not make a copy of the raised log, which
/// would be taken for the one before.
///
/// Catches: the save made at every start, not the first.
#[test]
fn a_later_start_does_not_save_a_log_the_release_has_changed() {
    let mut rig = rig("later");
    rig.activate_b();
    rig.boot(1_100);
    rig.run_new_release();
    fs::remove_dir_all(rig.backup(A)).expect("lose the copy");
    assert_eq!(
        rig.boot(1_200),
        Boot::Start {
            starts_left: Some(1)
        }
    );
    assert!(!rig.backup(A).exists());
}

/// Catches a rollback that cannot start: a release that raised the log is reverted at the end
/// of its starts; the log is as the older release left it before that one starts, nothing the
/// newer release added is left, and an older binary opens it. Before, it could not.
///
/// Catches: the log not put back after a revert; the files the new release added left
/// in place; the modes not carried back.
#[test]
fn a_revert_puts_the_log_back_before_the_older_release_starts() {
    let mut rig = rig("revert");
    let before = tree(&rig.log());
    rig.activate_b();
    rig.boot(1_100);
    rig.run_new_release();
    assert!(
        !rig.old_binary_starts(),
        "a raised log is refused by the older binary"
    );
    let done = run_out_the_starts(&mut rig);
    assert_eq!(
        done,
        Boot::Reverted(activate::Reverted {
            from: Some(c(B)),
            to: c(A)
        })
    );
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(tree(&rig.log()), before);
    assert!(rig.old_binary_starts());
    assert!(
        !rig.backup(A).exists(),
        "the copy is spent once it is put back"
    );
}

/// Catches a rollback of a swap that was confirmed (no trial, no pending): the next start of the
/// older release puts the log back.
///
/// Catches: the log put back only in the pending branch of the start check.
#[test]
fn a_rollback_of_a_confirmed_release_puts_the_log_back_at_the_next_start() {
    let mut rig = rig("rollback");
    let before = tree(&rig.log());
    rig.activate_b();
    rig.boot(1_100);
    rig.run_new_release();
    activate::confirm(&rig.layout, "o1", &c(B)).expect("confirm");
    let lock = Locked::acquire(&rig.layout).expect("lock");
    let state = lock.read().expect("read").expect("state");
    activate::rollback_in(&mut rig.exec, &rig.layout, &lock, state, &mut Vec::new())
        .expect("rollback");
    drop(lock);
    assert!(!rig.old_binary_starts());
    assert_eq!(rig.boot(3_000), Boot::Nothing);
    assert_eq!(tree(&rig.log()), before);
    assert!(rig.old_binary_starts());
}

/// Catches a log rewritten for nothing: the swap is reverted before the release started, so
/// the log is as it was, and it is left alone (not one file written again).
///
/// Catches: the log put back whether or not it differs.
#[test]
fn a_release_that_never_started_leaves_the_log_untouched() {
    let rig = rig("never");
    save(&rig.layout, &c(A), &c(B)).expect("save");
    let inode = |rig: &Rig| fs::metadata(rig.log().join("RECORD")).expect("meta").ino();
    let before = inode(&rig);
    assert_eq!(
        restore_owed(&rig.layout, &c(A)).expect("restore"),
        Restored::Unchanged
    );
    assert_eq!(inode(&rig), before);
    assert!(!rig.backup(A).exists());
}

/// Catches an activation that goes on without the copy it needs: a log that cannot be copied
/// (a link in it) reverts the swap at the first start, before the release runs; the reason is
/// left for the helper, nothing is left half made, and the log is as it was.
///
/// Catches: the failure ignored; the swap not reverted; the reason not left.
#[test]
fn a_log_that_cannot_be_saved_reverts_the_swap_before_the_release_starts() {
    let mut rig = rig("refuse");
    symlink("/etc/passwd", rig.log().join("raft/odd")).expect("link");
    let before = tree(&rig.log());
    rig.activate_b();
    let mut out = Vec::new();
    let done = activate::boot_check(&mut rig.exec, &rig.layout, 1_100, 600, &mut out).expect("b");
    assert_eq!(
        done,
        Boot::Reverted(activate::Reverted {
            from: Some(c(B)),
            to: c(A)
        })
    );
    let state = rig.state();
    assert_eq!((state.current, state.pending_confirm), (Some(c(A)), None));
    let why = refusal(&rig.layout).expect("a reason");
    assert!(
        why.starts_with(&format!("activation of {} refused", &B[..12])),
        "{why}"
    );
    assert!(why.contains("raft/odd"), "{why}");
    assert!(String::from_utf8_lossy(&out).contains("metadata log:"));
    assert!(!rig.backup(A).exists());
    assert!(!rig.backup(A).with_file_name(BACKUP_TMP).exists());
    assert_eq!(tree(&rig.log()), before);
    clear_refusal(&rig.layout);
    assert_eq!(refusal(&rig.layout), None);
}

/// Catches a rerun that overwrites the copy: a second save for the same transition keeps the
/// first, though the log has changed since.
///
/// Catches: the existing copy not recognised.
#[test]
fn a_second_save_for_the_same_transition_keeps_the_first_copy() {
    let rig = rig("again");
    assert_eq!(save(&rig.layout, &c(A), &c(B)).expect("save"), Saved::Taken);
    rig.run_new_release();
    assert_eq!(save(&rig.layout, &c(A), &c(B)).expect("save"), Saved::Kept);
    assert_eq!(
        fs::read(rig.backup(A).join("log/RECORD")).expect("copy"),
        OLD,
        "the copy is the log before"
    );
}

/// Catches a copy of a raised log taken for the one before: when the copy that is there is for
/// another release (its restore never ran), the log is put back first, then copied.
///
/// Catches: the older copy replaced without being put back.
#[test]
fn a_copy_owed_for_another_release_is_put_back_before_a_new_one_is_made() {
    let rig = rig("owed");
    save(&rig.layout, &c(A), &c(C)).expect("save for C");
    rig.run_new_release();
    assert_eq!(
        save(&rig.layout, &c(A), &c(B)).expect("save for B"),
        Saved::Taken
    );
    assert_eq!(
        fs::read(rig.backup(A).join("log/RECORD")).expect("copy"),
        OLD
    );
    assert_eq!(
        fs::read(rig.backup(A).join("for")).expect("for"),
        format!("{B}\n").into_bytes()
    );
    assert!(rig.old_binary_starts());
}

/// Catches the two states a kill between the copy and the state write leaves: a copy being
/// built (under its temporary name) is removed and made again, and a copy that is whole is
/// kept; either way the revert that follows puts the log back.
///
/// Catches: the temporary copy not removed first; a whole copy replaced.
#[test]
fn a_kill_between_the_copy_and_the_state_converges() {
    // Killed while building: a half-made copy under the temporary name.
    let mut rig = rig("kill-building");
    let before = tree(&rig.log());
    rig.activate_b();
    let tmp = rig.backup(A).with_file_name(BACKUP_TMP);
    fs::create_dir_all(tmp.join("log")).expect("half a copy");
    fs::write(tmp.join("log/junk"), b"x").expect("junk");
    rig.boot(1_100);
    assert!(!tmp.exists());
    assert_eq!(tree(&rig.backup(A).join("log")), before);

    // Killed after the copy was whole and before the state moved: the start check runs again.
    let mut rig = rig_with_copy_made("kill-copied");
    rig.run_new_release();
    rig.boot(1_100);
    assert_eq!(
        fs::read(rig.backup(A).join("log/RECORD")).expect("copy"),
        OLD
    );
    assert!(matches!(run_out_the_starts(&mut rig), Boot::Reverted(_)));
    assert!(rig.old_binary_starts());
}

/// A rig on which the start check saved the log and was killed before the state moved: the
/// copy is whole, the first start is not recorded.
fn rig_with_copy_made(name: &str) -> Rig {
    let mut rig = rig(name);
    rig.activate_b();
    save(&rig.layout, &c(A), &c(B)).expect("save");
    rig
}

/// Catches copies that pile up: making one removes the others, a link in `releases/` is not
/// followed to remove one, and `gc` takes the copy with its release.
///
/// Catches: no prune; the prune following `previous`; the copy kept outside the release.
#[test]
fn only_one_copy_is_kept_and_gc_takes_it_with_its_release() {
    let mut rig = rig("prune");
    save(&rig.layout, &c(C), &c(B)).expect("save in C");
    assert!(rig.backup(C).exists());
    symlink(
        A,
        rig.exec
            .root
            .join("var/lib/kbf-installer/releases/previous"),
    )
    .expect("link");
    assert_eq!(
        save(&rig.layout, &c(A), &c(B)).expect("save in A"),
        Saved::Taken
    );
    assert!(!rig.backup(C).exists(), "the older copy is gone");
    assert!(
        rig.backup(A).exists(),
        "a link to the release is not followed"
    );

    // A later swap makes the one in B and removes A's.
    save(&rig.layout, &c(B), &c(C)).expect("save in B");
    assert!(!rig.backup(A).exists());

    // `gc` removes a release that nothing names, its copy with it.
    Locked::acquire(&rig.layout)
        .expect("lock")
        .write(&State {
            current: Some(c(B)),
            ..State::default()
        })
        .expect("state");
    fs::remove_file(
        rig.exec
            .root
            .join("var/lib/kbf-installer/releases/previous"),
    )
    .expect("rm");
    fs::remove_file(rig.exec.root.join("var/lib/kbf-installer/releases/current")).expect("rm");
    symlink(
        B,
        rig.exec.root.join("var/lib/kbf-installer/releases/current"),
    )
    .expect("link");
    save(&rig.layout, &c(C), &c(A)).expect("save in C");
    assert!(rig.backup(C).exists());
    // `gc` keeps the two newest others; two newer releases push C out of them.
    for newer in ["1", "2"] {
        let dir = rig
            .exec
            .root
            .join("var/lib/kbf-installer/releases")
            .join(newer.repeat(40));
        fs::create_dir_all(&dir).expect("newer release");
        fs::File::open(&dir)
            .expect("open")
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000))
            .expect("mtime");
    }
    crate::release::gc(
        &mut rig.exec,
        &rig.layout,
        0,
        &std::collections::BTreeSet::new(),
        kbf_host::plan::Mode::Apply,
        &mut Vec::new(),
    )
    .expect("gc");
    assert!(
        !rig.exec
            .root
            .join("var/lib/kbf-installer/releases")
            .join(C)
            .exists()
    );
}

/// Catches a node with no log yet taken for one with a log: the copy records that there was
/// none, and putting it back leaves the directory empty (a release that made the log is
/// reverted to the state where an older binary makes its own).
///
/// Catches: an absent log treated as an error.
#[test]
fn a_node_that_had_no_log_gets_none_back() {
    let rig = rig("nolog");
    fs::remove_dir_all(rig.log()).expect("no log");
    assert_eq!(save(&rig.layout, &c(A), &c(B)).expect("save"), Saved::Taken);
    assert!(!rig.backup(A).join("log").exists());
    fs::create_dir_all(rig.log()).expect("made by the new release");
    rig.put("RECORD", RAISED, 0o640);
    assert_eq!(
        restore_owed(&rig.layout, &c(A)).expect("restore"),
        Restored::Put { from: c(B) }
    );
    assert!(rig.log().is_dir());
    assert_eq!(tree(&rig.log()), BTreeMap::new());
}

/// Catches a log that went missing: a copy with contents is put back where the directory is
/// gone, with the directory's own mode.
///
/// Catches: the directory not made.
#[test]
fn a_log_directory_that_is_gone_is_made_again() {
    let rig = rig("gone");
    fs::set_permissions(rig.log(), fs::Permissions::from_mode(0o750)).expect("mode");
    let before = tree(&rig.log());
    save(&rig.layout, &c(A), &c(B)).expect("save");
    fs::remove_dir_all(rig.log()).expect("lost");
    restore_owed(&rig.layout, &c(A)).expect("restore");
    assert_eq!(tree(&rig.log()), before);
    assert_eq!(
        fs::metadata(rig.log()).expect("meta").mode() & 0o7777,
        0o750
    );
}

/// Catches the host's mode deciding whether and where the log is: each mode's `kbf.toml`
/// gives the directory its node reads, and a file that names none, a path that is no path or
/// `/` is refused rather than guessed.
///
/// Catches: the default state directory dropped; a mode without a log given one.
#[test]
fn the_log_is_where_the_modes_kbf_toml_names_it() {
    let rig = rig("dir");
    let at = |text: &str| {
        rig.kbf_toml(text);
        dir(&rig.layout)
    };
    let under = |p: &str| rig.exec.root.join(p.trim_start_matches('/'));
    assert_eq!(
        at("mode = \"all-in-one\"\n[stores]\nmeta-dir = \"/m/a\"\n").expect("ok"),
        Some(under("/m/a"))
    );
    assert_eq!(
        at("mode = \"storage\"\n").expect("ok"),
        Some(under("/var/lib/kbf/meta"))
    );
    assert_eq!(
        at("mode = \"storage-and-daemon\"\n[dirs]\nstate = \"/srv/kbf/\"\n").expect("ok"),
        Some(under("/srv/kbf/meta"))
    );
    assert_eq!(
        at("mode = \"storage\"\n[dirs]\nstate = \"/srv\"\n[storage]\nmeta_dir = \"/x\"\n")
            .expect("ok"),
        Some(under("/x"))
    );
    for none in ["mode = \"frontend\"\n", "mode = \"daemon\"\n", "x = 1\n"] {
        assert_eq!(at(none).expect("ok"), None, "{none}");
    }
    for bad in [
        "mode = \"all-in-one\"\n",
        "mode = \"storage\"\n[storage]\nmeta_dir = \"relative\"\n",
        "mode = \"storage\"\n[storage]\nmeta_dir = \"/\"\n",
        "not toml [",
    ] {
        assert!(matches!(at(bad), Err(MetaLogError::Dir(_))), "{bad}");
    }
    fs::remove_file(rig.exec.root.join("etc/kbf/kbf.toml")).expect("rm");
    assert_eq!(dir(&rig.layout).expect("ok"), None);
}

/// Catches a host with no log taking part: a mode that keeps none saves nothing, and its start
/// check goes on as before.
///
/// Catches: a copy made whatever the mode.
#[test]
fn a_mode_with_no_log_saves_nothing() {
    let mut rig = rig("nolog-mode");
    rig.kbf_toml("mode = \"daemon\"\n");
    assert_eq!(save(&rig.layout, &c(A), &c(B)).expect("save"), Saved::NoLog);
    rig.activate_b();
    assert_eq!(
        rig.boot(1_100),
        Boot::Start {
            starts_left: Some(2)
        }
    );
    assert!(!rig.backup(A).exists());
}

/// Catches errors made into silence: a copy that names no release, a copy held where the
/// `kbf.toml` names no log, and a log that is not a directory are refused with the reason.
///
/// Catches: an unreadable `for` taken as no copy; a missing log directory name ignored.
#[test]
fn a_copy_that_cannot_be_read_or_placed_is_refused() {
    let rig = rig("bad-copy");
    save(&rig.layout, &c(A), &c(B)).expect("save");
    fs::write(rig.backup(A).join("for"), "not a commit\n").expect("garbage");
    assert!(matches!(
        restore_owed(&rig.layout, &c(A)),
        Err(MetaLogError::For(_))
    ));
    fs::write(rig.backup(A).join("for"), format!("{B}\n")).expect("for");
    rig.kbf_toml("mode = \"daemon\"\n");
    let why = restore_owed(&rig.layout, &c(A))
        .expect_err("no log named")
        .to_string();
    assert!(why.contains("names none"), "{why}");

    rig.kbf_toml("mode = \"storage\"\n[storage]\nmeta_dir = \"/var/lib/kbf-meta-file\"\n");
    fs::write(rig.exec.root.join("var/lib/kbf-meta-file"), b"x").expect("a file");
    assert!(matches!(
        save(&rig.layout, &c(C), &c(B)),
        Err(MetaLogError::Kind(_))
    ));
    assert!(!rig.backup(C).exists());
    assert!(!rig.backup(C).with_file_name(BACKUP_TMP).exists());
}

/// Catches a restore that leaves a file where a directory was, or the reverse, and one that
/// compares only names: a log that differs in a file's bytes, in its kind, or in a name is put
/// back.
///
/// Catches: names compared and not bytes; lengths compared and not bytes; kinds not
/// compared.
#[test]
fn a_log_that_differs_in_bytes_kind_or_name_is_put_back() {
    for change in 0..5 {
        let rig = rig(&format!("differs-{change}"));
        save(&rig.layout, &c(A), &c(B)).expect("save");
        let raft = rig.log().join("raft");
        match change {
            // Same length, other bytes.
            0 => fs::write(raft.join("segment-1"), b"entry one\nentry TWO\n").expect("w"),
            // A file made a directory.
            1 => {
                fs::remove_file(raft.join("segment-1")).expect("rm");
                fs::create_dir(raft.join("segment-1")).expect("dir");
            }
            // A name more.
            2 => fs::write(raft.join("extra"), b"").expect("w"),
            // A directory made a file.
            3 => {
                fs::remove_dir_all(&raft).expect("rm");
                fs::write(&raft, b"x").expect("w");
            }
            // A file cut short, past the buffer.
            _ => fs::write(raft.join("big"), vec![7u8; 140_000]).expect("w"),
        }
        assert!(
            matches!(
                restore_owed(&rig.layout, &c(A)).expect("restore"),
                Restored::Put { .. }
            ),
            "change {change}"
        );
        assert!(rig.old_binary_starts());
        assert_eq!(
            fs::read(raft.join("segment-1")).expect("back"),
            b"entry one\nentry two\n"
        );
    }
}

/// Catches a start check that assumes a state: a host with none (nothing installed yet) has no
/// release that could hold a copy, and the start goes on.
///
/// Catches: the missing release unwrapped.
#[test]
fn a_host_with_no_state_has_nothing_to_put_back() {
    let mut rig = rig("empty");
    // No state and no link: a link with no state is a lost state.
    let installer = rig.exec.root.join("var/lib/kbf-installer");
    fs::remove_file(installer.join("releases/current")).expect("rm");
    fs::remove_file(installer.join("state.json")).expect("rm");
    assert_eq!(rig.boot(1_000), Boot::Nothing);
}

/// Catches a first release saved against nothing: a swap with no release before it (a host
/// that had none current) has no older binary to go back to and no log to keep for it.
///
/// Catches: a copy made in the directory of the release itself.
#[test]
fn a_swap_with_no_older_release_saves_nothing() {
    let mut rig = rig("first-release");
    Locked::acquire(&rig.layout)
        .expect("lock")
        .write(&State::default())
        .expect("state");
    rig.activate_b();
    assert_eq!(rig.state().pending_confirm.expect("pending").previous, None);
    assert_eq!(
        rig.boot(1_100),
        Boot::Start {
            starts_left: Some(2)
        }
    );
    for release in [A, B, C] {
        assert!(!rig.backup(release).exists(), "{release}");
    }
    assert_eq!(refusal(&rig.layout), None);
}

/// Catches a restore that takes a link for the log's directory: a log that was replaced by a
/// link is refused, not emptied through the link.
///
/// Catches: the log's kind not checked before it is emptied.
#[test]
fn a_log_that_became_a_link_is_not_emptied() {
    let rig = rig("linked");
    save(&rig.layout, &c(A), &c(B)).expect("save");
    let elsewhere = rig.exec.root.join("var/lib/elsewhere");
    fs::create_dir_all(&elsewhere).expect("dir");
    fs::write(elsewhere.join("keep"), b"mine").expect("file");
    fs::remove_dir_all(rig.log()).expect("rm");
    symlink(&elsewhere, rig.log()).expect("link");
    assert!(matches!(
        restore_owed(&rig.layout, &c(A)),
        Err(MetaLogError::Kind(_))
    ));
    assert_eq!(fs::read(elsewhere.join("keep")).expect("kept"), b"mine");
    assert!(
        rig.backup(A).exists(),
        "the copy stays until it is put back"
    );
}

/// Catches a release directory that cannot be looked into taken for a release with no copy: a
/// start would then go on to run an older binary on a raised log. It is an error naming the
/// path.
///
/// Catches: any error looking for the copy taken for its absence.
#[test]
fn a_copy_that_cannot_be_looked_for_is_an_error_not_an_absent_one() {
    let rig = rig("not-a-dir");
    let release = rig.exec.root.join("var/lib/kbf-installer/releases").join(C);
    fs::remove_dir_all(&release).expect("rm");
    fs::write(&release, b"a file where the release belongs").expect("file");
    let why = restore_owed(&rig.layout, &c(C))
        .expect_err("an error")
        .to_string();
    assert!(why.contains(C), "{why}");
}
