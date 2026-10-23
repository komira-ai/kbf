//! SPIKE (never merges): what a pool user under the slot profile reaches of the GPU (Metal,
//! MLX), of a UI test run (XCUITest) and of Launch Services (a signed app), on a hosted
//! macOS runner. One `ARM <label> exit=N` line per arm, its output as `ARMOUT <label> | ...`,
//! the sandbox deny lines of its window as `DENY <label> | ...`. Asserts only that the
//! harness ran. The probes are `tools/spike/macos-apps-gpu`, built into `$KBF_SPIKE_BIN`
//! by the workflow before this test.

#![cfg(target_os = "macos")]

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use kbf_daemon::uid::LAUNCHCTL;
use kbf_driver_native::network::{Isolation, SANDBOX_EXEC, SLOT_PROFILE, slot_write_rules};
use kbf_driver_native::slots::{AS_USER_ARG, SlotUser};
use support::pool::{Scratch, Users, home_of};

const ENV_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin";

/// The slot profile without any of the three pending denies.
fn open_profile() -> String {
    "(version 1)\n(allow default)\n(deny file-write*)\n\
(allow file-write* (subpath (param \"KBF_LEASE\")) (subpath \"/dev\"))\n"
        .to_owned()
}

/// The open profile plus one deny.
fn plus(deny: &str) -> String {
    open_profile().replace("(deny file-write*)", &format!("{deny}\n(deny file-write*)"))
}

struct Ctx {
    user: SlotUser,
    as_user: String,
    rules: String,
    bin: String,
    logs: PathBuf,
    scratch: PathBuf,
}

fn local_time() -> String {
    let out = Command::new("/bin/date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .expect("date");
    String::from_utf8(out.stdout).expect("utf8").trim().to_owned()
}

fn as_user_cmd(ctx: &Ctx, argv: &[String]) -> Command {
    let (uid, gid) = (ctx.user.uid.to_string(), ctx.user.gid.to_string());
    let mut c = Command::new(LAUNCHCTL);
    c.args(["asuser", &uid, &ctx.as_user, AS_USER_ARG, &uid, &gid, "--"])
        .args(argv)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null());
    c
}

fn print_lines(tag: &str, label: &str, text: &str, keep: impl Fn(&str) -> bool, cap: usize) {
    let mut n = 0;
    for line in text.lines().filter(|l| keep(l)) {
        n += 1;
        if n <= cap {
            let l: String = line.chars().take(400).collect();
            println!("{tag} {label} | {l}");
        }
    }
    if n > cap {
        println!("{tag} {label} | ... {} more lines", n - cap);
    }
}

fn log_window(ctx: &Ctx, label: &str, since: &str, predicate: &str, tag: &str, needle: &str) {
    let file = ctx.logs.join(format!("{label}-{tag}.log"));
    let out = std::fs::File::create(&file).expect("log file");
    let _ = Command::new("/usr/bin/log")
        .args(["show", "--start", since, "--style", "compact", "--predicate", predicate])
        .stdout(out)
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status();
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    print_lines(tag, label, &text, |l| l.contains(needle), 60);
}

/// One arm: `script` (a probe script) with `args` (`{lease}` is the arm's lease directory),
/// as the pool user, under `profile` or, with None, with no sandbox.
fn run_arm(ctx: &Ctx, label: &str, profile: Option<&str>, script: &str, args: &[&str], secs: u64, ui: bool) {
    let lease = ctx.scratch.join(format!("lease-{label}"));
    let _ = std::fs::create_dir_all(lease.join("tmp"));
    for p in [&lease, &lease.join("tmp")] {
        std::os::unix::fs::chown(p, Some(ctx.user.uid), Some(ctx.user.gid)).expect("chown");
    }
    let home = home_of(&ctx.user);
    let lease_s = lease.to_string_lossy().into_owned();
    let mut inner: Vec<String> = vec![
        "-e".into(),
        "alarm shift; exec @ARGV".into(),
        secs.to_string(),
        "/usr/bin/env".into(),
        format!("HOME={}", home.display()),
        format!("TMPDIR={lease_s}/tmp"),
        format!("PATH={ENV_PATH}"),
        format!("USER={}", ctx.user.name),
        format!("LOGNAME={}", ctx.user.name),
        "/bin/bash".into(),
        format!("{}/{script}", ctx.bin),
        ctx.bin.clone(),
    ];
    inner.extend(args.iter().map(|a| a.replace("{lease}", &lease_s).replace("{label}", label)));
    let (prog, argv) = match profile {
        Some(p) => Isolation::detect_at(Path::new(SANDBOX_EXEC)).wrap_in(
            p,
            &lease,
            &ctx.rules,
            PathBuf::from("/usr/bin/perl"),
            &inner,
        ),
        None => (PathBuf::from("/usr/bin/perl"), inner),
    };
    let mut full = vec![prog.to_string_lossy().into_owned()];
    full.extend(argv);
    let since = local_time();
    let logfile = ctx.logs.join(format!("{label}.out"));
    let f = std::fs::File::create(&logfile).expect("out");
    let status = as_user_cmd(ctx, &full)
        .stdout(f.try_clone().expect("clone"))
        .stderr(f)
        .status();
    println!("ARM {label} sandbox={} exit={:?}", profile.is_some(), status.map(|s| s.code()));
    let text = std::fs::read_to_string(&logfile).unwrap_or_default();
    print_lines("ARMOUT", label, &text, |_| true, 150);
    // Whatever the arm left running ends here, by uid.
    let _ = Command::new("/usr/bin/pkill").args(["-9", "-U", &ctx.user.uid.to_string()]).status();
    std::thread::sleep(std::time::Duration::from_secs(4));
    if profile.is_some() {
        log_window(
            ctx,
            label,
            &since,
            "subsystem == \"com.apple.sandbox.reporting\" OR eventMessage CONTAINS \"deny(\"",
            "DENY",
            "deny",
        );
    }
    if ui {
        log_window(ctx, label, &since, "subsystem == \"com.apple.TCC\"", "TCC", "");
    }
}

fn automation(verb: &str) {
    let out = Command::new("/usr/bin/perl")
        .args(["-e", "alarm shift; exec @ARGV", "60", "/usr/bin/automationmodetool"])
        .args(verb.split_whitespace().filter(|v| !v.is_empty()))
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) => println!(
            "AUTOMATIONMODETOOL [{verb}] exit={:?} {}",
            o.status.code(),
            String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).replace('\n', " ")
        ),
        Err(e) => println!("AUTOMATIONMODETOOL [{verb}] cannot run: {e}"),
    }
}

#[ignore = "needs root, real users and the probes: see spike-slot-reach.yml job apps-gpu"]
#[test]
fn gpu_ui_and_app_arms_are_recorded() {
    support::pool::assert_root("apps_gpu_spike");
    let scratch = Scratch::new("appsgpu");
    let users = Users::new("appsgpu", 1);
    let user = users.0[0].clone();
    let bin = std::env::var("KBF_SPIKE_BIN").unwrap_or_else(|_| "/private/tmp/kbf-spike-bin".into());
    let logs = PathBuf::from(
        std::env::var("KBF_SPIKE_LOGS").unwrap_or_else(|_| "/private/tmp/kbf-spike-logs".into()),
    );
    std::fs::create_dir_all(&logs).expect("logs");
    let sw = Command::new("/usr/bin/sw_vers").output().expect("sw_vers");
    println!("SW_VERS {}", String::from_utf8_lossy(&sw.stdout).replace('\n', " "));
    let mut ctx = Ctx {
        user: user.clone(),
        as_user: scratch.as_user_binary(),
        rules: String::new(),
        bin,
        logs,
        scratch: scratch.0.clone(),
    };
    // The user's /var/folders entry, as the driver resolves it.
    let out = as_user_cmd(&ctx, &["/usr/bin/getconf".into(), "DARWIN_USER_DIR".into()])
        .output()
        .expect("getconf");
    let said = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let folders = std::fs::canonicalize(Path::new(&said).parent().expect("parent")).expect("folders");
    let home = std::fs::canonicalize(home_of(&user)).expect("home");
    ctx.rules = slot_write_rules(&home, &folders).expect("rules");
    println!("SETUP user={} uid={} home={} folders={}", user.name, user.uid, home.display(), folders.display());

    let denies = ["(deny mach-register)", "(deny lsopen)", "(deny file-mount)"];
    let gpu = |label: &str, profile: Option<&str>| run_arm(&ctx, label, profile, "probe_gpu.sh", &[], 400, false);
    let app = |label: &str, profile: Option<&str>| {
        run_arm(&ctx, label, profile, "probe_app.sh", &["{lease}", "{label}"], 200, false)
    };
    // A. GPU (Metal and MLX).
    gpu("gpu-control", None);
    gpu("gpu-open", Some(&open_profile()));
    for d in denies {
        gpu(&format!("gpu-plus-{}", d.trim_matches(['(', ')']).replace("deny ", "")), Some(&plus(d)));
    }
    gpu("gpu-slot", Some(SLOT_PROFILE));
    // C. Signed app through Launch Services.
    app("app-control", None);
    app("app-open", Some(&open_profile()));
    for d in denies {
        app(&format!("app-plus-{}", d.trim_matches(['(', ')']).replace("deny ", "")), Some(&plus(d)));
    }
    app("app-slot", Some(SLOT_PROFILE));
    // B. UI tests, before and after the automation setting.
    automation("");
    automation("disable-automationmode-without-authentication");
    automation("");
    let ui = |label: &str, profile: Option<&str>| {
        run_arm(&ctx, label, profile, "probe_ui.sh", &["{lease}/ui"], 800, true)
    };
    ui("ui-requires-control", None);
    ui("ui-requires-slot", Some(SLOT_PROFILE));
    automation("enable-automationmode-without-authentication");
    automation("");
    ui("ui-noauth-control", None);
    ui("ui-noauth-slot", Some(SLOT_PROFILE));
    println!("DONE");
}
