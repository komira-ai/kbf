//! The helper's passes on a scratch host: the real installer verbs over a real file tree,
//! with the fetch played by a fake that installs a directory (or fails as told), the lease
//! source set by the test, and the service restart recorded as a step. Orders are the
//! records `kbf-update` writes into the spool.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use kbf_host::testkit::{ScratchHost, reply};
use kbf_host::{Root, Step};
use kbf_update::record::file_name;
use kbf_update::{Facts, Locator, OrderRecord, Verb};

use crate::activate;
use crate::layout::{Layout, Platform};
use crate::state::{Locked, State};
use crate::testkit::{A, B, C, c};

use crate::helper::cycle::{Config, Helper};
use crate::helper::fetcher::fake::FakeFetcher;
use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, NotWired, ReleaseLocator};
use crate::helper::leases::fake::FakeLeases;
use crate::helper::memory::{Event, Memory};
use crate::helper::order::{Order, Verb as OrderVerb};
use crate::helper::restart::Restart;
use crate::spool::Entry;

const UNIT: &str = "kbf.service";
const T0: u64 = 1_800_000_000;
const UPDATE: &str = "var/lib/kbf-installer";

struct Rig {
    helper: Helper<ScratchHost>,
    root: PathBuf,
    layout: Layout,
    fetcher: FakeFetcher,
    leases: FakeLeases,
    /// The restarts the start-up made (the links were missing and were made).
    base: usize,
    platform: Platform,
}

/// The record `kbf-update` would write for `verb` on `commit`.
fn record(
    id: u64,
    epoch: u64,
    generation: u64,
    verb: Verb,
    commit: &str,
    platform: Platform,
) -> OrderRecord {
    let activate = verb == Verb::Activate;
    OrderRecord {
        order_id: id,
        epoch,
        generation,
        verb,
        commit: commit.to_owned(),
        window_s: 600,
        components: if activate {
            vec![Locator {
                name: match platform {
                    Platform::Linux => "kbf",
                    Platform::MacOs => "kbf-daemon",
                }
                .to_owned(),
                sha256: "ab".repeat(32),
                size: 1000,
                attestation: "{\"bundle\":1}".to_owned(),
            }]
        } else {
            Vec::new()
        },
        expected: activate.then(|| Facts {
            platform: match platform {
                Platform::Linux => "linux-x86_64".to_owned(),
                Platform::MacOs => "darwin-arm64".to_owned(),
            },
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
            profile: "release".to_owned(),
        }),
    }
}

impl Rig {
    /// A host running release A, with release B not installed, an empty spool, no lease and
    /// the service restart recorded.
    fn new(name: &str) -> Rig {
        Rig::on(name, Platform::Linux)
    }

    /// The same host on `platform`: macOS keeps its state and lock elsewhere.
    fn on(name: &str, platform: Platform) -> Rig {
        let mut host = ScratchHost::new(&format!("helper-{name}"));
        let root = host.root.clone();
        for sub in [
            "run",
            "var/run",
            "var/db/kbf-installer",
            "var/lib/kbf-installer",
            &format!("{UPDATE}/orders"),
            "var/lib/kbf-installer/releases",
        ] {
            fs::create_dir_all(root.join(sub)).expect("seed");
        }
        let dir = root.join("var/lib/kbf-installer/releases").join(A);
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"binary").expect("binary");
        host.answer = Box::new(|_| Ok(reply(0, "", "")));
        let layout = Layout::new(Root::new(&root).expect("absolute"), platform);
        Locked::acquire(&layout)
            .expect("lock")
            .write(&State {
                current: Some(c(A)),
                ..State::default()
            })
            .expect("state");
        let cfg = Config {
            update_dir: kbf_host::HostPath::new("/var/lib/kbf-installer").expect("path"),
            orders_dir: kbf_host::HostPath::new("/var/lib/kbf-installer/orders").expect("path"),
            restart: Restart::Systemd(UNIT.to_owned()),
            window_secs: 600,
            starts: 3,
        };
        let fetcher = FakeFetcher::new(&layout);
        let leases = FakeLeases::default();
        let mut helper = Helper::new(
            layout.clone(),
            host,
            cfg,
            Box::new(fetcher.clone()),
            Box::new(leases.clone()),
        );
        helper.recover().expect("recover");
        let mut rig = Rig {
            helper,
            root,
            layout,
            fetcher,
            leases,
            base: 0,
            platform,
        };
        rig.cycle(T0 - 100);
        rig.base = rig.restarts();
        rig
    }

    fn orders(&self) -> PathBuf {
        self.root.join(UPDATE).join("orders")
    }

    fn rec(&self, id: u64, epoch: u64, generation: u64, verb: Verb, commit: &str) -> OrderRecord {
        record(id, epoch, generation, verb, commit, self.platform)
    }

    /// Puts the record in the spool under its own name.
    fn put(&self, r: &OrderRecord) {
        let bytes = serde_json::to_vec(r).expect("json");
        fs::write(self.orders().join(r.file_name()), bytes).expect("record");
    }

    /// An order in epoch 1, generation 1.
    fn order(&self, id: u64, verb: Verb, commit: &str) {
        self.put(&self.rec(id, 1, 1, verb, commit));
    }

    fn put_raw(&self, epoch: u64, id: u64, body: &[u8]) {
        fs::write(self.orders().join(file_name(epoch, id)), body).expect("record");
    }

    fn left(&self) -> usize {
        fs::read_dir(self.orders()).expect("dir").count()
    }

    /// Release `commit` as a fetch would leave it.
    fn install(&self, commit: &str) {
        let dir = self
            .root
            .join("var/lib/kbf-installer/releases")
            .join(commit);
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"binary").expect("binary");
    }

    fn status(&self) -> serde_json::Value {
        let bytes = fs::read(self.root.join(UPDATE).join("status.json")).expect("status");
        serde_json::from_slice(&bytes).expect("json")
    }

    fn refusal(&self) -> String {
        self.status()["refusal"].as_str().expect("text").to_owned()
    }

    fn state(&self) -> State {
        crate::state::read_unlocked(&self.layout)
            .expect("reads")
            .expect("there")
    }

    fn restarts(&self) -> usize {
        self.helper
            .exec()
            .log
            .iter()
            .filter(|s| matches!(s, Step::Run(cmd) if cmd.args == ["restart", "--", UNIT]))
            .count()
            - self.base
    }

    fn cycle(&mut self, now: u64) {
        self.helper.cycle(now).expect("a pass");
    }

    fn current(&self) -> Option<crate::CommitId> {
        self.state().current
    }
}

fn link(rig: &Rig, name: &str) -> PathBuf {
    fs::read_link(rig.root.join("var/lib/kbf-installer/releases").join(name)).expect("link")
}

/// Catches the swap being skipped, the restart being skipped or repeated, a release that
/// is not installed not being fetched (once), and an order acted on twice: the activation
/// of a commit that is not on the host has the fetcher install it with the order's locators,
/// moves the links, restarts the unit once and reports a swap on trial; the same record put
/// back (a replay) fetches nothing and restarts nothing.
///
/// Mutants planted: no restart step; the acknowledged id not recorded (the replay restarts
/// again); the record not removed; the fetch skipped (the swap is refused as not installed).
#[test]
fn an_activate_order_fetches_swaps_restarts_once_and_is_not_replayed() {
    let mut rig = Rig::new("activate");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(link(&rig, "current"), PathBuf::from(B));
    assert_eq!(link(&rig, "previous"), PathBuf::from(A));
    assert_eq!(rig.restarts(), 1);
    assert_eq!(rig.fetcher.count(), 1);
    let call = rig.fetcher.calls.lock().expect("lock")[0].clone();
    assert_eq!(call.commit, c(B));
    assert_eq!(call.components[0].size, 1000);
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert_eq!(rig.status()["installed_commit"], B);
    assert_eq!(rig.status()["acked_order_id"], 5);
    assert_eq!(
        rig.status()["staged"]["protocol"],
        7,
        "the fetched release's facts"
    );
    assert_eq!(rig.left(), 0, "taken");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.restarts(), 1, "a replayed order restarts nothing");
    assert_eq!(rig.fetcher.count(), 1, "and fetches nothing");
    assert_eq!(rig.left(), 0, "removed unread");
}

/// Catches a release directory on the host being taken as proof: a rollback to a release
/// that is already installed is still handed to the fetcher, which verifies the directory
/// (or says it is unverified) instead of the cycle trusting that it is there.
///
/// Mutant planted: the fetch skipped when the directory exists (the fetcher is not called).
#[test]
fn an_installed_release_is_still_handed_to_the_fetcher() {
    let mut rig = Rig::new("installed");
    rig.install(B);
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.fetcher.count(), 1);
    assert_eq!(rig.current(), Some(c(B)));
    // A fetcher that finds the directory unverified refuses, and nothing moves.
    let mut other = Rig::new("installed-unverified");
    other.install(B);
    *other.fetcher.fail.lock().expect("lock") = Some(FetchError::Rejected(
        "unverified: rollback refused".to_owned(),
    ));
    other.order(5, Verb::Activate, B);
    other.cycle(T0);
    assert_eq!(other.current(), Some(c(A)));
    assert!(
        other.refusal().contains("unverified"),
        "{}",
        other.refusal()
    );
}

/// Catches a failed fetch changing the host: the source is unreachable, so `current` is
/// where it was, no link moved, nothing restarted, no swap is pending; the node is told
/// REFUSED with the fetch's reason, the order is spent (a person re-issues it, with a new
/// id) and the next order still runs.
///
/// Mutants planted: the restart owed before the fetch (a restart with nothing swapped);
/// the error swallowed (the swap goes ahead with no release).
#[test]
fn a_failed_fetch_leaves_current_alone_and_reports_refused() {
    let mut rig = Rig::new("fetch-fails");
    *rig.fetcher.fail.lock().expect("lock") =
        Some(FetchError::Unavailable("source unreachable".to_owned()));
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(A)));
    assert_eq!(link(&rig, "current"), PathBuf::from(A));
    assert_eq!(rig.restarts(), 0);
    assert_eq!(rig.state().pending_confirm, None);
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.refusal().contains("source unreachable"),
        "{}",
        rig.refusal()
    );
    assert_eq!(rig.status()["acked_order_id"], 5);
    assert_eq!(rig.left(), 0);
    // A fresh order (the person re-issues it) fetches again and goes through.
    rig.order(6, Verb::Activate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.current(), Some(c(B)));
    assert_eq!(rig.fetcher.count(), 2);
}

/// Catches a fetched release that is not the one ordered being swapped in: a fetcher whose
/// installed binary declares another protocol, another profile or another commit than the
/// order expected is refused before any link moves.
///
/// Mutants planted: the comparison dropped; only the protocol compared.
#[test]
fn a_fetched_release_that_declares_other_facts_is_refused_before_the_flip() {
    let cases: [(&str, crate::helper::fetcher::fake::Tamper); 4] = [
        ("protocol", |r| r.protocol = 8),
        ("profile", |r| r.profile = "debug".to_owned()),
        ("state schema", |r| r.state_schema = 9),
        ("commit", |r| r.commit = c(C)),
    ];
    for (what, change) in cases {
        let mut rig = Rig::new(&format!("facts-{}", what.replace(' ', "-")));
        *rig.fetcher.tamper.lock().expect("lock") = Some(change);
        rig.order(5, Verb::Activate, B);
        rig.cycle(T0);
        assert_eq!(rig.current(), Some(c(A)), "{what}");
        assert_eq!(rig.restarts(), 0, "{what}");
        assert_eq!(rig.status()["state"], "refused", "{what}");
        assert!(
            rig.refusal().contains("not the one ordered"),
            "{what}: {}",
            rig.refusal()
        );
    }
}

/// Catches the checks before the fetch being skipped: an activation while a trial is
/// pending, while a lease is live (read from the installer's own source), while the lease
/// source cannot answer is not even fetched. The one pending trial is another order's, so a
/// second timer is never started.
///
/// Mutants planted: the pending check dropped (the second activation reaches the installer's
/// own refusal after a wasted fetch); the live count compared with `> 1`.
#[test]
fn an_activation_is_refused_before_the_fetch_during_a_trial_or_with_leases_live() {
    let mut rig = Rig::new("pending");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.fetcher.count(), 1);
    // B is on trial, unconfirmed. Another activation, of a third release.
    rig.put(&rig.rec(6, 1, 2, Verb::Activate, C));
    rig.cycle(T0 + 3);
    assert_eq!(rig.fetcher.count(), 1, "nothing fetched");
    assert_eq!(rig.current(), Some(c(B)));
    assert!(rig.refusal().contains("on trial"), "{}", rig.refusal());
    assert_eq!(rig.restarts(), 1);

    let mut rig = Rig::new("leases");
    rig.leases.set(1);
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.fetcher.count(), 0);
    assert_eq!(rig.current(), Some(c(A)));
    assert!(
        rig.refusal().contains("1 leases are live"),
        "{}",
        rig.refusal()
    );
    // A confirm or revert takes no lease check.
    rig.leases.set(4);
    rig.order(6, Verb::Revert, A);
    rig.cycle(T0 + 3);
    assert!(!rig.refusal().contains("leases"), "{}", rig.refusal());
}

/// Catches an unreadable lease source being read as "none": the installer refuses, and
/// fetches nothing.
///
/// Mutant planted: the error arm treated as zero leases (the activation goes through).
#[test]
fn a_lease_source_that_cannot_answer_refuses_the_activation() {
    let mut rig = Rig::new("leases-unknown");
    rig.leases.set(usize::MAX);
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.fetcher.count(), 0);
    assert_eq!(rig.current(), Some(c(A)));
    assert!(rig.refusal().contains("cannot tell"), "{}", rig.refusal());
}

/// Catches a confirm that does not keep the swap, one that names the wrong swap, and a
/// revert that does not go back and restart: the confirm record carries the commit, the
/// helper finds the activation's own id.
///
/// Mutant planted: the confirm passing the confirm order's id instead of the pending swap's.
#[test]
fn a_confirm_order_keeps_the_swap_and_a_revert_order_goes_back() {
    let mut rig = Rig::new("confirm");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    rig.order(6, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.state().pending_confirm, None);
    assert_eq!(rig.status()["state"], "confirmed");
    assert_eq!(rig.restarts(), 1);

    rig.order(7, Verb::Revert, A);
    rig.cycle(T0 + 6);
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(rig.restarts(), 2, "a revert restarts the unit");
    assert_eq!(rig.status()["state"], "reverted");
}

/// Catches a swap left on trial forever: the Linux boot check starts the window at the
/// first start; the pass after the window reverts and restarts the unit.
///
/// Mutant planted: `expire` not called in the pass.
#[test]
fn a_swap_not_confirmed_in_its_window_is_reverted() {
    let mut rig = Rig::new("expire");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 3);
    // The first start of B (the unit's ExecStartPre) starts the window.
    let layout = rig.layout.clone();
    let mut out = Vec::new();
    let decision = activate::boot_check(rig.helper.exec_mut(), &layout, T0 + 10, 600, &mut out)
        .expect("checks");
    assert!(matches!(decision, activate::Boot::Start { .. }));
    rig.cycle(T0 + 500);
    assert_eq!(rig.state().current, Some(c(B)), "inside the window");
    rig.cycle(T0 + 10 + 601);
    assert_eq!(rig.state().current, Some(c(A)));
    assert_eq!(rig.status()["state"], "reverted");
    assert_eq!(rig.restarts(), 2);
}

/// Catches a restart lost on the way: when the restart fails, the owed restart stays in
/// `helper.json` and the next pass retries, and the node is told why in the meantime.
///
/// Mutant planted: `restart_due` cleared before the restart is tried.
#[test]
fn a_failed_restart_is_owed_until_it_succeeds() {
    let mut rig = Rig::new("restart-owed");
    let tries = std::rc::Rc::new(std::cell::Cell::new(0));
    let seen = tries.clone();
    rig.helper.exec_mut().answer = Box::new(move |_| {
        seen.set(seen.get() + 1);
        if seen.get() == 1 {
            Ok(reply(1, "", "unit not found"))
        } else {
            Ok(reply(0, "", ""))
        }
    });
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert!(rig.refusal().contains("restart failed"));
    rig.cycle(T0 + 6);
    assert_eq!(tries.get(), 2, "retried on the next pass, once");
    rig.cycle(T0 + 9);
    assert_eq!(tries.get(), 2, "and not again");
}

/// Catches a floor moved by garbage, and a record trusted for what it is not. Each of these
/// is refused, removed and acknowledges nothing, so the real order after it (a lower id)
/// still runs: text that is not JSON; a record with a field the writer never makes; a file
/// named for another order than it holds; a verb that is none of three; a commit that is not
/// a commit; an activation with no release claims; one over the size cap; a directory. A link
/// in the spool is refused as a link and removed as a link; what it points at is untouched.
///
/// Mutants planted: acknowledging an unparsable file's id (the real order is then ignored as
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
/// Mutant planted: `O_NOFOLLOW` dropped from the open (the secret is read; the reason is a
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
/// Mutants planted: the cap raised to 100 (the excess is acted on); the excess left on disk.
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
/// Mutants planted: expiry by the file's mtime (the ancient-mtime record is dropped and the
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
/// Mutant planted: the step returned as `None`.
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
/// Mutants planted: the clamp dropped; the bounds swapped.
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
/// Mutant planted: the sort dropped (with the files made in reverse id order).
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
/// Mutants planted: a jump cap on the id (the gap is refused); the epoch ignored (the stale
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
/// Mutant planted: either mode swapped.
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
/// Mutant planted: the filter dropped.
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
/// Mutants planted: the check dropped; the first-order exemption dropped.
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
/// Mutant planted: `order_id: order.id.to_string()` (the second passes for the first's repeat).
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

/// Catches a frontend update crashing the reader or moving the floor: the record parses (it
/// carries claims like an activation), is refused in status.json with "not implemented
/// (P13)", changes nothing, is removed, and does not advance the acknowledged id or epoch, so
/// a real order of a lower id in the same epoch still runs.
///
/// Mutant planted: the early arm in `take` removed (the order falls through to `act`, which
/// refuses it but acknowledges it, and the lower real order is then dropped as a replay).
#[test]
fn a_frontend_update_record_is_refused_not_implemented_and_moves_no_floor() {
    let mut rig = Rig::new("frontend-update");
    let mut r = rig.rec(9, 1, 1, Verb::Activate, B);
    r.verb = Verb::FrontendUpdate;
    rig.put(&r);
    rig.cycle(T0);
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.refusal().contains("not implemented (P13)"),
        "{}",
        rig.refusal()
    );
    assert_eq!(rig.status()["acked_order_id"], 0, "the floor did not move");
    assert_eq!(rig.current(), Some(c(A)));
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

/// Catches the two kinds of order sharing a floor. The node floor is at (1, 7); a frontend
/// record of the same epoch and a lower id is not a replay of it (it reaches its own
/// refusal), and with the frontend floor seeded at (1, 7) a node order of id 3 is not
/// dropped as a replay of the frontend's: each kind is ordered against its own pair.
///
/// Mutants planted: the floor taken from the node pair whatever the kind (the frontend
/// record is dropped silently, and the node order after the seeded frontend floor too).
#[test]
fn node_and_frontend_orders_have_separate_floors() {
    let mut rig = Rig::new("floors-per-kind");
    rig.order(7, Verb::Confirm, A);
    rig.cycle(T0);
    assert_eq!(rig.status()["acked_order_id"], 7);
    let mut r = rig.rec(3, 1, 1, Verb::Activate, B);
    r.verb = Verb::FrontendUpdate;
    rig.put(&r);
    rig.cycle(T0 + 3);
    assert!(
        rig.refusal().contains("not implemented (P13)"),
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
/// Mutants planted: the listing error ignored (no refusal is recorded); the `crowded` flag
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
    }
}

/// Catches the two defences that the parse already makes being relied on alone: an activation
/// with no release claims is refused by the vetting itself (fail closed), and a
/// `frontend_update` that reached the act step is refused, not carried out as a node verb.
///
/// Mutants planted: the claims check removed (the vetting panics or passes); the frontend arm
/// of `act` made a revert (the links move).
#[test]
fn vetting_and_acting_refuse_what_parsing_already_refuses() {
    let mut rig = Rig::new("defence");
    let mut memory = Memory::default();
    let before = rig.state();
    let why = rig
        .helper
        .prepare(&plain_order(OrderVerb::Activate), &before, &mut memory)
        .unwrap_err();
    assert!(why.contains("no release claims"), "{why}");
    rig.helper
        .act(&plain_order(OrderVerb::FrontendUpdate), T0, &mut memory)
        .expect("refused, not failed");
    assert_eq!(memory.event, Event::Refused);
    assert!(
        memory
            .refusal
            .contains("frontend_update is not implemented"),
        "{}",
        memory.refusal
    );
    assert_eq!(rig.current(), Some(c(A)));
}

/// Catches an activation naming a component set the platform does not take: a Linux host takes
/// exactly `kbf`; a second component is refused before the fetch, with both names in the
/// reason.
///
/// Mutant planted: the set compared as a prefix (an extra component passes).
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
struct Hollow;

impl Fetcher for Hollow {
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        Ok(crate::helper::fetcher::fake::honest(rec))
    }
}

/// Catches an activation that fails at the swap (the release is not on the host after all)
/// being reported as done: the order is refused with the installer's reason, the links stay
/// where they were, and no restart is owed. Also the fetcher of a build that has none refuses
/// every fetch.
///
/// Mutants planted: the swap error turned into `Ok(Done::quiet(..))` (the order is
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
    };
    assert!(matches!(NotWired.fetch(&loc), Err(FetchError::NotWired)));
}

/// Catches an activation that goes on to fetch while the node cannot be told a fetch is under
/// way (status.json cannot be written): the order is refused before the fetch, nothing moves,
/// and the pass reports the failure to write instead of ending as if it had reported.
///
/// Mutant planted: the report error ignored in the vetting (the fetch happens anyway).
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
/// Mutant planted: the pending trial refused whatever its activation (the repeat is refused).
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
