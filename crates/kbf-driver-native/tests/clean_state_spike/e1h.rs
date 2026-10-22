//! E1h: the POSIX IPC hermeticity follow-up (spike only; never for main). A pinned
//! python-build-standalone CPython runs multiprocessing in leases, some killed mid-run, under the
//! SIGTERM-first clean or the SIGKILL-only control; simulator leases end with `simctl shutdown`,
//! `simctl erase` and the clean; `launchctl reboot userspace` is looked at last.
//!
//! Lines to grep: `PBS`, `HREC`, `HPROCS`, `HCLEAN`, `HPROBE`, `PYLIFE`, `PYEV`, `HLEFT`, `HSIM`,
//! `HLEASE`, `HSUM`, `HPLATEAU`, `HSTAGES`, `HPAT`, `HFINAL`, `HRB`, plus the harness's `CELL`, `MID`.

use super::*;

const E1H_PY: &str = include_str!("e1h.py");
const PBS_URL: &str = "https://github.com/astral-sh/python-build-standalone/releases/download/20261003/cpython-3.13.16%2B20261003-aarch64-apple-darwin-install_only.tar.gz";
const PBS_SHA: &str = "d8975d7df4f08f7b1c7aafcdfacbddcec3d366415f2c1a72b2466b6850815933";
/// D: ran to completion; T: killed by the clean's SIGTERM-first; KP: parent SIGKILLed mid-run, then the clean; K: SIGKILL of every process (control).
const MODES: [&str; 4] = ["D", "T", "KP", "K"];
/// Whether the reboot job really runs `launchctl reboot userspace` (it may take the runner job with it).
const REBOOT_GO: bool = false;

fn root(body: &str, secs: u64) -> Out {
    let o = base_env(&user_of(CELL_USER)).root(body, secs);
    for l in o.text.lines().filter(|l| !l.is_empty()) {
        say(short(l, 400));
    }
    if o.timed_out {
        say(format!("E1H-TIMEOUT after {secs}s :: {}", short(body, 80)));
    }
    o
}

fn e1h_prepare() {
    fs::create_dir_all(WORK).expect("work dir");
    fs::write(format!("{WORK}/ipcc.c"), IPCC_C).expect("ipcc.c");
    fs::write(format!("{WORK}/e1h.py"), E1H_PY).expect("e1h.py");
    // No ipc-posix rule at all: the E1d profile minus its `(with report)` rules.
    let _ = IPC_RULES.set(String::new());
    root(
        "cd \"$W\" && clang -O1 -o ipcc ipcc.c 2>ipcc.log; echo clang-rc=$?; chmod 755 ipcc; chmod 644 e1h.py; mkdir -p e1h; chmod 755 e1h; \
         sw_vers | tr '\\n' ' '; echo; dtrace -V | head -1; csrutil status; df -h / | tail -1; sysctl kern.posix.sem.max kern.posix.shm.max 2>&1 | tr '\\n' ' '",
        180,
    );
    let o = root(&format!("h_fetch_pbs '{PBS_URL}' '{PBS_SHA}'"), 600);
    assert!(o.text.contains("PBS unpacked"), "the pinned interpreter was not unpacked: {}", short(&o.text, 300));
}

fn lease(cs: &Cs, row: &str, arm: Arm, tag: &str, i: u32, uidx: u32, mode: &str) {
    cs.cu.store(uidx, Ordering::SeqCst);
    let n = i.to_string();
    cs.cell(row, arm, Clean::None, &[("HMODE", mode), ("HN", &n), ("HARM", tag), ("BENV", "lease")]);
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1h_py() {
    let cs = Cs::new("e1h_py");
    e1h_prepare();
    root("h_rec_start", 120);
    // 52 leases over pool uids 61000..61002; every fourth lease of each uid cycles the four modes; 12 of them are unsandboxed (arm R).
    for i in 0..52u32 {
        let (arm, tag) = if i % 13 >= 10 { (Arm::R, "R") } else { (Arm::IpcRep, "D") };
        lease(&cs, "HP", arm, tag, i, i % 3, MODES[(i % 4) as usize]);
    }
    root("h_final", 300);
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1h_sim() {
    let cs = Cs::new("e1h_sim");
    e1h_prepare();
    root("h_rec_start", 120);
    // 15 sandboxed simulator leases (mode S: shutdown, erase, clean; mode C for leases 6..9: clean only), then 3 unsandboxed.
    for i in 0..18u32 {
        let (arm, tag) = if i >= 15 { (Arm::R, "R") } else { (Arm::IpcRep, "D") };
        let mode = if (6..9).contains(&i) { "C" } else { "S" };
        lease(&cs, "HS", arm, tag, i, i % 3, mode);
    }
    root("h_final", 300);
    cs.finish();
}

#[test]
#[ignore = "needs root and makes real users"]
fn e1h_reboot() {
    let cs = Cs::new("e1h_reboot");
    e1h_prepare();
    root("mkdir -p \"$EH\"; chmod 755 \"$EH\"; h_rb_names; h_rb_probe before; h_rb_survey", 300);
    if REBOOT_GO {
        root("h_rb_go", 400);
        root("h_rb_probe after", 120);
    } else {
        say("HRB skipped: REBOOT_GO is false in this build");
    }
    cs.finish();
}
