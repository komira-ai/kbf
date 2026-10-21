//! The phases of the spike, on a real Mac as root. Each prints `MEASURE` lines as it goes
//! (so a run that dies still shows what it measured) and records the few numbers the
//! closing summary repeats. A phase that cannot run is a failure of the harness, kept for
//! the end: the clean-up always runs first.

use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kbf_driver_native::slots::macos::{Ready, Slots};
use kbf_driver_native::slots::{Pool, SlotUser};

use crate::block::{self, FIRST_UID, Mac as _};
use crate::mac::{DSCL, RealMac};
use crate::stats::{Samples, Summary, measure, ms};

/// Past this the spike starts no further phase: the clean-up and the report still fit in
/// the workflow's timeout.
const BUDGET: Duration = Duration::from_secs(35 * 60);
/// How long the visibility poll waits for the last user.
const VISIBLE_WITHIN: Duration = Duration::from_secs(120);
/// The stages the users are counted at.
pub const STAGES: [u32; 2] = [100, 1000];
/// Users reset in the reset phase, spread evenly over the pool.
pub const RESET_SAMPLE: u32 = 20;
/// The kill deadline of a reset, as the daemon's.
const WITHIN: Duration = Duration::from_secs(10);

pub struct Spike<'a> {
    mac: &'a RealMac,
    started: Instant,
    created: u32,
    /// The numbers the closing summary repeats.
    pub summary: Vec<(String, String)>,
    /// What stopped a phase.
    pub failures: Vec<String>,
}

impl<'a> Spike<'a> {
    pub fn new(mac: &'a RealMac) -> Self {
        Self {
            mac,
            started: Instant::now(),
            created: 0,
            summary: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn say(&self, what: &str, fields: &str) {
        println!("{}", measure(what, fields));
    }

    fn note(&mut self, key: &str, value: String) {
        self.summary.push((key.to_owned(), value));
    }

    fn over_budget(&self) -> bool {
        self.started.elapsed() > BUDGET
    }

    fn fail(&mut self, why: String) {
        println!("FAILED {why}");
        self.failures.push(why);
    }

    /// Every phase, in order. Never panics on a phase's failure.
    pub fn run(&mut self) {
        println!(
            "MEASURE harness first_uid={FIRST_UID} block={} stages={STAGES:?} reset_sample={RESET_SAMPLE} budget_s={}",
            block::BLOCK,
            BUDGET.as_secs()
        );
        if let Err(why) = block::prepare(self.mac) {
            self.fail(format!("prepare: {why}"));
            return;
        }
        self.observe(0);
        for to in STAGES {
            if self.over_budget() {
                self.fail(format!(
                    "over the {}s budget before {to} users",
                    BUDGET.as_secs()
                ));
                break;
            }
            if let Err(why) = self.grow(to) {
                self.fail(format!("growing to {to} users: {why}"));
                break;
            }
            self.observe(to);
        }
        if self.created >= 1000 {
            self.one_process_experiment();
        }
        if self.created >= RESET_SAMPLE {
            self.reset_sample();
        }
    }

    /// Makes users up to `to`, timing the creation, the cache flush and how long until
    /// `getpwnam` in this process answers for each.
    fn grow(&mut self, to: u32) -> Result<(), String> {
        let from = self.created;
        let (spawns, started) = (self.mac.spawns(), Instant::now());
        let mut each = Samples::default();
        for i in from..to {
            let one = Instant::now();
            block::create_user(self.mac, i)?;
            each.push(one.elapsed());
            self.created = i + 1;
            if self.created.is_multiple_of(100) {
                println!(
                    "PROGRESS created={} elapsed_s={:.1}",
                    self.created,
                    started.elapsed().as_secs_f64()
                );
            }
            if self.over_budget() {
                return Err(format!(
                    "over the {}s budget after {} users",
                    BUDGET.as_secs(),
                    self.created
                ));
            }
        }
        let (took, made) = (started.elapsed(), to - from);
        let spawned = self.mac.spawns() - spawns;
        let each = each.summary().ok_or("no user made")?;
        self.say(
            "create",
            &format!(
                "users={made} from={from} to={to} total_s={:.2} spawns={spawned} spawns_per_user={:.1} per_user: {}",
                took.as_secs_f64(),
                spawned as f64 / f64::from(made),
                each.fields()
            ),
        );
        self.note(
            &format!("create_{made}_users_s"),
            format!(
                "{:.1} (p50 {} ms, p95 {} ms per user)",
                took.as_secs_f64(),
                ms(each.p50),
                ms(each.p95)
            ),
        );
        let flush = Instant::now();
        self.mac.flush();
        self.say("flush_cache", &format!("took_ms={}", ms(flush.elapsed())));
        self.visibility(from, to)
    }

    /// Ten more users one `dscl` each, then ten through one `dscl .` process that reads its
    /// commands from standard input, in the same directory state: does batching the writes
    /// help? A failure of the batch is the finding, not the harness's.
    fn one_process_experiment(&mut self) {
        if self.over_budget() {
            self.fail("over the budget before the one-process experiment".to_owned());
            return;
        }
        let (each, batch) = (1000..1010, 1010..1020);
        let (spawns, started) = (self.mac.spawns(), Instant::now());
        for i in each.clone() {
            if let Err(why) = block::create_user(self.mac, i) {
                return self.fail(format!("one-process experiment, plain user {i}: {why}"));
            }
        }
        let plain = started.elapsed();
        let plain_spawns = self.mac.spawns() - spawns;
        let (spawns, started) = (self.mac.spawns(), Instant::now());
        let made = self
            .mac
            .create_users_in_one_process(&batch.clone().collect::<Vec<_>>());
        let one = started.elapsed();
        let one_spawns = self.mac.spawns() - spawns;
        self.mac.flush();
        let found = batch
            .clone()
            .filter(|&i| RealMac::getpwnam(&block::user_name(i)) == Some(block::uid_of(i)))
            .count();
        let outcome = match &made {
            Ok(()) => "ok".to_owned(),
            Err(why) => format!("failed: {why}"),
        };
        self.say(
            "create_one_dscl_process",
            &format!(
                "at_users=1000 users=10 one_dscl_each: total_ms={} per_user_ms={} spawns={plain_spawns}; stdin_batch: total_ms={} per_user_ms={} spawns={one_spawns} outcome={outcome} visible_after_flush={found} of 10",
                ms(plain),
                ms(plain / 10),
                ms(one),
                ms(one / 10)
            ),
        );
        self.note(
            "create_per_user_ms_at_1000",
            format!(
                "one dscl each {} ms, one process {} ms ({outcome}, {found} of 10 visible)",
                ms(plain / 10),
                ms(one / 10)
            ),
        );
    }

    /// Polls `getpwnam` for users `from..to` until each answers with its uid.
    fn visibility(&mut self, from: u32, to: u32) -> Result<(), String> {
        let started = Instant::now();
        let mut pending: Vec<u32> = (from..to).collect();
        let mut seen = Samples::default();
        let mut passes = 0;
        while !pending.is_empty() && started.elapsed() < VISIBLE_WITHIN {
            passes += 1;
            pending.retain(|&i| {
                let visible = RealMac::getpwnam(&block::user_name(i)) == Some(block::uid_of(i));
                if visible {
                    seen.push(started.elapsed());
                }
                !visible
            });
            if !pending.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let fields = seen.summary().map_or_else(String::new, |s| s.fields());
        self.say(
            "visible",
            &format!(
                "users={} not_visible={} passes={passes} (time from the first look, after the flush) {fields}",
                to - from,
                pending.len()
            ),
        );
        if let Some(s) = seen.summary() {
            self.note(&format!("getpwnam_visible_{to}_max_ms"), ms(s.max));
        }
        if pending.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} users not visible to getpwnam within {VISIBLE_WITHIN:?}",
                pending.len()
            ))
        }
    }

    /// Everything measured at a user count: lookup latency, the listings the pool's check
    /// reads, the check itself, and `opendirectoryd`'s size.
    fn observe(&mut self, users: u32) {
        if self.over_budget() {
            self.fail(format!("over the budget before observing {users} users"));
            return;
        }
        self.lookups(users);
        if users > 0 {
            self.check_cost(users);
        }
        match self.mac.opendirectoryd_rss_kb() {
            Some(kb) => {
                self.say("opendirectoryd", &format!("at_users={users} rss_kb={kb}"));
                self.note(&format!("opendirectoryd_rss_kb_at_{users}"), kb.to_string());
            }
            None => self.say(
                "opendirectoryd",
                &format!("at_users={users} rss_kb=unknown"),
            ),
        }
    }

    /// Lookup latency with `users` test users in the directory: in this process
    /// (`getpwnam`, a hit and a miss), and by program (`id`, `dscl -read`), against the
    /// cost of starting a program at all; and the two whole-directory reads.
    fn lookups(&mut self, users: u32) {
        let mut hit = Samples::default();
        for _ in 0..200 {
            let t = Instant::now();
            let _ = RealMac::getpwnam("root");
            hit.push(t.elapsed());
        }
        let mut miss = Samples::default();
        for i in 0..50 {
            let t = Instant::now();
            let _ = RealMac::getpwnam(&format!("kbf-scale-miss-{users}-{i}"));
            miss.push(t.elapsed());
        }
        self.series("getpwnam_hit_in_process", users, &hit);
        self.series("getpwnam_miss_in_process", users, &miss);
        let last = block::user_name(users.saturating_sub(1));
        let record = format!("/Users/{last}");
        let root_record = format!("/Users/{}", "root");
        let mut programs: Vec<(&str, &str, Vec<&str>)> = vec![
            ("spawn_floor_true", "/usr/bin/true", vec![]),
            ("id_root", "/usr/bin/id", vec!["-u", "root"]),
            (
                "dscl_read_root",
                DSCL,
                vec![".", "-read", root_record.as_str(), "UniqueID"],
            ),
        ];
        if users > 0 {
            programs.push(("id_test_user", "/usr/bin/id", vec!["-u", &last]));
            programs.push((
                "dscl_read_test_user",
                DSCL,
                vec![".", "-read", &record, "UniqueID"],
            ));
        }
        for (name, program, args) in programs {
            let mut s = Samples::default();
            for _ in 0..20 {
                match self.mac.timed(program, &args) {
                    Ok(t) => s.push(t.took),
                    Err(why) => return self.fail(format!("{name}: {why}")),
                }
            }
            self.series(name, users, &s);
        }
        for (name, args) in [
            (
                "dscl_list_users_uid",
                vec![".", "-list", "/Users", "UniqueID"],
            ),
            (
                "dscl_readall_users_6_attrs",
                vec![
                    "-plist",
                    ".",
                    "-readall",
                    "/Users",
                    "UniqueID",
                    "PrimaryGroupID",
                    "NFSHomeDirectory",
                    "UserShell",
                    "IsHidden",
                    "Password",
                ],
            ),
        ] {
            let mut s = Samples::default();
            let mut bytes = 0;
            for _ in 0..3 {
                match self.mac.timed(DSCL, &args) {
                    Ok(t) => {
                        s.push(t.took);
                        bytes = t.stdout;
                    }
                    Err(why) => return self.fail(format!("{name}: {why}")),
                }
            }
            self.say(
                name,
                &format!(
                    "at_users={users} stdout_bytes={bytes} {}",
                    s.summary().map_or_else(String::new, |x| x.fields())
                ),
            );
            if let Some(x) = s.summary() {
                self.note(&format!("{name}_p50_ms_at_{users}"), ms(x.p50));
            }
        }
    }

    fn series(&mut self, name: &str, users: u32, samples: &Samples) {
        if let Some(s) = samples.summary() {
            self.say(
                &format!("lookup {name}"),
                &format!("at_users={users} {}", s.fields()),
            );
            if matches!(
                name,
                "getpwnam_miss_in_process"
                    | "id_root"
                    | "id_test_user"
                    | "dscl_read_root"
                    | "spawn_floor_true"
            ) {
                self.note(&format!("{name}_p50_ms_at_{users}"), ms(s.p50));
            }
        }
    }

    /// The pool's per-user check (`pool.rs` `user_problems`) run against test users: every
    /// user up to 100, an even sample of 25 above that, and the whole-pool figure either
    /// measured or extrapolated from the mean (it says which).
    fn check_cost(&mut self, users: u32) {
        let picks: Vec<u32> = if users <= 100 {
            (0..users).collect()
        } else {
            (0..25).map(|k| k * (users / 25)).collect()
        };
        let mut each = Samples::default();
        let mut spawns = 0;
        let started = Instant::now();
        for &i in &picks {
            match self.mac.check_user(i) {
                Ok((took, n)) => {
                    each.push(took);
                    spawns = n;
                }
                Err(why) => return self.fail(format!("check of kbf-scale-{i}: {why}")),
            }
        }
        let Some(s) = each.summary() else { return };
        let whole = if picks.len() == users as usize {
            format!(
                "whole_check_s={:.2} (measured)",
                started.elapsed().as_secs_f64()
            )
        } else {
            format!(
                "whole_check_s={:.2} (extrapolated: mean x users)",
                s.mean.as_secs_f64() * f64::from(users)
            )
        };
        self.say(
            "check",
            &format!(
                "at_users={users} spawns_per_user={spawns} {whole} per_user: {}",
                s.fields()
            ),
        );
        self.note(
            &format!("check_per_user_p50_ms_at_{users}"),
            format!("{} ({spawns} spawns)", ms(s.p50)),
        );
        self.note(&format!("check_whole_pool_s_at_{users}"), whole);
    }

    /// One full reset (today's clean of a slot, `Slot::release`) for each of a sample of
    /// the users: first with state in the home, the `/var/folders` entry and a process,
    /// then of a slot nothing used.
    fn reset_sample(&mut self) {
        if self.over_budget() {
            self.fail("over the budget before the reset phase".to_owned());
            return;
        }
        if let Err(why) = self.reset_rounds() {
            self.fail(format!("reset: {why}"));
        }
    }

    fn reset_rounds(&mut self) -> Result<(), String> {
        let step = (self.created / RESET_SAMPLE).max(1);
        let users: Vec<SlotUser> = (0..RESET_SAMPLE)
            .map(|k| k * step)
            .map(|i| SlotUser {
                name: block::user_name(i),
                uid: block::uid_of(i),
                gid: FIRST_UID,
            })
            .collect();
        let pool = Pool::new(users.clone()).map_err(|e| e.to_string())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let started = Instant::now();
        let slots = Slots::start(&pool, WITHIN).map_err(|e| format!("Slots::start: {e}"))?;
        let took = started.elapsed();
        self.say(
            "reset_start",
            &format!(
                "users={RESET_SAMPLE} total_ms={} per_user_ms={} (resolve through the user's bootstrap, then one clean of an unused slot, each) usable={}",
                ms(took),
                ms(took / RESET_SAMPLE),
                slots.count()
            ),
        );
        self.note("reset_start_per_user_ms", ms(took / RESET_SAMPLE));
        for (round, plant_first) in [("dirty", true), ("idle", false)] {
            if self.over_budget() {
                return Err(format!("over the budget before the {round} round"));
            }
            let mut held = Vec::new();
            for _ in 0..RESET_SAMPLE {
                held.push(runtime.block_on(slots.take()).map_err(|e| e.to_string())?);
            }
            let mut planted = Planted::default();
            let mut each = Samples::default();
            for slot in held {
                if plant_first {
                    planted.add(plant(slot.ready()));
                }
                let t = Instant::now();
                runtime.block_on(slot.release());
                each.push(t.elapsed());
            }
            let left: usize = users
                .iter()
                .filter(|u| kbf_daemon::uid::processes(u.uid).is_ok_and(|p| !p.is_empty()))
                .count();
            let s: Summary = each.summary().ok_or("no reset timed")?;
            self.say(
                &format!("reset {round}"),
                &format!(
                    "{} usable_after={} users_with_processes_after={left} {}",
                    if plant_first {
                        planted.line()
                    } else {
                        "planted=nothing".to_owned()
                    },
                    slots.count(),
                    s.fields()
                ),
            );
            self.note(&format!("reset_{round}_p50_ms"), ms(s.p50));
            self.note(&format!("reset_{round}_p95_ms"), ms(s.p95));
            self.note(
                &format!("reset_{round}_usable_after"),
                format!("{} of {RESET_SAMPLE}", slots.count()),
            );
        }
        Ok(())
    }

    /// The closing summary: `SUMMARY key value` per kept number, in the order measured.
    pub fn print_summary(&self, extra: &[(String, String)]) {
        println!("SUMMARY ---- pool scale spike ----");
        for (key, value) in self.summary.iter().chain(extra) {
            println!("SUMMARY {key} {value}");
        }
        println!(
            "SUMMARY users_created {} elapsed_s {:.1} failures {}",
            self.created,
            self.started.elapsed().as_secs_f64(),
            self.failures.len()
        );
    }
}

/// What [`plant`] left in one slot.
#[derive(Default)]
struct Planted {
    files: usize,
    folders: usize,
    sleepers: usize,
}

impl Planted {
    fn add(&mut self, (files, folders, sleepers): (usize, usize, usize)) {
        self.files += files;
        self.folders += folders;
        self.sleepers += sleepers;
    }

    fn line(&self) -> String {
        format!(
            "planted files_in_homes={} files_in_var_folders={} processes={}",
            self.files, self.folders, self.sleepers
        )
    }
}

/// State for a reset to remove: 200 files of 4 KiB in 5 directories of the home, a file
/// in each of `0`, `C` and `T` of the `/var/folders` entry that exists, and a `sleep` as
/// the user. All the user's own. Returns (files in the home, files in the entry, processes).
fn plant(ready: &Ready) -> (usize, usize, usize) {
    let (uid, gid) = (ready.user.uid, ready.user.gid);
    let mut files = 0;
    let put = |path: &Path, bytes: &[u8]| -> bool {
        std::fs::write(path, bytes).is_ok()
            && std::os::unix::fs::chown(path, Some(uid), Some(gid)).is_ok()
    };
    let block = vec![0x6b_u8; 4096];
    for d in 0..5 {
        let dir = ready.home.join(format!("d{d}"));
        let made = std::fs::create_dir(&dir).is_ok()
            && std::os::unix::fs::chown(&dir, Some(uid), Some(gid)).is_ok()
            && std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).is_ok();
        for f in 0..40 {
            files += usize::from(made && put(&dir.join(format!("f{f}")), &block));
        }
    }
    let folders = ["0", "C", "T"]
        .iter()
        .map(|sub| ready.folders.join(sub))
        .filter(|dir| dir.is_dir())
        .filter(|dir| put(&dir.join("kbf-scale-planted"), &block))
        .count();
    let sleeper = Command::new("/bin/sleep")
        .arg("300")
        .uid(uid)
        .gid(gid)
        .stdin(Stdio::null())
        .spawn();
    let sleepers = match sleeper {
        Ok(mut child) => {
            // Reaped by its own thread: a zombie of this process would still be "a
            // process of the uid" to the clean's check.
            std::thread::spawn(move || child.wait());
            1
        }
        Err(_) => 0,
    };
    (files, folders, sleepers)
}
