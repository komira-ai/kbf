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
use kbf_update::{Facts, Locator, OrderRecord, Source, Verb};

use crate::activate;
use crate::layout::{Layout, Platform};
use crate::state::{Locked, State};
use crate::testkit::{A, B, C, c};

use crate::helper::cycle::{Config, Helper};
use crate::helper::fetcher::fake::FakeFetcher;
use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, NotWired, ReleaseLocator};
use crate::helper::leases::fake::FakeLeases;
use crate::helper::mac_leases::tests::{MacHost, reply_pids};
use crate::helper::memory::{Event, Memory};
use crate::helper::order::{Order, Verb as OrderVerb};
use crate::helper::restart::Restart;
use crate::helper::service::fake::ServiceRecorder;
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
    /// The unit the helper restarts.
    unit: String,
    platform: Platform,
    /// The service manager and the process table.
    service: ServiceRecorder,
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
                name: "kbf".to_owned(),
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
        source: Source::Fetch,
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
        Rig::build(name, platform, 0, false)
    }

    /// A host that runs a frontend: it takes the proxy's three orders.
    fn frontend(name: &str) -> Rig {
        Rig::build(name, Platform::Linux, 0, true)
    }

    /// A frontend host that has no release yet, as `setup` finds it: no state, no links, no
    /// service running. The first release comes from `setup`, not from the rig.
    fn bare_frontend(name: &str) -> Rig {
        Rig::seeded(name, Platform::Linux, 0, true, false, UNIT)
    }

    /// The same host with `min_activation_interval_secs` set.
    fn build(name: &str, platform: Platform, min_interval: u64, frontend_host: bool) -> Rig {
        Rig::build_as(name, platform, min_interval, frontend_host, UNIT)
    }

    /// The same host on Linux, its helper restarting `unit`.
    fn restarting(name: &str, unit: &str) -> Rig {
        Rig::build_as(name, Platform::Linux, 0, false, unit)
    }

    fn build_as(
        name: &str,
        platform: Platform,
        min_interval: u64,
        frontend_host: bool,
        unit: &str,
    ) -> Rig {
        Rig::seeded(name, platform, min_interval, frontend_host, true, unit)
    }

    /// The host, running release A or (`running` false) with nothing installed, its helper
    /// restarting `unit`.
    fn seeded(
        name: &str,
        platform: Platform,
        min_interval: u64,
        frontend_host: bool,
        running: bool,
        unit: &str,
    ) -> Rig {
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
        let service = if running {
            let dir = root.join("var/lib/kbf-installer/releases").join(A);
            fs::create_dir_all(&dir).expect("release");
            fs::write(dir.join("kbf"), b"binary").expect("binary");
            ServiceRecorder::running(4242, dir.join("kbf"))
        } else {
            ServiceRecorder::default()
        };
        host.answer = service.answer();
        let layout = Layout::new(Root::new(&root).expect("absolute"), platform);
        if frontend_host {
            write_kbf_toml(&layout, "mode = \"frontend\"\n");
        }
        if running {
            Locked::acquire(&layout)
                .expect("lock")
                .write(&State {
                    current: Some(c(A)),
                    ..State::default()
                })
                .expect("state");
        }
        let cfg = Config {
            update_dir: kbf_host::HostPath::new("/var/lib/kbf-installer").expect("path"),
            orders_dir: kbf_host::HostPath::new("/var/lib/kbf-installer/orders").expect("path"),
            restart: Restart::Systemd(unit.to_owned()),
            window_secs: 600,
            starts: 3,
            min_activation_interval_secs: min_interval,
        };
        let fetcher = FakeFetcher::new(&layout);
        let leases = FakeLeases::default();
        let mut helper = Helper::new(
            layout.clone(),
            host,
            cfg,
            Box::new(fetcher.clone()),
            Box::new(leases.clone()),
            Box::new(service.clone()),
        );
        helper.recover().expect("recover");
        let mut rig = Rig {
            helper,
            root,
            layout,
            fetcher,
            leases,
            base: 0,
            unit: unit.to_owned(),
            platform,
            service,
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
            .filter(|s| matches!(s, Step::Run(cmd) if cmd.args == ["restart", "--", self.unit.as_str()]))
            .count()
            - self.base
    }

    /// The service starts `commit`: the Linux boot check runs (the window starts), and the
    /// unit's main process runs a file of that release.
    fn first_start(&mut self, commit: &str, now: u64) {
        let layout = self.layout.clone();
        let mut out = Vec::new();
        activate::boot_check(self.helper.exec_mut(), &layout, now, 600, &mut out)
            .expect("boot check");
        self.service.0.borrow_mut().exes = vec![(
            4242,
            self.root
                .join("var/lib/kbf-installer/releases")
                .join(commit)
                .join("kbf"),
        )];
    }

    fn cycle(&mut self, now: u64) {
        self.helper.cycle(now).expect("a pass");
    }

    fn current(&self) -> Option<crate::CommitId> {
        self.state().current
    }
}

/// Writes the host's `kbf.toml` (where its platform keeps it) with `text`.
fn write_kbf_toml(layout: &Layout, text: &str) {
    let path = layout.join(&layout.kbf_toml());
    fs::create_dir_all(path.parent().expect("a parent")).expect("config dir");
    fs::write(path, text).expect("kbf.toml");
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
/// Catches: no restart step; the acknowledged id not recorded (the replay restarts
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

/// Catches the order's expected facts not reaching the fetcher: an activation is fetched
/// through `fetch_activation` with exactly the facts the record carries, so the verify
/// pipeline can hold the binary's own report against them before anything is installed.
///
/// Catches: the cycle calling plain `fetch` (the fetcher is given no facts); one fact
/// of the five taken from the wrong field (`min_protocol` for `protocol`).
#[test]
fn an_activation_hands_the_fetcher_the_orders_facts() {
    let mut rig = Rig::new("facts");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    let given = rig.fetcher.expected.lock().expect("lock").clone();
    assert_eq!(
        given,
        vec![Facts {
            platform: "linux-x86_64".to_owned(),
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
            profile: "release".to_owned(),
        }]
    );
}

/// Catches a release directory on the host being taken as proof: a rollback to a release
/// that is already installed is still handed to the fetcher, which verifies the directory
/// (or says it is unverified) instead of the cycle trusting that it is there.
///
/// Catches: the fetch skipped when the directory exists (the fetcher is not called).
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
/// Catches: the restart owed before the fetch (a restart with nothing swapped);
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
/// Catches: the comparison dropped; only the protocol compared.
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
/// Catches: the pending check dropped (the second activation reaches the installer's
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

/// A macOS helper whose lease source is the real Mac source over a scripted host: the
/// source's own `setup` plays the host (processes, directories), the rig the helper's.
fn mac_rig(name: &str, setup: impl FnOnce(&mut MacHost)) -> Rig {
    let mut rig = Rig::on(name, Platform::MacOs);
    let mut host = MacHost::new(name);
    setup(&mut host);
    rig.helper.set_leases(Box::new(host.source()));
    rig
}

/// Catches the Mac source not wired into the cycle: with a keeper running on the Mac an
/// activation is refused with the live count, before any fetch, and the release stays; with
/// the host idle the same order is fetched and swapped; a Mac that cannot tell (no
/// `sandbox-exec`) refuses with the reason; and when only a stale directory remains with no
/// daemon running the activation goes through and the pass logs which directory it let by.
///
/// Catches: the keepers ignored; the `sandbox-exec` check dropped; the lease note not logged
/// by the cycle; the note naming no directory.
#[test]
fn a_mac_activation_follows_the_mac_lease_source() {
    let mut rig = mac_rig("mac-keeper", |h| h.keepers(reply_pids("412\n")));
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.fetcher.count(), 0);
    assert_eq!(rig.current(), Some(c(A)));
    assert!(
        rig.refusal().contains("1 leases are live"),
        "{}",
        rig.refusal()
    );

    let mut rig = mac_rig("mac-idle", |_| {});
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(B)));
    assert_eq!(rig.restarts(), 1);

    let mut rig = mac_rig("mac-blind", |h| h.without_sandbox());
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.fetcher.count(), 0);
    assert!(
        rig.refusal()
            .contains("cannot tell whether leases are live: ")
            && rig.refusal().contains("sandbox-exec"),
        "{}",
        rig.refusal()
    );

    let mut rig = mac_rig("mac-stale", |h| {
        h.no_daemon();
        h.dir("lease-3-7");
    });
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(B)));
    let mut log = Vec::new();
    rig.helper.drain_log(&mut log);
    let log = String::from_utf8(log).expect("text");
    assert!(
        log.contains(
            "leases: /Volumes/kbf/data/leases/lease-3-7 remains and no kbf daemon is running"
        ),
        "{log}"
    );
}

/// Catches an unreadable lease source being read as "none": the installer refuses, and
/// fetches nothing.
///
/// Catches: the error arm treated as zero leases (the activation goes through).
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

/// Catches a node `activate` taken when the unit the helper restarts is a frontend's. A
/// frontend holds no leases, so a running one reads as idle and the lease check lets the
/// order through; the swap would then restart the frontend, which moves by `frontend_update`
/// (the proxy owns its streams). The order is refused before any fetch, with no live lease
/// and no restart, for the template's instance and for the plain unit; the same order for the
/// daemon's unit, with the same idle lease source, is taken.
///
/// Catches: the check removed (both frontend units swap); the check refusing every
/// unit (the daemon's order is refused).
#[test]
fn an_activation_is_refused_when_the_restarted_unit_is_a_frontends() {
    for unit in ["kbf-frontend@a.service", "kbf-frontend.service"] {
        let mut rig = Rig::restarting("frontend-unit", unit);
        rig.order(5, Verb::Activate, B);
        rig.cycle(T0);
        let why = format!(
            "denied: order 5: {unit:?} is a frontend's unit; a frontend moves by frontend_update, \
             and an activate would restart it"
        );
        let seen = (
            rig.fetcher.count(),
            rig.current(),
            rig.restarts(),
            rig.refusal(),
        );
        assert_eq!(seen, (0, Some(c(A)), 0, why), "{unit}");
    }
    let mut rig = Rig::restarting("daemon-unit", "kbf-daemon.service");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    let seen = (rig.fetcher.count(), rig.current(), rig.restarts());
    assert_eq!(seen, (1, Some(c(B)), 1));
}

/// Catches a confirm that does not keep the swap, one that names the wrong swap, and a
/// revert that does not go back and restart: the confirm record carries the commit, the
/// helper finds the activation's own id.
///
/// Catches: the confirm passing the confirm order's id instead of the pending swap's.
#[test]
fn a_confirm_order_keeps_the_swap_and_a_revert_order_goes_back() {
    let mut rig = Rig::new("confirm-revert");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    rig.first_start(B, T0 + 1);
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
/// Catches: `expire` not called in the pass.
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
/// Catches: `restart_due` cleared before the restart is tried.
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

mod components_tests;
mod frontend_tests;
mod meta_log_tests;
mod notice_tests;
mod pipeline_fetcher_tests;
mod refused_id_tests;
mod release_pass_tests;
use release_pass_tests::Hollow;
mod scratch_sweep_tests;
mod seed_order_tests;
mod spool_order_tests;
mod staged_tests;
