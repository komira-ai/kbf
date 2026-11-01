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
    let mut rig = Rig::new("confirm");
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
struct Hollow;

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

mod components_tests;
mod frontend_tests;
mod meta_log_tests;
mod notice_tests;
mod pipeline_fetcher_tests;
mod refused_id_tests;
mod scratch_sweep_tests;
mod seed_order_tests;
mod staged_tests;
