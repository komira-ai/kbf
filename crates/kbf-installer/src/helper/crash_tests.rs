//! The helper killed at every step of a pass, then restarted: the links, the state and the
//! restarts must agree afterwards, for an activate, a revert and an expiry. (Adopted from
//! the reviewer's probe of #929: it was red at revert steps 4 and 5 and expiry steps 4 and 5
//! before the restart was owed ahead of the verb.)
use std::cell::{Cell, RefCell};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;

use crate::activate;
use crate::layout::Layout;
use crate::state::{Locked, State};
use crate::testkit::{A, B, c};
use kbf_host::exec::Exec;
use kbf_host::testkit::{ScratchHost, reply};
use kbf_host::{Cmd, HostPath, Root, Step};

use crate::helper::cycle::{Config, Helper};
use crate::helper::fetcher::fake::FakeFetcher;
use crate::helper::leases::fake::FakeLeases;
use crate::helper::restart::Restart;
use crate::helper::service::fake::ServiceRecorder;
use kbf_update::{Facts, Locator, OrderRecord, Verb};

const UNIT: &str = "kbf.service";
const T0: u64 = 1_800_000_000;

struct Crashy {
    inner: Rc<RefCell<ScratchHost>>,
    counter: Rc<Cell<usize>>,
    crash_at: Rc<Cell<usize>>, // 0 = never; odd = before step, even = after
}

impl Exec for Crashy {
    fn euid(&self) -> u32 {
        self.inner.borrow().euid()
    }
    fn inspect(&self, p: &HostPath) -> std::io::Result<Option<kbf_host::Entry>> {
        self.inner.borrow().inspect(p)
    }
    fn stat(&self, p: &HostPath) -> std::io::Result<Option<kbf_host::Meta>> {
        self.inner.borrow().stat(p)
    }
    fn query(&self, c: &Cmd) -> std::io::Result<std::process::Output> {
        self.inner.borrow().query(c)
    }
    fn http_status(&self, u: &kbf_host::HttpUrl) -> std::io::Result<u16> {
        self.inner.borrow().http_status(u)
    }
    fn apply(&mut self, s: &Step) -> std::io::Result<()> {
        let n = self.counter.get() + 1;
        self.counter.set(n);
        let at = self.crash_at.get();
        if at != 0 && n == at.div_ceil(2) && !at.is_multiple_of(2) {
            panic!("killed before step {n}");
        }
        let r = self.inner.borrow_mut().apply(s);
        if at != 0 && n == at / 2 && at.is_multiple_of(2) {
            panic!("killed after step {n}");
        }
        r
    }
    fn check_binary(&self) -> Result<(), kbf_host::trust::Untrusted> {
        Ok(())
    }
}

struct World {
    host: Rc<RefCell<ScratchHost>>,
    root: PathBuf,
    layout: Layout,
    counter: Rc<Cell<usize>>,
    crash_at: Rc<Cell<usize>>,
    fetcher: FakeFetcher,
}

impl World {
    fn new(name: &str) -> World {
        let mut host = ScratchHost::new(&format!("probe-{name}"));
        let root = host.root.clone();
        for sub in [
            "run",
            "var/lib/kbf-installer",
            "var/lib/kbf-installer/orders",
            "var/lib/kbf-installer/releases",
        ] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        // A is running; B is on the host already except where a scenario has it fetched.
        for r in [A, B] {
            let d = root.join("var/lib/kbf-installer/releases").join(r);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("kbf"), b"binary").unwrap();
        }
        host.answer = Box::new(|_| Ok(reply(0, "", "")));
        let layout = Layout::linux(Root::new(&root).unwrap());
        Locked::acquire(&layout)
            .unwrap()
            .write(&State {
                current: Some(c(A)),
                staged: Some(c(B)),
                ..State::default()
            })
            .unwrap();
        World {
            fetcher: FakeFetcher::new(&layout),
            host: Rc::new(RefCell::new(host)),
            root,
            layout,
            counter: Rc::new(Cell::new(0)),
            crash_at: Rc::new(Cell::new(0)),
        }
    }
    fn helper(&self) -> Helper<Crashy> {
        let cfg = Config {
            update_dir: HostPath::new("/var/lib/kbf-installer").unwrap(),
            orders_dir: HostPath::new("/var/lib/kbf-installer/orders").unwrap(),
            restart: Restart::Systemd(UNIT.to_owned()),
            window_secs: 600,
            starts: 3,
            min_activation_interval_secs: 0,
        };
        Helper::new(
            self.layout.clone(),
            Crashy {
                inner: self.host.clone(),
                counter: self.counter.clone(),
                crash_at: self.crash_at.clone(),
            },
            cfg,
            Box::new(self.fetcher.clone()),
            Box::new(FakeLeases::default()),
            Box::new(ServiceRecorder::default()),
        )
    }
    fn order(&self, id: u64, verb: &str, commit: &str) {
        self.order_in(id, 1, verb, commit);
    }
    fn order_in(&self, id: u64, epoch: u64, verb: &str, commit: &str) {
        let verb = match verb {
            "activate" => Verb::Activate,
            "confirm" => Verb::Confirm,
            "frontend_update" => Verb::FrontendUpdate,
            _ => Verb::Revert,
        };
        let activate = verb == Verb::Activate;
        let record = OrderRecord {
            order_id: id,
            epoch,
            generation: 1,
            verb,
            commit: commit.to_owned(),
            window_s: 600,
            components: if activate {
                vec![Locator {
                    name: "kbf".to_owned(),
                    sha256: "ab".repeat(32),
                    size: 1000,
                    attestation: "{}".to_owned(),
                }]
            } else {
                Vec::new()
            },
            expected: activate.then(|| Facts {
                platform: "linux-x86_64".to_owned(),
                protocol: 7,
                min_protocol: 6,
                state_schema: 3,
                profile: "release".to_owned(),
            }),
        };
        fs::write(
            self.root
                .join("var/lib/kbf-installer/orders")
                .join(record.file_name()),
            record.to_bytes().unwrap(),
        )
        .unwrap();
    }
    fn restarts(&self) -> usize {
        self.host
            .borrow()
            .log
            .iter()
            .filter(|s| matches!(s, Step::Run(cmd) if cmd.args == ["restart", "--", UNIT]))
            .count()
    }
    fn link(&self) -> String {
        fs::read_link(self.root.join("var/lib/kbf-installer/releases/current"))
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }
    fn state(&self) -> State {
        crate::state::read_unlocked(&self.layout).unwrap().unwrap()
    }
    fn orders_left(&self) -> usize {
        fs::read_dir(self.root.join("var/lib/kbf-installer/orders"))
            .unwrap()
            .count()
    }
}

fn pass(w: &World, now: u64) {
    let mut h = w.helper();
    h.recover().unwrap();
    h.cycle(now).unwrap();
}

fn crash_pass(w: &World, now: u64, at: usize) -> bool {
    w.counter.set(0);
    w.crash_at.set(at);
    let r = catch_unwind(AssertUnwindSafe(|| {
        let mut h = w.helper();
        h.cycle(now).unwrap();
    }));
    w.crash_at.set(0);
    r.is_err()
}

fn steps_in_clean_pass(scn: &str) -> usize {
    let w = prep(scn, "count");
    w.counter.set(0);
    pass(&w, T0 + 700);
    w.counter.get()
}

fn prep(scn: &str, tag: &str) -> World {
    let w = World::new(&format!("{scn}-{tag}"));
    if scn.starts_with("frontend") {
        let dir = w.root.join("etc/kbf");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("kbf.toml"), "mode = \"frontend\"\n").unwrap();
    }
    pass(&w, T0 - 100);
    match scn {
        // The proxy's three orders; the host starts with B staged except for the update,
        // which fetches it (B is not on the host then).
        "frontend_update" => {
            fs::remove_dir_all(w.root.join("var/lib/kbf-installer/releases").join(B)).unwrap();
            Locked::acquire(&w.layout)
                .unwrap()
                .write(&State {
                    current: Some(c(A)),
                    ..State::default()
                })
                .unwrap();
            w.order(5, "frontend_update", B);
        }
        "frontend_confirm" => w.order(5, "confirm", B),
        "frontend_revert" => w.order(5, "revert", B),
        "activate" => {
            w.order(5, "activate", B);
        }
        // B is not on the host: the fetcher installs it, and the pass can die on either
        // side of that.
        "activate_fetching" => {
            fs::remove_dir_all(w.root.join("var/lib/kbf-installer/releases").join(B)).unwrap();
            w.order(5, "activate", B);
        }
        "revert" => {
            w.order(5, "activate", B);
            pass(&w, T0);
            w.order(6, "confirm", B);
            pass(&w, T0 + 3);
            w.order(7, "revert", A);
        }
        // A revert from a restored store: epoch 2 counts its ids again, so id 1 is newer
        // than id 6 of epoch 1.
        "revert_in_a_new_epoch" => {
            w.order(5, "activate", B);
            pass(&w, T0);
            w.order(6, "confirm", B);
            pass(&w, T0 + 3);
            w.order_in(1, 2, "revert", A);
        }
        "expire" => {
            w.order(5, "activate", B);
            pass(&w, T0);
            let mut out = Vec::new();
            let mut ex = Crashy {
                inner: w.host.clone(),
                counter: w.counter.clone(),
                crash_at: w.crash_at.clone(),
            };
            activate::boot_check(&mut ex, &w.layout, T0 + 10, 600, &mut out).unwrap();
        }
        _ => unreachable!(),
    }
    w
}

/// Catches a restart lost by a kill between the links moving and the pass's next save.
fn kill_at_every_step_of(scn: &str) {
    let mut bad = Vec::new();
    {
        let n = steps_in_clean_pass(scn);
        eprintln!("{scn}: {n} steps in a clean pass");
        for at in 1..=(2 * n) {
            let w = prep(scn, &format!("k{at}"));
            let before = w.restarts();
            let died = crash_pass(&w, T0 + 700, at);
            for i in 0..3 {
                pass(&w, T0 + 800 + i * 3);
            }
            let st = w.state();
            let link = w.link();
            let cur = st
                .current
                .as_ref()
                .map(|x| x.as_str().to_owned())
                .unwrap_or_default();
            let restarts = w.restarts() - before;
            let mut why = Vec::new();
            if !link.ends_with(&cur) {
                why.push(format!("link {link} != state {cur}"));
            }
            let activating = scn.starts_with("activate");
            let frontend = scn.starts_with("frontend");
            let moved = !frontend && ((activating && cur == B) || (!activating && cur == A));
            if frontend {
                // No unit is restarted for the proxy's orders; the state and the status end
                // where the order puts them, and `current` is B only after a confirm.
                let (want_cur, want_staged) = match scn {
                    "frontend_update" => (A, Some(c(B))),
                    "frontend_confirm" => (B, None),
                    _ => (A, None),
                };
                if restarts != 0 {
                    why.push(format!("{restarts} restarts"));
                }
                if cur != want_cur || st.staged != want_staged {
                    why.push(format!("current {cur}, staged {:?}", st.staged));
                }
                let status = fs::read(w.root.join("var/lib/kbf-installer/status.json")).unwrap();
                let status: serde_json::Value = serde_json::from_slice(&status).unwrap();
                if status["installed_commit"] != cur.as_str() {
                    why.push(format!(
                        "status says installed {}",
                        status["installed_commit"]
                    ));
                }
            }
            if moved && restarts == 0 {
                why.push("links moved, service never restarted".to_owned());
            }
            if restarts > 2 {
                why.push(format!("{restarts} restarts"));
            }
            if w.orders_left() != 0 {
                why.push("order file left".to_owned());
            }
            if scn == "activate_fetching"
                && w.fetcher.count() != 1
                && !(w.fetcher.count() == 2 && died)
            {
                why.push(format!("{} fetches", w.fetcher.count()));
            }
            if !why.is_empty() {
                bad.push(format!("{scn} step {at} (died {died}): {}", why.join("; ")));
            }
        }
    }
    for b in &bad {
        eprintln!("BAD {b}");
    }
    assert!(bad.is_empty(), "{} bad", bad.len());
}

#[test]
fn kill_at_every_step_of_an_activate() {
    kill_at_every_step_of("activate");
}

/// Catches a pass that dies around a fetch leaving the host wrong: a kill at every step of
/// an activation of a release that is not on the host. The fetch is not a step (it writes the
/// release directory), so the kills fall before it, between it and the swap, and after; in
/// every case the links and the state agree after the next passes, the service was
/// restarted if the links moved, and the release is fetched once, or twice only when the
/// kill came before the fetch had finished.
///
/// Mutants planted: the restart owed before the fetch (a kill in between restarts the
/// service with nothing swapped); the order file removed before the swap.
#[test]
fn kill_at_every_step_of_an_activate_that_fetches() {
    kill_at_every_step_of("activate_fetching");
}

#[test]
fn kill_at_every_step_of_a_revert() {
    kill_at_every_step_of("revert");
}

#[test]
fn kill_at_every_step_of_a_revert_in_a_new_epoch() {
    kill_at_every_step_of("revert_in_a_new_epoch");
}

/// Catches a pass of a frontend host that dies around the proxy's orders leaving the host
/// wrong: killed at every step of a `frontend_update` that fetches, a `confirm` that flips
/// `previous` and `current`, and a `revert` that drops the staging, then started again (the
/// start-up repair, then passes), the links agree with the state, the state is where the order
/// puts it, `status.json` names the same installed release, no unit was restarted (not by the
/// repair either), and the order file is gone.
///
/// Mutants planted: the repair owing a restart on a frontend host; the status not rewritten
/// after a restart; the links moved before the state (the repair then restores the old ones).
#[test]
fn kill_at_every_step_of_a_frontend_update() {
    kill_at_every_step_of("frontend_update");
}

#[test]
fn kill_at_every_step_of_a_frontend_confirm() {
    kill_at_every_step_of("frontend_confirm");
}

#[test]
fn kill_at_every_step_of_a_frontend_revert() {
    kill_at_every_step_of("frontend_revert");
}

#[test]
fn kill_at_every_step_of_an_expiry() {
    kill_at_every_step_of("expire");
}

/// Catches orders read through a linked spool: the service user owns that directory's
/// parent chain below the root tree and could point `orders` at one of its own anywhere.
/// Mutant seen red: `orders` opened by a path that follows the link: the helper acted on the
/// order and removed the file.
#[test]
fn a_spool_replaced_by_a_link_is_not_followed_for_orders() {
    let w = World::new("spoollink");
    pass(&w, T0 - 100);
    let base = w.root.join("var/lib/kbf-installer");
    let elsewhere = w.root.join("var/lib/elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::remove_dir_all(base.join("orders")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, base.join("orders")).unwrap();
    w.order_in(5, 1, "activate", B);
    // `order_in` wrote through the link, into `elsewhere`.
    let f = elsewhere.join(kbf_update::record::file_name(1, 5));
    assert!(f.exists());
    pass(&w, T0);
    assert!(
        w.link().ends_with(A),
        "helper acted on an order through a linked spool"
    );
    assert!(f.exists(), "helper unlinked a file through a linked spool");
}
