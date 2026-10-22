//! Experiment round E1 of the Mac pool plan (spike only; never for main). Each test is one slice
//! of `.github/workflows/spike-slot-reach.yml`. The cells reuse the harness of `main.rs`; the
//! plants and probes are the `E1` section of `cs.sh`.
//!
//! Lines to grep: `E1ROW`, `E1DOOR`, `E1BUILD`, `E1BISECT`, `E1CTL`, `BTMCTL`, `OLDPROC`, `LATE`,
//! `BASEDIFF`, `TCCPROBE`, `LEASE-END-REAL`, plus the harness's `CELL`, `CLEAN`, `ASSERT`, `MID`.

use std::collections::BTreeMap;

use super::*;

/// What one cell planted and denied (by subrow), whether the leased probe read present, and
/// the clean's word.
#[derive(Clone, Default)]
struct Res {
    planted: Vec<String>,
    denied: Vec<String>,
    leased: bool,
    word: String,
}

impl Res {
    fn any_planted(&self) -> bool {
        !self.planted.is_empty()
    }
}

fn run(cs: &Cs, row: &str, arm: Arm, clean: Clean, extra: &[(&str, &str)]) -> Res {
    cs.cell(row, arm, clean, extra);
    Res {
        planted: cs.last_planted.lock().unwrap().clone(),
        denied: cs.last_denied.lock().unwrap().clone(),
        leased: *cs.last_leased.lock().unwrap(),
        word: cs.last_word.lock().unwrap().clone(),
    }
}

fn state(r: &Res, sub: &str) -> &'static str {
    if r.planted.iter().any(|s| s == sub) {
        "open"
    } else if r.denied.iter().any(|s| s == sub) {
        "closed"
    } else {
        "none"
    }
}

/// One line per subrow: its state under each arm (`open` = the plant succeeded, `closed` =
/// it was refused, `none` = it printed neither).
fn door_table(kind: &str, row: &str, results: &[(Arm, Res)]) {
    let mut subs: BTreeMap<String, ()> = BTreeMap::new();
    for (_, r) in results {
        for s in r.planted.iter().chain(r.denied.iter()) {
            subs.insert(s.clone(), ());
        }
    }
    for sub in subs.keys() {
        let cols: Vec<String> = results
            .iter()
            .map(|(a, r)| format!("{}={}", a.name(), state(r, sub)))
            .collect();
        say(format!("{kind} {row} {sub} :: {}", cols.join(" ")));
    }
}

/// X-L rows: the unsandboxed control first (the shape `tests/slot_clean.rs` plants), then the
/// shipped profile (A), then v2; then the cleans' order mutants on the arm that planted.
fn xl_row(cs: &Cs, row: &str, mutants: bool) {
    say(format!("ROW {row} start"));
    let r = run(cs, row, Arm::R, Clean::P("P"), &[]);
    let a = run(cs, row, Arm::A, Clean::S, &[]);
    let v = run(cs, row, Arm::V2, Clean::S, &[]);
    let word = |x: &Res| {
        format!(
            "planted={} leased-probe={} clean={}",
            if x.any_planted() { "y" } else { "n" },
            if x.leased { "present" } else { "absent" },
            x.word
        )
    };
    say(format!(
        "E1ROW {row} R[{}] A[{}] V2[{}] control={}",
        word(&r),
        word(&a),
        word(&v),
        if r.any_planted() && r.leased {
            "OK(R planted and the probe saw it)"
        } else if r.any_planted() {
            "PROBE-BLIND(R planted, the probe read absent)"
        } else {
            "BLIND(R did not plant: A and V2 columns prove nothing)"
        }
    ));
    if mutants {
        let arm = if a.any_planted() { Arm::A } else { Arm::R };
        for m in ["P", "P-kill-first", "P-nokill"] {
            let x = run(cs, row, arm, Clean::P(m), &[]);
            say(format!("E1MUT {row} arm={} mode={m} {}", arm.name(), word(&x)));
        }
    }
    say(format!("ROW {row} done"));
}

#[test]
#[ignore = "needs root and makes real users: cargo xtask sudo-test -p kbf-driver-native --test clean_state_spike -- --nocapture e1_xl"]
fn e1_xl() {
    let cs = Cs::new("e1_xl");
    let env0 = base_env(&user_of(CELL_USER));
    // The BTM probe's positive controls, once: a job in a gui domain, and plist FILES with no job.
    let o = env0.root("cs_btmctl", 300);
    for l in o.text.lines() {
        say(format!("BTMCTL-RAW {}", short(l, 220)));
    }
    for row in ["XL_ka", "XL_sub", "XL_load", "XL_btm", "XL_resp"] {
        xl_row(&cs, row, row == "XL_ka" || row == "XL_resp");
    }
    // Which rule of A closes launchd: A without its file-write deny.
    for row in ["XL_ka", "XL_btm"] {
        for arm in [Arm::ANoWrite, Arm::AllowAll] {
            let x = run(&cs, row, arm, Clean::S, &[]);
            say(format!("E1ROW {row} {} planted={} leased-probe={} clean={}", arm.name(), x.any_planted(), x.leased, x.word));
        }
    }
    xl_row(&cs, "W04x", true);
    xl_row(&cs, "W11x", false);
    // X8: BTM by plist file in ~/Library/LaunchAgents, no job.
    say("ROW X8 start");
    let r = run(&cs, "X8", Arm::R, Clean::P("P"), &[]);
    let a = run(&cs, "X8", Arm::A, Clean::S, &[]);
    let p = run(&cs, "X8", Arm::A, Clean::P("P"), &[]);
    say(format!(
        "E1ROW X8 R[planted={} btm-probe={} clean={}] A/S[planted={} btm-probe={} clean={}] A/P[planted={} btm-probe={} clean={}]",
        r.any_planted(), r.leased, r.word, a.any_planted(), a.leased, a.word, p.any_planted(), p.leased, p.word
    ));
    say("ROW X8 done");
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_canaries() {
    let cs = Cs::new("e1_canaries");
    // The canary suite under every arm: the unsandboxed control, the shipped profile, v2, and v2 with
    // one deny removed (the profile mutant: that door must open).
    say("ROW XCAN start");
    let mut results = vec![
        (Arm::R, run(&cs, "XCAN", Arm::R, Clean::S, &[])),
        (Arm::A, run(&cs, "XCAN", Arm::A, Clean::S, &[])),
        (Arm::V2, run(&cs, "XCAN", Arm::V2, Clean::S, &[])),
    ];
    for i in 0..V2_RULES.len() {
        results.push((Arm::V2No(i), run(&cs, "XCAN", Arm::V2No(i), Clean::S, &[])));
    }
    door_table("E1DOOR", "XCAN", &results);
    say("ROW XCAN done");

    // X1: the same doors as the console user (uid 501), which has a GUI session and real domains.
    say("ROW X1 start");
    cs.runner.store(true, Ordering::SeqCst);
    let mut x1 = Vec::new();
    for arm in [
        Arm::R,
        Arm::A,
        Arm::V2,
        Arm::V2No(2),
        Arm::V2No(3),
    ] {
        x1.push((arm, run(&cs, "X1", arm, Clean::None, &[])));
    }
    cs.runner.store(false, Ordering::SeqCst);
    door_table("E1DOOR", "X1", &x1);
    say("ROW X1 done");
    cs.finish();
}

/// The build rows under the lease environment: R, A, V2; where a subrow works under R and A and
/// fails under V2, each v2 deny alone is tried on that row (the bisect).
fn build_matrix(cs: &Cs, rows: &[&str]) {
    let ex = [("BENV", "lease")];
    for row in rows {
        say(format!("ROW {row} start"));
        let r = run(cs, row, Arm::R, Clean::None, &ex);
        let a = run(cs, row, Arm::A, Clean::None, &ex);
        let v = run(cs, row, Arm::V2, Clean::None, &ex);
        let base = vec![(Arm::R, r), (Arm::A, a.clone()), (Arm::V2, v.clone())];
        door_table("E1BUILD", row, &base);
        let broken: Vec<&String> = a
            .planted
            .iter()
            .filter(|s| v.denied.contains(s) || !v.planted.contains(s))
            .collect();
        if !broken.is_empty() {
            say(format!(
                "E1BISECT {row} subrows that pass under A and fail under V2: {broken:?}; one v2 deny at a time"
            ));
            let mut singles = Vec::new();
            for i in 0..V2_RULES.len() {
                singles.push((Arm::V2Only(i), run(cs, row, Arm::V2Only(i), Clean::None, &ex)));
            }
            door_table("E1BISECT", row, &singles);
        }
        say(format!("ROW {row} done"));
    }
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_builds() {
    let cs = Cs::new("e1_builds");
    let o = base_env(&user_of(CELL_USER)).root("cs_prepare_rust", 120);
    say(format!("INFO prepare-rust {}", short(&o.text, 200)));
    build_matrix(&cs, &["BR1", "BR2", "BR3", "BR4", "BR5", "BR6", "BR7"]);
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_xcode() {
    let cs = Cs::new("e1_xcode");
    let env0 = base_env(&user_of(CELL_USER));
    let o = env0.root(
        "xcode-select -p; xcodebuild -version | tr '\\n' ' '; xcrun simctl list runtimes available | head -5",
        120,
    );
    say(format!("INFO xcode {}", short(&o.text, 400)));
    build_matrix(&cs, &["BR8", "BR8i"]);
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_x5() {
    let cs = Cs::new("e1_x5");
    for row in ["X5a", "X5c", "X5d"] {
        say(format!("ROW {row} start"));
        let r = run(&cs, row, Arm::R, Clean::P("P"), &[]);
        let a = run(&cs, row, Arm::A, Clean::S, &[]);
        let p = run(&cs, row, Arm::A, Clean::P("P"), &[]);
        let v = run(&cs, row, Arm::V2, Clean::S, &[]);
        say(format!(
            "E1ROW {row} R[planted={} clean={}] A/S[planted={} clean={}] A/P[planted={} clean={}] V2[planted={} denied={:?}]",
            r.any_planted(), r.word, a.any_planted(), a.word, p.any_planted(), p.word, v.any_planted(), v.denied
        ));
        say(format!("ROW {row} done"));
    }
    run(&cs, "X5b", Arm::A, Clean::S, &[]);
    cs.finish();
}

fn tcc(job: &str, dir: Option<&str>, pre: Option<&str>) {
    let cs = Cs::new_with(job, dir, pre);
    // TCC plants in a pool home, probed from the runner step's context and from a system LaunchDaemon (root,
    // no grant), against the console user's own home as the positive control (TCCPROBE ctl ...).
    run(&cs, "TCC", Arm::A, Clean::S, &[]);
    run(&cs, "TCC", Arm::A, Clean::P("P"), &[]);
    run(&cs, "TCC", Arm::A, Clean::PDaemon, &[]);
    cs.wipe_row("W07", &[Clean::PDaemon]);
    cs.finish();
    if pre.is_some() {
        let o = base_env(&user_of(CELL_USER)).root("cs_tcc_vol_end", 120);
        say(format!("INFO tcc-vol-end {}", short(&o.text, 200)));
    }
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_tcc_vol() {
    tcc("e1_tcc_vol", Some("/Volumes/kbfpool/home"), Some("cs_tcc_vol"));
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_tcc_users() {
    tcc("e1_tcc_users", Some("/Users"), None);
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_tcc_var() {
    tcc("e1_tcc_var", None, None);
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_x6() {
    let cs = Cs::new("e1_x6");
    let a = run(&cs, "X6", Arm::A, Clean::S, &[]);
    let p = run(&cs, "X6", Arm::A, Clean::P("P"), &[]);
    say(format!(
        "E1ROW X6 A/S[planted={} leased-probe={} clean={}] A/P[planted={} leased-probe={} clean={}]",
        a.any_planted(), a.leased, a.word, p.any_planted(), p.leased, p.word
    ));
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_x7() {
    let cs = Cs::new("e1_x7");
    cs.late.store(true, Ordering::SeqCst);
    // Round 1 showed the controls and the quiet rows; this round: the simulator row with the late arrivals grouped,
    // and the builds under the lease environment.
    run(&cs, "CH34", Arm::A, Clean::S, &[]);
    run(&cs, "CH34", Arm::A, Clean::P("P"), &[]);
    let o = base_env(&user_of(CELL_USER)).root("cs_prepare_rust", 120);
    say(format!("INFO prepare-rust {}", short(&o.text, 200)));
    for row in ["BR1", "BR2", "BR3", "BR4", "BR5"] {
        run(&cs, row, Arm::A, Clean::S, &[("BENV", "lease")]);
    }
    cs.finish();
}

/// A snapshot of a pool user's state, with its name, uid, home and the pool gid replaced.
fn norm(src: &str, out: &str, user: &SlotUser) -> String {
    format!(
        "grep -v -e '^ds|' -e '^folders|' '{src}' | sed -e 's#{h}/{n}#<H>#g' -e 's/{n}/<U>/g' -e 's/{id}/<ID>/g' -e 's/{g}/<G>/g' > '{out}'",
        h = homes(),
        n = user.name,
        id = user.uid,
        g = UID0
    )
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1_base() {
    let cs = Cs::new("e1_base");
    let env0 = base_env(&user_of(CELL_USER));
    let reference = format!("{WORK}/b1.ref");
    let mut have_ref = false;
    for u in [1u32, 2, 3] {
        cs.cu.store(u, Ordering::SeqCst);
        let user = user_of(u);
        for cycle in 1..=3 {
            cs.cell("B1", Arm::A, Clean::S, &[]);
            let out = format!("{WORK}/b1.u{u}c{cycle}");
            env0.root(&norm(&format!("{WORK}/snap.last.after"), &out, &user), 60);
            if !have_ref {
                env0.root(&format!("cp '{out}' '{reference}'"), 10);
                have_ref = true;
                let n = env0.root(&format!("wc -l < '{reference}'"), 10);
                say(format!("BASEREF user={u} cycle={cycle} lines={}", short(&n.text, 20)));
                continue;
            }
            let d = env0.root(&format!("sdiff_ '{reference}' '{out}'"), 60);
            let lines: Vec<&str> = d.text.lines().filter(|l| !l.is_empty()).collect();
            say(format!("BASEDIFF user={u} cycle={cycle} vs-ref lines={}", lines.len()));
            for l in lines.iter().take(10) {
                say(format!("  | {}", short(l, 190)));
            }
        }
    }
    // Control: a used user whose clean skipped the kill must differ from the reference.
    cs.cu.store(2, Ordering::SeqCst);
    let user = user_of(2);
    cs.cell("B1k", Arm::A, Clean::P("P-nokill"), &[]);
    let out = format!("{WORK}/b1.ctl");
    env0.root(&norm(&format!("{WORK}/snap.last.after"), &out, &user), 60);
    let d = env0.root(&format!("sdiff_ '{reference}' '{out}'"), 60);
    let n = d.text.lines().filter(|l| !l.is_empty()).count();
    say(format!(
        "BASEDIFF-CTL a clean without the kill: lines={n} -> {}",
        if n > 0 { "the comparison sees a surviving process" } else { "BLIND" }
    ));
    for l in d.text.lines().take(6) {
        say(format!("  | {}", short(l, 190)));
    }
    // B0: users 4, 5, 6 were never used; the reference against each (the earlier run saw 42 lines against B0).
    for u in [4u32, 5, 6] {
        let user = user_of(u);
        let mut e = base_env(&user);
        e.row = "B0".into();
        e.marker = format!("kbfb0{u:06x}");
        e.mk = format!("{WORK}/mk.{}", e.marker);
        let raw = format!("{WORK}/snap.b0.raw.{u}");
        let out = format!("{WORK}/snap.b0.norm.{u}");
        e.root(&format!("touch \"$MK\"; snap > '{raw}' 2>&1"), 180);
        env0.root(&norm(&raw, &out, &user), 60);
        let d = env0.root(&format!("sdiff_ '{reference}' '{out}'"), 60);
        let n = d.text.lines().filter(|l| !l.is_empty()).count();
        say(format!("BASEDIFF-B0 user={u} (never used) vs B1 reference: lines={n}"));
    }
    cs.finish();
}

// ---------------------------------------------------------------- E1d: POSIX shm and sem allowed with `(with report)`

/// Does this OS accept `rule` after `(version 1)(allow default)`? Prints the answer either way.
fn sbx_accepts(rule: &str) -> bool {
    let mut c = Command::new("/usr/bin/sandbox-exec");
    c.args(["-p", &format!("(version 1)(allow default){rule}"), "/usr/bin/true"]);
    let o = run_timeout(c, 20);
    let ok = o.code == Some(0);
    say(format!("OPSYN {} {} :: {}", if ok { "accepted" } else { "REJECTED" }, rule, short(&o.text, 160)));
    ok
}

/// Builds the helper and the analysis script, finds the working `(with report)` syntax, and sets `IPC_RULES`.
fn e1d_prepare(sim: bool) {
    let env = base_env(&user_of(CELL_USER));
    fs::create_dir_all(WORK).expect("work dir");
    fs::write(format!("{WORK}/ipcc.c"), IPCC_C).expect("ipcc.c");
    fs::write(format!("{WORK}/e1d.py"), E1D_PY).expect("e1d.py");
    let o = env.root(
        "cd \"$W\" && clang -O1 -o ipcc ipcc.c 2>ipcc.log; echo clang-rc=$?; chmod 755 ipcc; cp ipcc ipcc_suid; chown 0:0 ipcc_suid; chmod 4755 ipcc_suid; \
         ls -l ipcc ipcc_suid | cut -c1-80; head -c 600 ipcc.log; chmod 644 e1d.py; mkdir -p e1d; chmod 755 e1d",
        120,
    );
    for l in o.text.lines() {
        say(format!("E1D-PREP {}", short(l, 200)));
    }
    if sim {
        let o = env.root(
            "cd \"$W\" && xcrun -sdk iphonesimulator clang -arch arm64 -mios-simulator-version-min=15.0 -O1 -o ipcc_sim ipcc.c 2>ipcc_sim.log; echo sim-clang-rc=$?; chmod 755 ipcc_sim; head -c 400 ipcc_sim.log",
            120,
        );
        for l in o.text.lines() {
            say(format!("E1D-PREP {}", short(l, 200)));
        }
    }
    let o = env.root("sw_vers | tr '\\n' ' '; echo; /usr/bin/python3 -I -c 'import sys;print(sys.version)'; dtrace -V | head -1; csrutil status", 30);
    for l in o.text.lines() {
        say(format!("E1D-PREP {}", short(l, 200)));
    }
    // The syntax, with the variants printed so a rejected form is on record.
    for r in [
        "(allow ipc-posix-shm* (with report))",
        "(allow ipc-posix-shm (with report))",
        "(allow ipc-posix-shm-write-create (with report))",
        "(allow ipc-posix-sem* (with report))",
        "(allow ipc-posix-sem (with report))",
        "(allow ipc-posix-sem-create (with report))",
        "(allow ipc-posix* (with report))",
        "(report ipc-posix-shm*)",
        "(deny ipc-posix-shm* (with no-report))",
    ] {
        sbx_accepts(r);
    }
    let pick = |cands: &[&str]| cands.iter().find(|c| sbx_accepts(c)).map(|c| (*c).to_owned());
    let shm = pick(&["(allow ipc-posix-shm* (with report))", "(allow ipc-posix-shm (with report))"]).expect("no shm report rule accepted");
    let sem = pick(&["(allow ipc-posix-sem* (with report))", "(allow ipc-posix-sem (with report))"]).expect("no sem report rule accepted");
    let rules = format!("\n{shm}\n{sem}");
    say(format!("E1D-RULES {}", short(&rules, 200)));
    let _ = IPC_RULES.set(rules);
}

/// `persist:info` for the Sandbox report subsystem, so info-level reports reach the on-disk log; or the default.
fn e1d_log_persist(on: bool) {
    let env = base_env(&user_of(CELL_USER));
    let cmd = if on {
        "log config --mode 'level:info,persist:info' --subsystem com.apple.sandbox.reporting 2>&1 | head -3"
    } else {
        "log config --reset --subsystem com.apple.sandbox.reporting 2>&1 | head -3"
    };
    let o = env.root(&format!("{cmd}; log config --status --subsystem com.apple.sandbox.reporting 2>&1 | head -c 200"), 60);
    for l in o.text.lines() {
        say(format!("E1D-LOGCFG on={on} {}", short(l, 200)));
    }
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1d_core() {
    let cs = Cs::new("e1d_core");
    e1d_prepare(false);
    // M1/M2/M3 under the report arm.
    run(&cs, "D1", Arm::IpcRep, Clean::None, &[]);
    run(&cs, "DALL", Arm::IpcRep, Clean::None, &[("BENV", "lease")]);
    run(&cs, "DF2", Arm::IpcRep, Clean::None, &[]);
    // Hiding attempts with the default log config, then the retention test (which turns persist:info on), then again.
    run(&cs, "D5", Arm::IpcRep, Clean::None, &[("BENV", "lease")]);
    run(&cs, "DR", Arm::IpcRep, Clean::None, &[]);
    run(&cs, "D5", Arm::IpcRep, Clean::None, &[("BENV", "lease")]);
    e1d_log_persist(false);
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1d_xcode() {
    let cs = Cs::new("e1d_xcode");
    e1d_prepare(true);
    // The same simulator-spawn cell with the default log config and with persist:info; DS keeps its names for DS2 on another uid.
    run(&cs, "DS", Arm::IpcRep, Clean::None, &[("BENV", "lease")]);
    e1d_log_persist(true);
    run(&cs, "DS", Arm::IpcRep, Clean::None, &[("BENV", "lease"), ("NOCLEAN", "1")]);
    cs.cu.store(1, Ordering::SeqCst);
    run(&cs, "DS2", Arm::IpcRep, Clean::None, &[("BENV", "lease"), ("CLEANLEFT", "1")]);
    cs.cu.store(CELL_USER, Ordering::SeqCst);
    run(&cs, "DX", Arm::IpcRep, Clean::None, &[("BENV", "lease")]);
    e1d_log_persist(false);
    cs.finish();
}
