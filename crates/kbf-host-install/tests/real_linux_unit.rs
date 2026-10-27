//! The helper's systemd units (`packaging/host-agent/linux/kbf-host-local*.service`) run for
//! real: the runner's privilege drop, the one the helper makes to run `curl` as another account,
//! made inside a transient unit that carries each unit's own `[Service]` settings.
//!
//! systemd (255, and its main branch when this was written) takes CAP_SETUID from a unit's
//! process when the unit names an account (`User=`, even `User=root`) and has a system call
//! filter (which `RestrictRealtime=`, `LockPersonality=` and `ProtectKernelModules=` each add);
//! with `NoNewPrivileges=` the program it starts does not get it back. Such a helper cannot drop
//! to another account, and every release fetch fails with `Operation not permitted`. No unit
//! test sees that: it needs systemd and root. So the drop is made where systemd puts the helper.
//!
//! These tests need Linux, systemd and root, so they are `#[ignore]`d and run in the
//! `host-install-linux` workflow on a GitHub-hosted Ubuntu runner through `cargo xtask sudo-test
//! -p kbf-host-install --test real_linux_unit`. Every raw output goes to `target/real-linux` (the
//! workflow uploads it). Each test captures first and asserts after.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_host_install::mac_tools::{Cmd, Exec, Identity, Runner, SystemRunner};
use kbf_testkit::dir::TestDir;

const LIMIT: Duration = Duration::from_secs(60);

/// The helper's units.
const UNITS: [&str; 2] = ["kbf-host-local.service", "kbf-host-local-frontend.service"];

/// The `[Service]` keys that are not how the process is confined: what it starts, when systemd
/// starts it again, and the directories systemd makes for it. Every other key of the unit is
/// passed to the transient unit, so a setting added to the unit later is tested too.
const NOT_CONFINEMENT: &[&str] = &[
    "Type",
    "ExecStart",
    "Restart",
    "RestartSec",
    "RuntimeDirectory",
    "RuntimeDirectoryMode",
    "StateDirectory",
    "StateDirectoryMode",
];

const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";

/// Where the raw outputs go.
fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/real-linux")
}

/// Write the raw output of a command: its stdout, its stderr (when it has any) and its
/// exit code.
fn capture(name: &str, exec: &Exec) {
    let dir = out_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{name}.stdout")), &exec.stdout).unwrap();
    if !exec.stderr.is_empty() {
        fs::write(dir.join(format!("{name}.stderr")), &exec.stderr).unwrap();
    }
    let code = exec.code.map_or("signal".to_owned(), |c| c.to_string());
    fs::write(dir.join(format!("{name}.code")), format!("{code}\n")).unwrap();
}

fn run(cmd: &Cmd) -> Exec {
    SystemRunner
        .run(cmd)
        .unwrap_or_else(|e| panic!("{} did not run: {e}", cmd.program))
}

/// The `key=value` lines of a unit's `[Service]` section that confine its process.
fn confinement(unit: &str) -> Vec<String> {
    let mut in_service = false;
    let mut found = Vec::new();
    for line in unit.lines().map(str::trim) {
        if let Some(section) = line.strip_prefix('[') {
            in_service = section == "Service]";
            continue;
        }
        if !in_service || line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        let (key, _) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("not a key=value line: {line}"));
        if !NOT_CONFINEMENT.contains(&key.trim()) {
            found.push(line.to_owned());
        }
    }
    found
}

/// The uid and gid of `nobody`, from `/etc/passwd`.
fn nobody() -> Identity {
    let passwd = fs::read_to_string("/etc/passwd").unwrap();
    let fields: Vec<&str> = passwd
        .lines()
        .find(|l| l.starts_with("nobody:"))
        .expect("an account nobody")
        .split(':')
        .collect();
    Identity {
        uid: fields[2].parse().unwrap(),
        gid: fields[3].parse().unwrap(),
    }
}

/// The value after `prefix` on the first line of `text` that starts with it.
fn line_after<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines().find_map(|l| l.strip_prefix(prefix))
}

/// Not a test of its own: the process the drop starts, which prints its ids.
#[test]
#[ignore = "run by drop_inside_the_unit, as another user"]
fn print_my_ids() {
    println!(
        "ids: {} {} {} {}",
        rustix::process::getuid().as_raw(),
        rustix::process::geteuid().as_raw(),
        rustix::process::getgid().as_raw(),
        rustix::process::getegid().as_raw()
    );
}

/// Not a test of its own: what the transient unit runs. It prints its own capability sets, then
/// makes the runner's drop to `nobody` (the helper's drop for `curl`) and prints what came of
/// it: the dropped process's output, or the error that stopped it. The test that started it
/// reads those lines.
#[test]
#[ignore = "run by the_helper_units_let_the_runner_drop_to_another_account, in a transient unit"]
fn drop_inside_the_unit() {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    for key in ["CapPrm:", "CapEff:", "NoNewPrivs:", "Seccomp:"] {
        println!("{key} {}", line_after(&status, key).unwrap_or("?").trim());
    }
    let probe = Cmd {
        program: std::env::current_exe().unwrap().display().to_string(),
        args: ["--ignored", "--exact", "print_my_ids", "--nocapture"]
            .map(str::to_owned)
            .to_vec(),
        timeout: Some(LIMIT),
        run_as: Some(nobody()),
        ..Cmd::default()
    };
    match SystemRunner.run(&probe) {
        Ok(exec) => {
            let code = exec.code.map_or("signal".to_owned(), |c| c.to_string());
            println!("drop: ran, code {code}");
            print!("{}", exec.stdout_text());
            eprint!("{}", exec.stderr_text());
        }
        Err(e) => println!("drop: refused: {e}"),
    }
}

/// Each helper unit's own confinement, applied by systemd to a transient unit, leaves the
/// runner able to drop to another account: the dropped process runs as `nobody`, real and
/// effective. With `User=root` in a unit, systemd takes CAP_SETUID and the drop is refused
/// (`Operation not permitted`), which fails this test.
#[test]
#[ignore = "needs Linux, systemd and root: cargo xtask sudo-test"]
fn the_helper_units_let_the_runner_drop_to_another_account() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run through `cargo xtask sudo-test`"
    );
    let want = nobody();
    assert_ne!(want.uid, 0);
    // The test binary again, where the unit's process and `nobody` can reach it: not under
    // /home (ProtectHome=) or /tmp and /var/tmp (PrivateTmp=).
    let dir = TestDir::under("/var/lib", "kbf-real-linux-unit");
    fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
    let copy = dir.join("probe");
    fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).unwrap();

    let packaging = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/host-agent/linux");
    for (n, unit) in UNITS.iter().enumerate() {
        let text = fs::read_to_string(packaging.join(unit)).unwrap();
        let lines = confinement(&text);
        assert!(
            lines.iter().any(|l| l == "NoNewPrivileges=yes"),
            "{unit}: the settings were read: {lines:?}"
        );
        let mut args: Vec<String> = [
            "--quiet",
            "--wait",
            "--pipe",
            "--collect",
            &format!("--unit=kbf-real-linux-unit-{}-{n}", std::process::id()),
        ]
        .map(str::to_owned)
        .to_vec();
        for line in &lines {
            args.push("-p".to_owned());
            args.push(line.clone());
        }
        args.push(copy.display().to_string());
        args.extend(
            [
                "--ignored",
                "--exact",
                "drop_inside_the_unit",
                "--nocapture",
            ]
            .map(str::to_owned),
        );
        let exec = run(&Cmd {
            program: SYSTEMD_RUN.to_owned(),
            args,
            timeout: Some(LIMIT),
            ..Cmd::default()
        });
        let name = unit.trim_end_matches(".service");
        capture(name, &exec);
        let out = exec.stdout_text();
        println!("{unit} with {lines:?}:\n{out}");
        assert!(exec.success(), "{unit}: {}", exec.stderr_text());
        let drop = line_after(&out, "drop: ").unwrap_or_else(|| panic!("{unit}: no drop line"));
        assert_eq!(drop, "ran, code 0", "{unit}: the drop");
        let ids: Vec<u32> = line_after(&out, "ids: ")
            .unwrap_or_else(|| panic!("{unit}: no ids line"))
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(ids, [want.uid, want.uid, want.gid, want.gid], "{unit}");
    }
}
