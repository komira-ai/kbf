//! The spool and the order records on a scratch host: what a file that is not an order does,
//! the cap and the expiry of records, the records `kbf-update` writes read back by this side,
//! the restart command, the confirm window, the order of acting, and the modes of the
//! helper's files.

use super::*;

/// Catches a floor moved by garbage, and a record trusted for what it is not. Each of these
/// is refused, removed and acknowledges nothing, so the real order after it (a lower id)
/// still runs: text that is not JSON; a record with a field the writer never makes; a file
/// named for another order than it holds; a verb that is none of three; a commit that is not
/// a commit; an activation with no release claims; one over the size cap; a directory. A link
/// in the spool is refused as a link and removed as a link; what it points at is untouched.
///
/// Catches: acknowledging an unparsable file's id (the real order is then ignored as
/// old); removing through the link's target (the secret goes); `deny_unknown_fields` dropped
/// (the extra field is accepted).
#[test]
fn a_file_that_is_not_an_order_moves_nothing() {
    let mut rig = Rig::new("garbage");
    let secret = rig.root.join("secret");
    fs::write(&secret, b"not yours").expect("secret");
    let good = rig.rec(1, 1, 1, Verb::Confirm, B);
    let text = serde_json::to_string(&good).expect("json");
    rig.put_raw(1, 900, b"not json");
    rig.put_raw(1, 902, text.replace('}', ",\"path\":\"/etc\"}").as_bytes());
    rig.put_raw(
        1,
        903,
        text.replace("\"order_id\":1", "\"order_id\":9").as_bytes(),
    );
    rig.put_raw(1, 904, text.replace("confirm", "reboot").as_bytes());
    rig.put_raw(1, 905, text.replace(B, "NOT-A-COMMIT").as_bytes());
    let no_claims = serde_json::to_string(&rig.rec(1, 1, 1, Verb::Confirm, B))
        .expect("json")
        .replace("confirm", "activate");
    rig.put_raw(1, 906, no_claims.as_bytes());
    rig.put_raw(
        1,
        907,
        &vec![b' '; (kbf_update::MAX_RECORD_BYTES + 1) as usize],
    );
    fs::create_dir(rig.orders().join(file_name(1, 908))).expect("dir");
    symlink(&secret, rig.orders().join(file_name(1, 901))).expect("link");
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["state"], "refused");
    assert_eq!(rig.status()["acked_order_id"], 0);
    assert_eq!(fs::read(&secret).expect("kept"), b"not yours");
    // All gone but the directory, which cannot be unlinked as a file and is reported.
    assert_eq!(rig.left(), 1);
    assert!(rig.orders().join(file_name(1, 908)).is_dir());
    rig.order(2, Verb::Activate, B);
    fs::remove_dir(rig.orders().join(file_name(1, 908))).expect("clear");
    rig.cycle(T0 + 6);
    assert_eq!(rig.state().current, Some(c(B)));
    assert_eq!(rig.restarts(), 1);
}

/// Catches a symlinked record being followed: the link is refused as a link with the
/// secret behind it never read (the refusal says symlink, not a parse error of the secret's
/// content), and the order it would have been is not acted on.
///
/// Catches: `O_NOFOLLOW` dropped from the open (the secret is read; the reason is a
/// parse error, not a symlink).
#[test]
fn a_symlinked_record_is_refused_as_a_link_and_not_read() {
    let mut rig = Rig::new("symlink");
    let target = rig.root.join("real-record.json");
    fs::write(
        &target,
        serde_json::to_vec(&rig.rec(5, 1, 1, Verb::Activate, B)).expect("json"),
    )
    .expect("record");
    symlink(&target, rig.orders().join(file_name(1, 5))).expect("link");
    rig.cycle(T0);
    assert_eq!(
        rig.current(),
        Some(c(A)),
        "the link's order was not acted on"
    );
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.refusal().to_ascii_lowercase().contains("link"),
        "{}",
        rig.refusal()
    );
    assert!(target.exists(), "the target is untouched");
    assert_eq!(rig.left(), 0, "the link itself is removed");
}

/// Catches an unbounded spool: 70 valid records are in it, only the first 64 (by epoch and
/// id) are acted on and the rest are deleted unread with a reason, so a stuck or hostile
/// writer cannot grow the directory or the work of a pass.
///
/// Catches: the cap raised to 100 (the excess is acted on); the excess left on disk.
#[test]
fn the_spool_holds_at_most_64_records_and_the_excess_is_deleted() {
    let mut rig = Rig::new("count-cap");
    for id in 1..=70 {
        rig.order(id, Verb::Confirm, A);
    }
    rig.cycle(T0);
    assert_eq!(rig.left(), 0, "all taken or deleted");
    assert_eq!(
        rig.status()["acked_order_id"],
        64,
        "the 64th is the last acted on"
    );
    assert!(
        rig.refusal().contains("more than 64 records"),
        "{}",
        rig.refusal()
    );
}

/// Catches a stale record acted on after a long stop, and expiry measured by something the
/// writer controls. A record is good for 1800 s from the moment root first saw it, kept in
/// `helper.json`: one whose file time is ancient but which root saw a second ago is acted on;
/// one root saw 1801 s ago and which is still there is deleted unread.
///
/// Catches: expiry by the file's mtime (the ancient-mtime record is dropped and the
/// stale one acted on); first-seen reset every pass (the stale record is acted on); the bound
/// `>` made `>=` (the record exactly 1800 s old is dropped).
#[test]
fn a_record_expires_by_roots_first_seen_time_not_the_files() {
    let mut rig = Rig::new("expiry");
    rig.order(5, Verb::Activate, B);
    // The writer sets the file's time far in the past.
    let file = fs::File::options()
        .write(true)
        .open(rig.orders().join(file_name(1, 5)))
        .expect("open");
    file.set_modified(std::time::UNIX_EPOCH).expect("mtime");
    drop(file);
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(B)), "root saw it just now: acted on");

    // A record root first sees at T1 and is still in the spool 1801 s later (here: the
    // installer was held, so it was seen but not taken) is expired.
    let mut rig = Rig::new("expiry-stale");
    rig.leases.set(0);
    rig.put(&rig.rec(5, 1, 1, Verb::Activate, B));
    let memory_path = rig.root.join("var/lib/kbf-installer/helper.json");
    // Root has seen it at T0 - 1801 already (as a persisted first-seen time).
    let mut memory: serde_json::Value =
        serde_json::from_slice(&fs::read(&memory_path).expect("memory")).expect("json");
    memory["first_seen"] = serde_json::json!({ file_name(1, 5): T0 - 1801 });
    fs::write(&memory_path, serde_json::to_vec(&memory).expect("json")).expect("seed");
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(A)), "expired: not acted on");
    assert_eq!(rig.fetcher.count(), 0);
    assert!(rig.refusal().contains("expired"), "{}", rig.refusal());
    assert_eq!(rig.left(), 0, "and deleted");

    // Exactly 1800 s is still good: a record expires when it is *more than* that old.
    let mut rig = Rig::new("expiry-edge");
    rig.put(&rig.rec(5, 1, 1, Verb::Activate, B));
    let memory_path = rig.root.join("var/lib/kbf-installer/helper.json");
    let mut memory: serde_json::Value =
        serde_json::from_slice(&fs::read(&memory_path).expect("memory")).expect("json");
    memory["first_seen"] = serde_json::json!({ file_name(1, 5): T0 - 1800 });
    fs::write(&memory_path, serde_json::to_vec(&memory).expect("json")).expect("seed");
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(B)), "1800 s old is not expired");
}

/// Catches an activation for another platform than this host's: the record claims a macOS
/// release on a Linux host, so nothing is fetched.
#[test]
fn an_order_for_another_platform_is_refused_before_the_fetch() {
    let mut rig = Rig::new("platform");
    let mut r = rig.rec(1, 1, 1, Verb::Activate, B);
    r.expected.as_mut().expect("facts").platform = "darwin-arm64".to_owned();
    rig.put(&r);
    rig.cycle(T0);
    assert_eq!(rig.fetcher.count(), 0);
    assert!(rig.refusal().contains("platform"), "{}", rig.refusal());
}

/// Catches a status.json the node cannot read in a state the helper reports: what the helper
/// writes is what `kbf-update` reads, with the facts of the fetched release.
#[test]
fn status_json_is_read_by_the_node_client() {
    let mut rig = Rig::new("status-contract");
    let status = rig.root.join(UPDATE).join("status.json");
    let read = || kbf_update::helper::read(&status);
    rig.cycle(T0);
    let kbf_update::helper::Helper::Reported(idle) = read() else {
        panic!("reported")
    };
    assert_eq!(idle.state, kbf_update::helper::HelperState::Idle);
    assert_eq!(idle.installed.as_deref(), Some(A));
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 6);
    let kbf_update::helper::Helper::Reported(on_trial) = read() else {
        panic!("reported")
    };
    assert_eq!(
        on_trial.state,
        kbf_update::helper::HelperState::ActivatedUnconfirmed
    );
    assert_eq!(on_trial.acked_order_id, 5);
    assert_eq!(on_trial.staged_facts.expect("facts").protocol, 7);
    assert!(on_trial.holds(B) && on_trial.holds(A));
}

/// Catches what the writer makes and the reader refuses disagreeing: the record
/// `kbf-update` builds from an order and an offer is read by this side as the same order,
/// claims and all.
#[test]
fn the_record_kbf_update_writes_is_the_order_this_side_reads() {
    use crate::helper::order::{self, Verb as OrderVerb};
    let offer = kbf_update_offer();
    let order_in = kbf_update::Order {
        order_id: 5,
        epoch: 2,
        generation: 9,
        verb: Some(Verb::Activate),
        commit: B.to_owned(),
        window_s: 600,
        source: Some(Source::Fetch),
    };
    let r = OrderRecord::for_order(&order_in, Verb::Activate, Some(&offer)).expect("record");
    let bytes = r.to_bytes().expect("bytes");
    let (epoch, id) = order::key_of(&r.file_name()).expect("name");
    let read = order::parse(epoch, id, &bytes).expect("read");
    assert_eq!((read.id, read.epoch, read.generation), (5, 2, 9));
    assert_eq!(read.verb, OrderVerb::Activate);
    assert_eq!(read.commit, c(B));
    let claims = read.claims.expect("claims");
    assert_eq!(
        (claims.protocol, claims.min_protocol, claims.state_schema),
        (7, 6, 3)
    );
    assert_eq!(claims.components[0].sha256, "ab".repeat(32));
    // The name must be the pair inside.
    assert!(order::parse(2, 6, &bytes).is_err());
    assert!(order::parse(3, 5, &bytes).is_err());
}

/// Catches the staged record `kbf-update` writes being read by this side as anything but a
/// staged order: the source survives the file, and the record carries no claims although the
/// caller handed the writer an offer.
///
/// Catches: the writer ignoring the order's source; the writer consuming the offer
/// for a staged order.
#[test]
fn the_staged_record_kbf_update_writes_is_the_staged_order_this_side_reads() {
    use crate::helper::order::{self, Verb as OrderVerb};
    let offer = kbf_update_offer();
    let order_in = kbf_update::Order {
        order_id: 5,
        epoch: 2,
        generation: 9,
        verb: Some(Verb::Activate),
        commit: B.to_owned(),
        window_s: 600,
        source: Some(Source::Staged),
    };
    let r = OrderRecord::for_order(&order_in, Verb::Activate, Some(&offer)).expect("record");
    let bytes = r.to_bytes().expect("bytes");
    let (epoch, id) = order::key_of(&r.file_name()).expect("name");
    let read = order::parse(epoch, id, &bytes).expect("read");
    assert_eq!(read.source, Source::Staged);
    assert!(read.claims.is_none());
    assert_eq!(read.verb, OrderVerb::Activate);
}

fn kbf_update_offer() -> kbf_update::Offer {
    kbf_update::Offer {
        generation: 9,
        commit: B.to_owned(),
        platform: "linux-x86_64".to_owned(),
        components: vec![kbf_update::Component {
            name: "kbf".to_owned(),
            sha256: "ab".repeat(32),
            size: 1000,
            attestation: b"{\"bundle\":1}".to_vec(),
        }],
        expected: kbf_update::Expected {
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
            profile: "release".to_owned(),
        },
    }
}

/// Catches a restart that runs the wrong program or the wrong unit: argv is fixed by the
/// helper's own flag, an absolute program, and nothing else.
///
/// Catches: the step returned as `None`.
#[test]
fn the_restart_is_one_fixed_command() {
    let program = |r: Restart| match r.step() {
        Step::Run(cmd) => (cmd.program.as_str().to_owned(), cmd.args),
        other => panic!("not a command: {other:?}"),
    };
    assert_eq!(
        program(Restart::Systemd("kbf.service".to_owned())),
        (
            "/usr/bin/systemctl".to_owned(),
            vec![
                "restart".to_owned(),
                "--".to_owned(),
                "kbf.service".to_owned()
            ]
        )
    );
    assert_eq!(
        program(Restart::Launchd("com.komira.kbf-daemon".to_owned())),
        (
            "/bin/launchctl".to_owned(),
            vec![
                "kickstart".to_owned(),
                "-k".to_owned(),
                "system/com.komira.kbf-daemon".to_owned()
            ]
        )
    );
}

/// Catches a confirm window the order chooses: on macOS the window starts at the swap and
/// the helper clamps the order's `window_s` to its own 120..=1800 seconds.
///
/// Catches: the clamp dropped; the bounds swapped.
#[test]
fn the_window_an_order_asks_for_is_clamped_on_macos() {
    for (asked, kept) in [(5_u32, 120_u64), (600, 600), (99_999, 1800)] {
        let mut rig = Rig::on(&format!("window-{asked}"), Platform::MacOs);
        let mut r = rig.rec(5, 1, 1, Verb::Activate, B);
        r.window_s = asked;
        rig.put(&r);
        rig.cycle(T0 + 3);
        let pending = rig.state().pending_confirm.expect("on trial");
        assert_eq!(pending.deadline_wall, Some(T0 + 3 + kept), "asked {asked}");
    }
}

/// Catches orders taken in the order the directory lists them: three orders in one pass
/// (activate, confirm, revert) end on the old release only if run by (epoch, id).
///
/// Catches: the sort dropped (with the files made in reverse id order).
#[test]
fn orders_are_acted_on_in_id_order() {
    let mut rig = Rig::new("order-sort");
    rig.order(7, Verb::Revert, A);
    rig.order(6, Verb::Confirm, B);
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(rig.status()["acked_order_id"], 7);
    assert_eq!(
        rig.restarts(),
        1,
        "one restart for the pass, onto where it ended"
    );
}

/// Catches a stale order acted on, or refused for the wrong reason. Orders are ordered by
/// (epoch, then id): a gap of two million ids in one epoch is acted on; an order of an older
/// epoch is refused whatever its id; the same pair again, and a lower id of the same epoch,
/// are replays dropped without a word (they move nothing); a newer epoch's lower id is
/// acted on (the restored store counts again).
///
/// Catches: a jump cap on the id (the gap is refused); the epoch ignored (the stale
/// order runs, the newer epoch's lower id is dropped as a replay).
#[test]
fn orders_are_ordered_by_epoch_then_id_and_a_gap_is_no_reason_to_refuse() {
    let mut rig = Rig::new("order-epoch");
    let big = 2_000_000;
    rig.put(&rig.rec(big, 7, 1, Verb::Activate, B));
    rig.cycle(T0 + 3);
    assert_eq!(
        rig.current(),
        Some(c(B)),
        "a gap of two million is acted on"
    );
    assert_eq!(rig.status()["acked_order_id"], big);
    // An older epoch with a higher id: refused, nothing moves.
    rig.put(&rig.rec(big + 5, 6, 1, Verb::Revert, A));
    rig.cycle(T0 + 6);
    assert_eq!(rig.current(), Some(c(B)));
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert!(rig.refusal().contains("epoch"));
    assert_eq!(rig.status()["acked_order_id"], big);
    // The same epoch and the same id again, and a lower id: replays, dropped.
    rig.put(&rig.rec(big, 7, 1, Verb::Revert, A));
    rig.put(&rig.rec(big - 1, 7, 1, Verb::Revert, A));
    rig.cycle(T0 + 9);
    assert_eq!(rig.current(), Some(c(B)));
    // A newer epoch counts from its own start: a lower id is acted on.
    rig.put(&rig.rec(3, 8, 1, Verb::Revert, A));
    rig.cycle(T0 + 12);
    assert_eq!(rig.current(), Some(c(A)));
    assert_eq!(rig.status()["acked_order_id"], 3);
}

/// Catches helper.json readable by the service user, and status.json not (the node reads
/// it as that user). The modes are set by the write itself, not by the umask of whoever
/// runs the helper.
///
/// Catches: either mode swapped.
#[test]
fn the_helpers_files_have_their_modes() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new("modes");
    let mode = |p: &str| {
        fs::metadata(rig.root.join(p))
            .expect("meta")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("var/lib/kbf-installer/helper.json"), 0o600);
    assert_eq!(mode(&format!("{UPDATE}/status.json")), 0o644);
}

/// Catches fetched facts shown for a release that is neither current nor staged (an old
/// fact kept after another release took over): only those bring their protocol numbers.
///
/// Catches: the filter dropped.
#[test]
fn fetched_facts_are_shown_only_for_the_current_or_staged_release() {
    use crate::helper::memory::{Memory, StagedFacts};
    let memory = Memory {
        staged: Some(StagedFacts {
            commit: c(B),
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
        }),
        ..Memory::default()
    };
    let render = |current: &str, staged: Option<&str>| {
        let state = State {
            current: Some(c(current)),
            staged: staged.map(c),
            ..State::default()
        };
        let json: serde_json::Value =
            serde_json::from_slice(&crate::helper::status::render(&state, &memory)).expect("json");
        json
    };
    assert_eq!(render(A, Some(B))["staged"]["protocol"], 7);
    assert_eq!(render(B, None)["staged"]["protocol"], 7);
    assert!(render(A, None)["staged"].is_null());
    assert!(render(A, Some(A))["staged"].is_null());
}

/// Catches a helper that dies on a spool it cannot make sense of: no spool, an empty one,
/// names that are not records (the client's state file, a half-written `.partial-` file).
/// Each is quietly skipped and left.
#[test]
fn a_missing_spool_and_names_that_are_not_records_are_skipped() {
    let mut rig = Rig::new("odd-spool");
    fs::write(rig.orders().join("client.json"), b"{}").expect("state");
    fs::write(rig.orders().join(".partial-abc.json"), b"{").expect("partial");
    rig.cycle(T0);
    assert_eq!(rig.status()["state"], "idle");
    assert_eq!(rig.left(), 2, "left alone");
    fs::remove_dir_all(rig.orders()).expect("remove");
    rig.cycle(T0 + 3);
}

/// Catches the helper's own files being wrong ending the run in silence or in a guess:
/// a state.json that cannot be read stops the pass with an error, a helper.json that cannot
/// be read or parsed stops the pass too.
#[test]
fn unreadable_state_is_reported_and_unreadable_memory_stops_the_pass() {
    let mut rig = Rig::new("unreadable");
    rig.helper.recover().expect("recovers");
    fs::write(
        rig.root.join("var/lib/kbf-installer/state.json"),
        b"not json",
    )
    .expect("ruin");
    let error = rig.helper.cycle(T0).expect_err("cannot read the state");
    assert!(error.to_string().contains("host state"), "{error}");
    rig.helper.recover().expect("recovers into a refusal");
    let memory =
        fs::read_to_string(rig.root.join("var/lib/kbf-installer/helper.json")).expect("memory");
    assert!(memory.contains("refused"), "{memory}");
    let path = rig.root.join("var/lib/kbf-installer/helper.json");
    fs::write(&path, b"not json").expect("ruin");
    let error = rig.helper.cycle(T0).expect_err("cannot parse it");
    assert!(error.to_string().contains("helper.json"), "{error}");
    fs::remove_file(&path).expect("remove");
    fs::create_dir(&path).expect("a directory where the file belongs");
    let error = rig.helper.cycle(T0).expect_err("cannot read it");
    assert!(error.to_string().contains("directory"), "{error}");
}

/// Catches orders that claim more than they can: an activate of the release already running
/// changes nothing and restarts nothing; a confirm with nothing on trial is refused.
#[test]
fn orders_the_host_cannot_carry_out_are_refused_and_change_nothing() {
    let mut rig = Rig::new("cannot");
    rig.order(5, Verb::Activate, A);
    rig.cycle(T0);
    assert_eq!(rig.restarts(), 0);
    assert_eq!(rig.fetcher.count(), 0, "it is the running one");
    assert_eq!(rig.status()["acked_order_id"], 5);
    rig.order(7, Verb::Confirm, B);
    rig.cycle(T0 + 6);
    assert_eq!(rig.status()["state"], "refused");
    assert_eq!(rig.restarts(), 0);
}

/// Catches a revert that cannot happen being hidden: the swap's window runs out but the
/// release to go back to is gone; the helper keeps the swap, says why in status.json, and
/// does not restart onto nothing.
#[test]
fn an_expiry_that_cannot_revert_is_reported() {
    let mut rig = Rig::new("expire-fails");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 3);
    let layout = rig.layout.clone();
    activate::boot_check(
        rig.helper.exec_mut(),
        &layout,
        T0 + 10,
        600,
        &mut Vec::new(),
    )
    .expect("starts");
    fs::remove_file(
        rig.root
            .join("var/lib/kbf-installer/releases")
            .join(A)
            .join("kbf"),
    )
    .expect("gone");
    let before = rig.restarts();
    rig.cycle(T0 + 10 + 601);
    assert_eq!(rig.state().current, Some(c(B)));
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert!(!rig.refusal().is_empty());
    assert_eq!(rig.restarts(), before);
}

/// Catches an epoch that wedges the helper: an order more than a thousand epochs above the
/// last acted on is refused with a reason in status.json and NOT acknowledged, and the next
/// real order still runs. The first order a host acts on is measured from nothing.
///
/// Catches: the check dropped; the first-order exemption dropped.
#[test]
fn an_epoch_far_above_the_last_is_refused_without_moving_the_floor() {
    let mut rig = Rig::new("epoch-jump");
    rig.put(&rig.rec(1, 5000, 1, Verb::Activate, B));
    rig.cycle(T0 + 3);
    assert_eq!(rig.current(), Some(c(B)));
    assert_eq!(rig.status()["acked_order_id"], 1);
    rig.put(&rig.rec(
        2,
        5000 + crate::helper::order::MAX_EPOCH_JUMP + 1,
        1,
        Verb::Revert,
        A,
    ));
    rig.cycle(T0 + 6);
    assert_eq!(rig.current(), Some(c(B)));
    let refusal = rig.refusal();
    assert!(
        refusal.contains("more than") && refusal.contains("epoch"),
        "{refusal}"
    );
    assert_eq!(rig.status()["acked_order_id"], 1);
    rig.put(&rig.rec(
        3,
        5000 + crate::helper::order::MAX_EPOCH_JUMP,
        1,
        Verb::Revert,
        A,
    ));
    rig.cycle(T0 + 9);
    assert_eq!(rig.current(), Some(c(A)));
    assert_eq!(rig.status()["acked_order_id"], 3);
}

/// Catches an activation named by its id alone: the same commit ordered under the same id
/// in two epochs must be two different activations. With a swap of the first still on trial,
/// the second is refused (a trial is pending) and restarts nothing.
///
/// Catches: `order_id: order.id.to_string()` (the second passes for the first's repeat).
#[test]
fn the_same_id_in_two_epochs_is_two_activations() {
    let mut rig = Rig::new("same-id");
    rig.put(&rig.rec(5, 7, 1, Verb::Activate, B));
    rig.cycle(T0 + 3);
    assert_eq!(rig.restarts(), 1);
    let pending = rig.state().pending_confirm.expect("on trial");
    assert_eq!(pending.activation, "7-5");
    rig.put(&rig.rec(5, 8, 1, Verb::Activate, B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.restarts(), 1, "not the first one's repeat");
    assert!(rig.refusal().contains("on trial"), "{}", rig.refusal());
    assert_eq!(rig.status()["acked_order_id"], 5);
}

/// Catches a revert with nothing to go back to being reported as done: a host that never
/// swapped has no previous release, so the order is refused with the installer's reason and
/// nothing restarts.
#[test]
fn a_revert_with_nothing_to_go_back_to_is_refused() {
    let mut rig = Rig::new("revert-nothing");
    rig.order(5, Verb::Revert, A);
    rig.cycle(T0);
    assert_eq!(rig.status()["state"], "refused");
    assert_eq!(rig.status()["acked_order_id"], 5);
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(rig.restarts(), 0);
}
