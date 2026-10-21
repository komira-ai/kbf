//! What a pool user can leave behind, what the clean removes, and what proves it gone: a
//! measurement spike (it prints, it asserts only that the harness ran and removed what it
//! made). Design: the CEO's rule that clean-up is deterministic and clean state is
//! asserted, so every channel where a build might leave state is planted as a pool user
//! under four profiles (A: the shipped slot profile, B: A plus the POSIX/SysV IPC and
//! mach-register denies, C: B plus the state-creating classes, R: no sandbox), probed as
//! root while the lease is live, cleaned by S (the shipped `Slot::release`) or P (the
//! proposed clean in `cs.sh`, with mutants), and probed again after the clean and after the
//! user is deleted and made again.
//!
//! Output lines (grep them): `CELL`, `ASSERT`, `SNAPDIFF`, `CLEAN`, `WIPE`, `PINFO`, `INFO`,
//! `OPUNKNOWN`, `ARM`, `BUILD*`, `POSCTL`, `NOISE`, `B1DIFF`, `DRIFT`, `SENTINEL`, `SUMMARY`.
//!
//! The plants and probes are shell (`cs.sh`), because they are the commands a build runs;
//! the Rust is the harness: users, the real `Slots`/`Slot::launch` (the driver's own wrap
//! of the profile, `__as-user` and `launchctl asuser`), the shipped clean, the timeouts.

#![cfg(target_os = "macos")]
#![allow(dead_code)]

#[path = "../support/mod.rs"]
mod support;

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use kbf_driver_native::network::{Isolation, Network};
use kbf_driver_native::slots::macos::{Slot, Slots};
use kbf_driver_native::slots::{Pool, SlotUser};

const WORK: &str = "/private/var/kbf-cs-work";
const LEASES: &str = "/private/var/kbf-cs-leases.noindex";
const HOMES: &str = "/private/var/kbf-cs-homes.noindex";
const UID0: u32 = 61000;
const NUSERS: u32 = 8;
/// The user the cells run as; users 6 and 7 are the "other" uid of the cross-uid probes.
const CELL_USER: u32 = 0;

const CS_SH: &str = include_str!("cs.sh");
const CANARY_C: &str = include_str!("canary.c");

const B_RULES: &[&str] = &["(deny ipc-posix*)", "(deny ipc-sysv*)", "(deny mach-register)"];
const C_RULES: &[&str] = &[
    "(deny job-creation)",
    "(deny lsopen)",
    "(deny user-preference-write)",
    "(deny file-mount)",
    "(deny file-write-setugid)",
    "(deny darwin-notification-post)",
    "(deny distributed-notification-post)",
    "(deny network-outbound (remote tcp \"*:22\"))",
    "(deny network-outbound (remote tcp \"*:5900\"))",
    "(deny sysctl-write)",
    "(deny nvram*)",
    "(deny system-set-time)",
    "(deny authorization-right-obtain)",
    "(deny process-exec (literal \"/usr/bin/crontab\") (literal \"/usr/bin/at\") (literal \"/usr/bin/sudo\") (literal \"/usr/bin/su\") (literal \"/usr/bin/login\"))",
    "(deny mach-lookup (global-name \"com.apple.coreservices.launchservicesd\") (global-name \"com.apple.backgroundtaskmanagementd\") (global-name \"com.apple.tccd\") (global-name \"com.apple.pasteboard.1\") (global-name-prefix \"com.apple.CoreSimulator.\") (global-name-prefix \"com.apple.coreservices.appleevents\"))",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    A,
    B,
    C,
    R,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::A => "A",
            Arm::B => "B",
            Arm::C => "C",
            Arm::R => "R",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clean {
    /// The shipped `Slot::release`.
    S,
    /// The proposed clean in `cs.sh` (mode: P, P-kill-first, P-rm-first, P-noflags, P-nounmount).
    P(&'static str),
    /// P run by a system LaunchDaemon (no grants of the runner's step).
    PDaemon,
    /// No clean: the cell only plants and probes.
    None,
}

impl Clean {
    fn name(self) -> String {
        match self {
            Clean::S => "S".into(),
            Clean::P(m) => m.into(),
            Clean::PDaemon => "P-daemon".into(),
            Clean::None => "-".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    Planted,
    Denied,
    Skipped,
    Error,
}

/// What one root or user command printed.
struct Out {
    code: Option<i32>,
    timed_out: bool,
    text: String,
}

static OUT_N: AtomicU32 = AtomicU32::new(0);

/// Runs `cmd` with stdin closed and stdout and stderr in one file, killing it at `secs`.
fn run_timeout(mut cmd: Command, secs: u64) -> Out {
    let n = OUT_N.fetch_add(1, Ordering::SeqCst);
    let path = format!("{WORK}/out.{n}");
    let file = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            return Out {
                code: None,
                timed_out: false,
                text: format!("no out file {path}: {e}"),
            };
        }
    };
    let err = file.try_clone().expect("clone out file");
    cmd.stdin(Stdio::null()).stdout(file).stderr(err);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Out {
                code: None,
                timed_out: false,
                text: format!("spawn: {e}"),
            };
        }
    };
    let started = Instant::now();
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if started.elapsed() > Duration::from_secs(secs) {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };
    let text = fs::read(&path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = fs::remove_file(&path);
    Out {
        code,
        timed_out,
        text,
    }
}

/// The variables of one cell, exported before every script of it.
#[derive(Clone, Default)]
struct Env {
    id: u32,
    gid: u32,
    name: String,
    home: String,
    folders: String,
    lease: String,
    marker: String,
    row: String,
    mk: String,
    stage: String,
    mode: String,
    snapx: String,
    other: String,
    other_id: u32,
    extra: Vec<(String, String)>,
}

impl Env {
    fn exports(&self) -> String {
        let mut s = format!(
            "export W='{WORK}' K='{WORK}/canary' ID='{}' GID='{}' U='{}' H='{}' F='{}' L='{}' M='{}' ROW='{}' MK='{}' LEASES='{LEASES}' HOMES='{HOMES}' OU='{}' OID='{}' STAGE='{}' MODE='{}' SNAPX='{}'; ",
            self.id,
            self.gid,
            self.name,
            self.home,
            self.folders,
            self.lease,
            self.marker,
            self.row,
            self.mk,
            self.other,
            self.other_id,
            self.stage,
            self.mode,
            self.snapx
        );
        for (k, v) in &self.extra {
            s.push_str(&format!("export {k}='{v}'; "));
        }
        s
    }

    /// `body` as root, after the exports and `cs.sh`.
    fn root(&self, body: &str, secs: u64) -> Out {
        let mut cmd = Command::new("/bin/bash");
        cmd.arg("-c")
            .arg(format!("{}. {WORK}/cs.sh; {body}", self.exports()));
        run_timeout(cmd, secs)
    }
}

struct Stats {
    cells: u32,
    planted: u32,
    denied: u32,
    skipped: u32,
    errors: u32,
    s_clean: (u32, u32),
    p_clean: (u32, u32),
    blind: u32,
    quarantined: u32,
}

struct Cs {
    rt: tokio::runtime::Runtime,
    iso: Isolation,
    counter: AtomicU32,
    rules_b: String,
    rules_c: String,
    stats: Mutex<Stats>,
    job: String,
    last_marker: Mutex<String>,
}

fn say(line: impl AsRef<str>) {
    println!("{}", line.as_ref());
}

fn short(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(n).collect()
}

fn user_of(i: u32) -> SlotUser {
    SlotUser {
        name: format!("kbf-cs-{i}"),
        uid: UID0 + i,
        gid: UID0,
    }
}

fn base_env(user: &SlotUser) -> Env {
    let other = user_of(NUSERS - 1);
    Env {
        id: user.uid,
        gid: user.gid,
        name: user.name.clone(),
        home: format!("{HOMES}/{}", user.name),
        folders: "/nonexistent-folders".into(),
        lease: format!("{LEASES}/none"),
        marker: "kbf00000000".into(),
        row: "NONE".into(),
        mk: format!("{WORK}/mk.none"),
        stage: "base".into(),
        mode: "P".into(),
        snapx: String::new(),
        other: other.name.clone(),
        other_id: other.uid,
        extra: Vec::new(),
    }
}

impl Cs {
    fn new(job: &str) -> Cs {
        support::pool::assert_root("clean_state_spike");
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .try_init();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        fs::create_dir_all(WORK).expect("work dir");
        fs::set_permissions(WORK, fs::Permissions::from_mode(0o755)).expect("chmod work");
        fs::create_dir_all(LEASES).expect("leases dir");
        fs::set_permissions(LEASES, fs::Permissions::from_mode(0o755)).expect("chmod leases");
        fs::write(format!("{WORK}/cs.sh"), CS_SH).expect("cs.sh");
        fs::write(format!("{WORK}/canary.c"), CANARY_C).expect("canary.c");
        fs::set_permissions(format!("{WORK}/cs.sh"), fs::Permissions::from_mode(0o644)).expect("chmod");
        let env = base_env(&user_of(CELL_USER));
        // The canary, with fewer verbs if the OS headers lack some.
        let built = env.root(
            "cd \"$W\" && if clang -O0 -o canary canary.c 2>build.log; then echo full; \
             elif clang -O0 -DNO_MACH -o canary canary.c 2>>build.log; then echo no-mach; \
             elif clang -O0 -DNO_MACH -DNO_NOTIFY -o canary canary.c 2>>build.log; then echo no-mach-no-notify; \
             else echo FAILED; fi; chmod 755 canary; head -c 600 build.log",
            300,
        );
        say(format!("CANARY-BUILD {}", short(&built.text, 700)));
        let sw = env.root("sw_vers | tr '\\n' ' '; uname -m; launchctl managername; echo uid-console=$(stat -f %u /dev/console)", 30);
        say(format!("ENV {}", short(&sw.text, 300)));
        // What an earlier run left, then the users.
        let sweep = env.root("cs_sweep", 600);
        say(format!("INFO sweep-start {}", short(&sweep.text, 200)));
        for i in 0..NUSERS {
            let u = user_of(i);
            let made = env.root(
                &format!(
                    "cs_mkuser '{}' {} {} '{HOMES}/{}'",
                    u.name, u.uid, u.gid, u.name
                ),
                120,
            );
            if made.code != Some(0) {
                panic!("cannot make user {}: {}", u.name, made.text);
            }
        }
        let iso = Isolation::detect();
        say(format!("INFO isolation {}", iso.name()));
        let (rules_b, rules_c) = Self::arms(&env);
        Cs {
            rt,
            iso,
            counter: AtomicU32::new(1),
            rules_b,
            rules_c,
            stats: Mutex::new(Stats {
                cells: 0,
                planted: 0,
                denied: 0,
                skipped: 0,
                errors: 0,
                s_clean: (0, 0),
                p_clean: (0, 0),
                blind: 0,
                quarantined: 0,
            }),
            job: job.to_owned(),
            last_marker: Mutex::new(String::new()),
        }
    }

    /// Every added operation compiled alone first; one the OS rejects is dropped and printed.
    fn arms(env: &Env) -> (String, String) {
        let ok = |rule: &str| -> bool {
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.args(["-p", &format!("(version 1)(allow default){rule}"), "/usr/bin/true"]);
            let o = run_timeout(c, 20);
            if o.code == Some(0) {
                true
            } else {
                say(format!("OPUNKNOWN {} :: {}", rule, short(&o.text, 160)));
                false
            }
        };
        let _ = env;
        let b: Vec<&str> = B_RULES.iter().copied().filter(|r| ok(*r)).collect();
        let c: Vec<&str> = C_RULES.iter().copied().filter(|r| ok(*r)).collect();
        let rules_b: String = b.iter().map(|r| format!("\n{r}")).collect();
        let rules_c: String = format!(
            "{rules_b}{}",
            c.iter().map(|r| format!("\n{r}")).collect::<String>()
        );
        say(format!("ARM B accepted {} of {} rules", b.len(), B_RULES.len()));
        say(format!("ARM C accepted {} of {} added rules", c.len(), C_RULES.len()));
        (rules_b, rules_c)
    }

    fn finish(&self) {
        let env = base_env(&user_of(CELL_USER));
        let sweep = env.root("cs_sweep", 600);
        say(format!("INFO sweep-end {}", short(&sweep.text, 200)));
        let s = self.stats.lock().unwrap();
        say(format!(
            "SUMMARY job={} cells={} planted={} denied={} skipped={} errors={} cleanS_gone={}/{} cleanP_gone={}/{} blind={} quarantined={}",
            self.job,
            s.cells,
            s.planted,
            s.denied,
            s.skipped,
            s.errors,
            s.s_clean.0,
            s.s_clean.1,
            s.p_clean.0,
            s.p_clean.1,
            s.blind,
            s.quarantined
        ));
        assert!(
            sweep.text.contains("users left: 0"),
            "the harness left users behind: {}",
            sweep.text
        );
    }

    fn plant_secs(row: &str) -> u64 {
        match row {
            "W09" => 900,
            "CH19" | "CH20" | "CH23" | "CH30" | "CH33" | "CH34" | "BR1" | "BR3" | "BR5" => 240,
            _ => 120,
        }
    }

    fn command_for(&self, slot: &Slot, arm: Arm, lease: &Path, script: &str) -> Command {
        let (program, mut args) = slot
            .launch(
                &self.iso,
                Network::On,
                lease,
                PathBuf::from("/bin/bash"),
                &["-c".to_owned(), script.to_owned()],
            )
            .expect("launch");
        match arm {
            Arm::A => {}
            Arm::B | Arm::C => {
                let i = args.iter().position(|a| a == "-p").expect("-p in the launch");
                let rules = if arm == Arm::B { &self.rules_b } else { &self.rules_c };
                args[i + 1].push_str(rules);
            }
            Arm::R => {
                let i = args.iter().position(|a| a == "--").expect("-- in the launch");
                args.truncate(i + 1);
                args.extend(["/bin/bash".to_owned(), "-c".to_owned(), script.to_owned()]);
            }
        }
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    }

    /// One cell: a fresh clean user, one plant under `arm`, probes, the clean, probes, and
    /// the user made again.
    fn cell(&self, row: &str, arm: Arm, clean: Clean, extra: &[(&str, &str)]) -> Verdict {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let tag = format!("{row} {} {}", arm.name(), clean.name());
        let user = user_of(CELL_USER);
        let mut env = base_env(&user);
        env.row = row.to_owned();
        env.marker = format!("kbf{n:08x}");
        env.mk = format!("{WORK}/mk.{}", env.marker);
        *self.last_marker.lock().unwrap() = env.marker.clone();
        env.mode = match clean {
            Clean::P(m) => m.to_owned(),
            _ => "P".to_owned(),
        };
        env.extra = extra
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        env.snapx = match row {
            "CH17" | "CH21" | "W04" => "btm".to_owned(),
            "CH14" | "CH15" | "W05" | "W06a" | "W06c" => "lsof".to_owned(),
            _ => String::new(),
        };
        self.stats.lock().unwrap().cells += 1;

        let pool = Pool::new(vec![user.clone()]).expect("pool");
        let slots = match Slots::start(&pool, Duration::from_secs(60)) {
            Ok(s) => s,
            Err(e) => {
                say(format!("CELLERR {tag} Slots::start: {e}"));
                self.stats.lock().unwrap().errors += 1;
                self.reset_user(&env);
                return Verdict::Error;
            }
        };
        if slots.count() == 0 {
            say(format!("CELLERR {tag} the slot is quarantined at start"));
            self.stats.lock().unwrap().errors += 1;
            drop(slots);
            self.reset_user(&env);
            return Verdict::Error;
        }
        let slot = self.rt.block_on(slots.take()).expect("take");
        env.home = slot.ready().home.to_string_lossy().into_owned();
        env.folders = slot.ready().folders.to_string_lossy().into_owned();
        let lease = PathBuf::from(format!("{LEASES}/L{n}"));
        fs::create_dir_all(&lease).expect("lease dir");
        env.lease = lease.to_string_lossy().into_owned();
        self.rt.block_on(slot.hand_over(&lease)).expect("hand over");

        let fixture = env.root(
            "cs_skel; hasfn fixture_$ROW && fixture_$ROW; touch \"$MK\"; echo fixture-done",
            120,
        );
        for line in fixture.text.lines().filter(|l| !l.is_empty() && *l != "fixture-done") {
            say(format!("INFO {tag} fixture {}", short(line, 200)));
        }
        if !fixture.text.contains("fixture-done") {
            say(format!("INFO {tag} fixture did not finish"));
        }
        let snaps = clean != Clean::None;
        let base = format!("{WORK}/snap.{}.base", env.marker);
        if snaps {
            let o = env.root(&format!("snap > '{base}' 2>&1"), 180);
            if o.timed_out {
                say(format!("INFO {tag} base snapshot timed out"));
            }
        }

        // The plant: the driver's own launch, profile by arm.
        let script = format!("{}. {WORK}/cs.sh; plant_run", env.exports());
        let started = Instant::now();
        let out = run_timeout(
            self.command_for(&slot, arm, &lease, &script),
            Self::plant_secs(row),
        );
        let verdict = self.report_plant(&tag, &out, started.elapsed());

        std::thread::sleep(Duration::from_millis(800));
        // Probes while the lease is live.
        env.stage = "leased".into();
        let leased = self.probe(&tag, "leased", &env, verdict);
        let leased_present = leased;
        if verdict == Verdict::Planted && !leased_present && !matches!(row, "CH35" | "CH37") {
            self.stats.lock().unwrap().blind += 1;
            say(format!("PROBE-BLIND {tag} plant succeeded but the probe reads absent while leased"));
        }
        if snaps && arm != Arm::C && arm != Arm::B {
            let leased_snap = format!("{WORK}/snap.{}.leased", env.marker);
            env.root(&format!("snap > '{leased_snap}' 2>&1"), 180);
            self.diff(&tag, "leased", &env, &base, &leased_snap);
        }
        // The driver's end of a lease: the lease directory goes (no mount crossed) before the slot is cleaned.
        let le = env.root("cs_lease_end", 120);
        if le.text.contains("LEASE-DIR-LEFT") {
            say(format!("INFO {tag} {}", short(&le.text, 160)));
        }

        // The clean.
        env.stage = "clean".into();
        let mut quarantined = false;
        let mut cleaned = false;
        let t = Instant::now();
        let mut pinfo = String::new();
        match clean {
            Clean::S => {
                self.rt.block_on(slot.release());
                quarantined = slots.count() == 0;
                cleaned = true;
            }
            Clean::P(_) => {
                std::mem::forget(slot);
                let o = env.root("cs_clean_p", 600);
                pinfo = short(&o.text, 300);
                cleaned = true;
            }
            Clean::PDaemon => {
                std::mem::forget(slot);
                let o = env.root("cs_clean_p_daemon", 600);
                pinfo = short(&o.text, 300);
                cleaned = true;
            }
            Clean::None => {
                std::mem::forget(slot);
            }
        }
        let took = t.elapsed();
        if cleaned {
            let aside = env.root("ls -d \"$HOMES\"/.kbf-recreated-\"$U\"-* 2>/dev/null | wc -l | tr -d ' '", 30);
            let recreated = aside.text.trim() != "0";
            let after = format!("{WORK}/snap.{}.after", env.marker);
            env.root(&format!("snap > '{after}' 2>&1"), 180);
            let present = self.probe(&tag, "clean", &env, verdict);
            let diffs = self.diff(&tag, "clean", &env, &base, &after);
            let word = if quarantined {
                "quarantined"
            } else if !present && diffs == 0 {
                "gone"
            } else if present {
                "left(probe)"
            } else {
                "left(snapshot)"
            };
            say(format!(
                "CLEAN {tag} {word} took_ms={} probe={} snapshot_diff_lines={diffs} quarantined={} recreated={} {pinfo}",
                took.as_millis(),
                if present { "present" } else { "absent" },
                if quarantined { "y" } else { "n" },
                if recreated { "y" } else { "n" },
            ));
            if row.starts_with('W') {
                say(format!(
                    "WIPE {row} {} {} took_ms={} quarantined={} recreated={}",
                    clean.name(),
                    if word == "gone" { "ok" } else { "failed(not-clean)" },
                    took.as_millis(),
                    if quarantined { "y" } else { "n" },
                    if recreated { "y" } else { "n" },
                ));
            }
            if verdict == Verdict::Planted {
                let mut st = self.stats.lock().unwrap();
                let ok = u32::from(word == "gone");
                match clean {
                    Clean::S => {
                        st.s_clean.0 += ok;
                        st.s_clean.1 += 1;
                    }
                    _ => {
                        st.p_clean.0 += ok;
                        st.p_clean.1 += 1;
                    }
                }
                if quarantined {
                    st.quarantined += 1;
                }
            }
        }
        drop(slots);
        // The user made again (what survives a delete and a create at the same uid).
        self.reset_user(&env);
        env.stage = "reset".into();
        self.probe(&tag, "reset", &env, verdict);
        let end = env.root("cs_cell_end", 120);
        for line in end.text.lines().filter(|l| l.starts_with("SENTINEL")) {
            say(format!("{line} ({tag})"));
        }
        verdict
    }

    fn reset_user(&self, env: &Env) {
        let u = user_of(CELL_USER);
        let o = env.root(
            &format!(
                "cs_rmuser '{}' {} '{HOMES}/{}'; cs_mkuser '{}' {} {} '{HOMES}/{}'",
                u.name, u.uid, u.name, u.name, u.uid, u.gid, u.name
            ),
            300,
        );
        if o.code != Some(0) {
            say(format!("INFO reset-user: {}", short(&o.text, 200)));
        }
    }

    fn report_plant(&self, tag: &str, out: &Out, took: Duration) -> Verdict {
        let mut verdict = Verdict::Denied;
        let mut any = false;
        let mut skipped = false;
        for line in out.text.lines() {
            if let Some(rest) = line.strip_prefix("PLANTED ") {
                say(format!("CELL {tag} planted {rest}"));
                verdict = Verdict::Planted;
                any = true;
            } else if let Some(rest) = line.strip_prefix("DENIED ") {
                say(format!("CELL {tag} denied {}", short(rest, 220)));
                any = true;
            } else if let Some(rest) = line.strip_prefix("SKIPPED ") {
                say(format!("CELL {tag} skipped {}", short(rest, 220)));
                skipped = true;
                any = true;
            } else if let Some(rest) = line.strip_prefix("INFO ") {
                say(format!("INFO {tag} {}", short(rest, 220)));
            }
        }
        if out.timed_out {
            say(format!("CELL {tag} timeout after {:?}", took));
        }
        if !any {
            say(format!(
                "CELL {tag} no-plant-output code={:?} out={}",
                out.code,
                short(&out.text, 300)
            ));
            self.stats.lock().unwrap().errors += 1;
            return Verdict::Error;
        }
        if verdict != Verdict::Planted && skipped {
            verdict = Verdict::Skipped;
        }
        let mut st = self.stats.lock().unwrap();
        match verdict {
            Verdict::Planted => st.planted += 1,
            Verdict::Denied => st.denied += 1,
            Verdict::Skipped => st.skipped += 1,
            Verdict::Error => st.errors += 1,
        }
        say(format!("CELLTIME {tag} plant_ms={}", took.as_millis()));
        verdict
    }

    /// Runs `probe_<ROW>`; prints `ASSERT <tag> <stage> present|absent`. True if present.
    fn probe(&self, tag: &str, stage: &str, env: &Env, _v: Verdict) -> bool {
        let o = env.root(
            "if hasfn probe_$ROW; then probe_$ROW; else echo \"PROBE $ROW none\"; fi",
            240,
        );
        let mut present = false;
        let mut seen = false;
        for line in o.text.lines() {
            if let Some(rest) = line.strip_prefix("PROBE ") {
                seen = true;
                if rest.contains(" present") {
                    present = true;
                }
            } else if line.starts_with("INFO ") {
                say(format!("INFO {tag} {stage} {}", short(&line[5..], 220)));
            } else if line.starts_with("BUILD") {
                say(format!("{line} ({tag} {stage})"));
            }
        }
        if !seen {
            say(format!("ASSERT {tag} {stage} probe-no-output {}", short(&o.text, 200)));
        } else {
            say(format!(
                "ASSERT {tag} {stage} {}",
                if present { "present" } else { "absent" }
            ));
        }
        present
    }

    /// `diff` of two snapshots; prints the count and up to 10 lines. Returns the count.
    fn diff(&self, tag: &str, stage: &str, env: &Env, a: &str, b: &str) -> usize {
        let o = env.root(&format!("sdiff_ '{a}' '{b}'"), 60);
        let lines: Vec<&str> = o.text.lines().filter(|l| !l.is_empty()).collect();
        // The launchd-own agents of the uid (distnoted, cfprefsd): the shipped clean's check
        // tolerates them too, so they are counted apart.
        let is_agent = |l: &&str| {
            l.contains("distnoted") || l.contains("cfprefsd") || l.contains("com.apple.rootless")
        };
        let agents = lines.iter().filter(|l| is_agent(*l)).count();
        let others = lines.len() - agents;
        say(format!(
            "SNAPDIFF {tag} {stage} lines={others} agent_lines={agents}"
        ));
        for l in lines.iter().take(12) {
            say(format!("  | {}", short(l, 190)));
        }
        others
    }

    /// The cells of one channel row.
    fn channel_row(&self, row: &str, mutants: &[&'static str]) {
        say(format!("ROW {row} start"));
        let a = self.cell(row, Arm::A, Clean::S, &[]);
        let arm = if a == Verdict::Planted { Arm::A } else { Arm::R };
        if a == Verdict::Planted {
            self.cell(row, Arm::A, Clean::P("P"), &[]);
        }
        self.cell(row, Arm::B, Clean::None, &[]);
        self.cell(row, Arm::C, Clean::None, &[]);
        self.cell(row, Arm::R, Clean::P("P"), &[]);
        if a != Verdict::Planted && a != Verdict::Skipped {
            self.cell(row, Arm::R, Clean::S, &[]);
        }
        for m in mutants {
            self.cell(row, arm, Clean::P(*m), &[]);
        }
        say(format!("ROW {row} done"));
    }

    /// The cells of one wipe row: S and P twins, the mutants that test an ordering claim.
    fn wipe_row(&self, row: &str, mutants: &[Clean]) {
        say(format!("ROW {row} start"));
        let a = self.cell(row, Arm::A, Clean::S, &[]);
        let arm = if a == Verdict::Planted {
            Arm::A
        } else {
            self.cell(row, Arm::R, Clean::S, &[]);
            Arm::R
        };
        self.cell(row, arm, Clean::P("P"), &[]);
        for m in mutants {
            self.cell(row, arm, *m, &[]);
        }
        say(format!("ROW {row} done"));
    }
}

fn mutants_of(row: &str) -> Vec<&'static str> {
    match row {
        "CH17" => vec!["P-kill-first"],
        "CH24" => vec!["P-rm-first"],
        "CH07" => vec!["P-nounmount"],
        _ => Vec::new(),
    }
}

fn channels(job: &str, rows: &[&str]) {
    let cs = Cs::new(job);
    for row in rows {
        cs.channel_row(row, &mutants_of(row));
    }
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users: cargo xtask sudo-test -p kbf-driver-native --test clean_state_spike -- --nocapture channels_1"]
fn channels_1() {
    channels(
        "channels_1",
        &["CH01", "CH02", "CH03", "CH04", "CH05", "CH06", "CH07", "CH08", "CH09", "CH10", "CH11", "CH12"],
    );
}

#[test]
#[ignore = "needs root and makes real users"]
fn channels_2() {
    channels(
        "channels_2",
        &["CH13", "CH14", "CH15", "CH16", "CH17", "CH18", "CH19", "CH20", "CH21", "CH22"],
    );
}

#[test]
#[ignore = "needs root and makes real users"]
fn channels_3() {
    channels(
        "channels_3",
        &["CH23", "CH24", "CH25", "CH26", "CH27", "CH28", "CH29", "CH30"],
    );
}

#[test]
#[ignore = "needs root and makes real users"]
fn channels_4() {
    channels(
        "channels_4",
        &["CH31", "CH32", "CH33", "CH34", "CH35", "CH36", "CH37"],
    );
}

#[test]
#[ignore = "needs root and makes real users"]
fn wipe_1() {
    let cs = Cs::new("wipe_1");
    cs.wipe_row("W01", &[Clean::P("P-noflags")]);
    cs.wipe_row("W02", &[]);
    cs.wipe_row("W03", &[]);
    cs.wipe_row("W04", &[Clean::P("P-kill-first")]);
    cs.wipe_row("W05", &[]);
    cs.wipe_row("W06a", &[]);
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn wipe_2() {
    let cs = Cs::new("wipe_2");
    cs.wipe_row("W06b", &[Clean::P("P-nounmount")]);
    cs.wipe_row("W06c", &[]);
    cs.wipe_row("W07", &[Clean::PDaemon]);
    cs.wipe_row("W08", &[]);
    cs.wipe_row("W10", &[]);
    cs.wipe_row("W11", &[]);
    say("SKIP W12 reason=runner-safety");
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn wipe_3() {
    let cs = Cs::new("wipe_3");
    cs.wipe_row("W09", &[]);
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn baseline_and_builds() {
    let cs = Cs::new("baseline_and_builds");
    let env0 = base_env(&user_of(CELL_USER));
    let dirs = "find /private/var /private/tmp /Library /Users /usr/local /opt -xdev -newer";
    let top = "2>/dev/null | cut -d/ -f1-5 | sort | uniq -c | sort -rn";

    // B2 first, while nothing of the spike runs: the idle window (users exist, nobody uses them).
    let o = env0.root(
        &format!(
            "touch \"$W/mk.idle\"; sleep 60; {dirs} \"$W/mk.idle\" {top} | head -60 > \"$W/drift.idle\"; sed -E 's/^ *[0-9]+ //' \"$W/drift.idle\" > \"$W/drift.idle.dirs\"; head -25 \"$W/drift.idle\""
        ),
        400,
    );
    for l in o.text.lines() {
        say(format!("DRIFT idle {}", short(l, 160)));
    }
    env0.root("touch \"$W/mk.active\"", 10);

    // B0: users 5, 6, 7 were made and never used. Their snapshots must match once
    // the names, uids and homes are normalised (the noise test); the dscl hash is left
    // out (it holds the GeneratedUID) and the folders entry does not exist yet.
    let mut snaps = Vec::new();
    for i in [5u32, 6, 7] {
        let u = user_of(i);
        let mut e = base_env(&u);
        e.row = "B0".into();
        e.marker = format!("kbfb0{i:06x}");
        e.mk = format!("{WORK}/mk.{}", e.marker);
        let path = format!("{WORK}/snap.b0.{i}");
        let o = e.root(
            &format!(
                "touch \"$MK\"; snap | awk '/^== /{{s=$2}} s!=\"ds\" && s!=\"folders\"{{print}}' | sed -e \"s#$H#<H>#g\" -e \"s#$U#<U>#g\" -e \"s/$ID/<ID>/g\" > '{path}' 2>&1; wc -l < '{path}'"
            ),
            180,
        );
        say(format!("INFO B0 user {} snapshot lines {}", u.name, short(&o.text, 40)));
        snaps.push(path);
    }
    for pair in [(0usize, 1usize), (1, 2)] {
        let o = env0.root(&format!("sdiff_ '{}' '{}'", snaps[pair.0], snaps[pair.1]), 30);
        let n = o.text.lines().filter(|l| !l.is_empty()).count();
        say(format!(
            "NOISE B0 f{} vs f{}: {}",
            pair.0,
            pair.1,
            if n == 0 { "identical".to_owned() } else { format!("DIFFERENT lines={n}") }
        ));
        for l in o.text.lines().take(10) {
            say(format!("  | {}", short(l, 190)));
        }
    }

    // B1: user 0 ran `true` once under arm A and was cleaned by S: what merely using a
    // pool user leaves (its snapshot after the clean against the never-used B0).
    let cell = cs.cell("B1", Arm::A, Clean::S, &[]);
    say(format!("INFO B1 cell verdict {cell:?}"));
    let m = cs.last_marker.lock().unwrap().clone();
    let o = env0.root(
        &format!(
            "awk '/^== /{{s=$2}} s!=\"ds\" && s!=\"folders\"{{print}}' '{WORK}/snap.{m}.after' | sed -e 's#{HOMES}/kbf-cs-0#<H>#g' -e 's/kbf-cs-0/<U>/g' -e 's/61000/<ID>/g' > '{WORK}/b1.norm'; sdiff_ '{}' '{WORK}/b1.norm'",
            snaps[0]
        ),
        60,
    );
    let n = o.text.lines().filter(|l| !l.is_empty()).count();
    say(format!("B1DIFF B0 (never used) vs B1 (used once, cleaned by S): lines={n}"));
    for l in o.text.lines().take(15) {
        say(format!("  | {}", short(l, 190)));
    }

    // B3: positive controls for the probes.
    let o = env0.root("cs_posctl", 300);
    for l in o.text.lines() {
        say(format!("POSCTL {}", short(l, 220)));
    }

    // B4: real builds as a pool user under arm A, with the driver's environment and a hostile one.
    let o = env0.root("cs_prepare_rust", 120);
    say(format!("INFO prepare-rust {}", short(&o.text, 200)));
    for row in ["BR1", "BR2", "BR3", "BR4", "BR5"] {
        for benv in ["driver", "hostile"] {
            cs.cell(row, Arm::A, Clean::S, &[("BENV", benv)]);
        }
    }

    // B2, the active window (B1, the builds): what moved on the machine beyond the idle set
    // and the runner's own home.
    let o = env0.root(
        &format!(
            "{dirs} \"$W/mk.active\" {top} | grep -v '/Users/runner/' | head -60 | while read -r n d; do grep -qxF \"$d\" \"$W/drift.idle.dirs\" || echo \"$n $d\"; done | head -30"
        ),
        400,
    );
    for l in o.text.lines() {
        say(format!("DRIFT active-minus-idle {}", short(l, 160)));
    }
    let o = env0.root("cs_inventory", 400);
    for l in o.text.lines() {
        say(format!("INVENTORY {}", short(l, 200)));
    }
    cs.finish();
}
