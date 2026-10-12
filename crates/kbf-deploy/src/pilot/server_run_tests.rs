//! The wait for kbf-server's start line, an apply end to end, and the rollback.

use super::*;

fn ready(offset: usize) -> Ready {
    Ready {
        log: p("/srv/log/out.log"),
        err_log: p("/srv/log/err.log"),
        offset,
    }
}

fn waited(exec: &FakeExec, r: &Ready) -> (Result<(), PilotError>, String, usize) {
    let pauses = Cell::new(0);
    let mut out = Vec::new();
    let got = wait_ready(exec, r, &|| pauses.set(pauses.get() + 1), &mut out);
    (got, String::from_utf8(out).expect("utf-8"), pauses.get())
}

/// Catches a wait that takes an old start line (one from before the restart) as
/// this start's, that misses the new one, that gives up early or waits forever, and
/// one that reads a log shorter than before (rotated) from a stale offset.
#[test]
fn the_wait_takes_only_a_start_line_written_after_the_restart() {
    let mut exec = FakeExec::new(UID);
    let old = "kbf-server 0.0.9 reapi=a worker=b\n";
    let new = "kbf-server 0.1.0 reapi=192.0.2.10:8980 worker=192.0.2.10:8981\n";
    put(
        &mut exec,
        "/srv/log/out.log",
        0o600,
        format!("{old}{new}").as_bytes(),
    );
    let (got, out, pauses) = waited(&exec, &ready(old.len()));
    assert!(got.is_ok());
    assert_eq!(out, format!("READY: {}", new));
    assert_eq!(pauses, 0);
    put(&mut exec, "/srv/log/out.log", 0o600, new.as_bytes());
    let (got, out, _) = waited(&exec, &ready(1000));
    assert!(got.is_ok(), "a rotated log is read from its start: {out}");
    put(&mut exec, "/srv/log/out.log", 0o600, old.as_bytes());
    let (got, _, pauses) = waited(&exec, &ready(old.len()));
    assert_eq!(
        got.err().map(|e| e.to_string()).as_deref(),
        Some("kbf-server did not report ready within 30 s")
    );
    assert_eq!(pauses, 30);
}

/// Catches a failed start reported without the error log's last lines (the operator
/// has nothing to go on), more than its last 20, or a missing error log made fatal
/// before the failure is reported.
#[test]
fn no_start_line_prints_the_error_logs_last_20_lines() {
    let mut exec = FakeExec::new(UID);
    let (got, out, _) = waited(&exec, &ready(0));
    assert!(matches!(got, Err(PilotError::NotReady { secs: 30 })));
    assert_eq!(
        out,
        "NOT READY after 30 s. Last lines of /srv/log/err.log:\n"
    );
    let err: String = (1..=25).map(|i| format!("e{i}\n")).collect();
    put(&mut exec, "/srv/log/err.log", 0o600, err.as_bytes());
    let (_, out, _) = waited(&exec, &ready(0));
    let tail: Vec<&str> = out.lines().skip(1).collect();
    let want: Vec<String> = (6..=25).map(|i| format!("    e{i}")).collect();
    assert_eq!(tail, want);
}

/// Catches `run` waiting after a dry run (nothing restarted), not waiting after an
/// apply that restarted, and the restart's start line not found. The host here is a
/// scratch root on disk whose `systemctl restart` writes the start line, as
/// kbf-server does; the second run, with nothing changed, neither restarts nor waits.
#[test]
fn an_apply_restarts_and_waits_for_the_start_line() {
    let mut exec = ScratchHost::new("pilot_server_apply");
    exec.euid = rustix::process::geteuid().as_raw();
    let root = exec.root.clone();
    let write = |path: &str, mode: u32, bytes: &[u8]| {
        let full = root.join(path.trim_start_matches('/'));
        fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
        fs::write(&full, bytes).expect("write");
        fs::set_permissions(&full, fs::Permissions::from_mode(mode)).expect("chmod");
    };
    let (ca, server, key) = pki("kbf-pilot-server");
    write(&format!("{PKI}/ca.pem"), 0o644, ca.as_bytes());
    write(&format!("{PKI}/server.pem"), 0o644, server.as_bytes());
    write(&format!("{PKI}/server.key"), 0o600, key.as_bytes());
    write(S3, 0o600, b"AWS_ACCESS_KEY_ID=a\nAWS_SECRET_ACCESS_KEY=b\n");
    write(BIN, 0o755, &elf(62));
    write("/var/lib/systemd/linger/op", 0o644, b"");
    let w = world();
    let tools = answer(&w);
    let log = root.join(format!("{STATE}/log/kbf-server.out.log").trim_start_matches('/'));
    exec.answer = Box::new(move |cmd| {
        if cmd.args.get(1).is_some_and(|a| a == "restart") {
            fs::write(
                &log,
                "kbf-server 0.1.0 reapi=192.0.2.10:8980 worker=192.0.2.10:8981\n",
            )
            .expect("the start line");
        }
        tools(cmd)
    });
    let pauses = Cell::new(0);
    let pause = || pauses.set(pauses.get() + 1);
    let mut out = Vec::new();
    run(&mut exec, &install(), X86, Mode::DryRun, &mut out, &pause).expect("a dry run");
    assert!(
        String::from_utf8_lossy(&out).ends_with("dry run: nothing changed. Re-run with --apply.\n")
    );
    assert!(w.borrow().calls.is_empty());
    let mut out = Vec::new();
    run(&mut exec, &install(), X86, Mode::Apply, &mut out, &pause).expect("an apply");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(
        out.ends_with("READY: kbf-server 0.1.0 reapi=192.0.2.10:8980 worker=192.0.2.10:8981\n"),
        "{out}"
    );
    let installed = fs::read(root.join(format!("{STATE}/bin/kbf-server").trim_start_matches('/')))
        .expect("installed");
    assert_eq!(installed, elf(62));
    let mode = |path: &str| {
        fs::metadata(root.join(path.trim_start_matches('/')))
            .expect("there")
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode(STATE), 0o700);
    assert_eq!(mode(&format!("{UNITS}/{SERVICE}")), 0o644);
    let mut out = Vec::new();
    run(&mut exec, &install(), X86, Mode::Apply, &mut out, &pause).expect("a re-run");
    assert!(String::from_utf8_lossy(&out).ends_with("nothing to do\n"));
    assert_eq!(w.borrow().calls.len(), 3);
    assert_eq!(pauses.get(), 0);
}

fn uninstall() -> Uninstall {
    Uninstall {
        state: None,
        home: home(),
    }
}

fn removal(exec: &FakeExec) -> (Plan, String) {
    let mut out = Vec::new();
    let plan = plan_uninstall(exec, &uninstall(), X86, "1700000000", &mut out).expect("plans");
    (plan, String::from_utf8(out).expect("utf-8"))
}

/// Catches a rollback that leaves the service running, deletes the units instead of
/// keeping them, keeps them where systemd still reads them, or skips the reload; and
/// one that is not idempotent (a second run plans the same moves again).
#[test]
fn uninstall_stops_the_service_and_moves_both_units_aside() {
    let (mut exec, w) = host();
    let first = planned(&exec, &install(), Mode::Apply).0.expect("plans");
    execute(
        &first.plan,
        RunAs::User,
        Mode::Apply,
        &mut exec,
        &mut Vec::new(),
    )
    .expect("installs");
    w.borrow_mut().installed = true;
    let (plan, out) = removal(&exec);
    let removed = format!("{STATE}/removed");
    let env = "(env XDG_RUNTIME_DIR)";
    let service_len = match exec.entries.get(&p(&format!("{UNITS}/{SERVICE}"))) {
        Some(Entry::File { bytes, .. }) => bytes.len(),
        _ => panic!("installed"),
    };
    let slice_len = match exec.entries.get(&p(&format!("{UNITS}/{SLICE}"))) {
        Some(Entry::File { bytes, .. }) => bytes.len(),
        _ => panic!("installed"),
    };
    assert_eq!(
        steps(&plan),
        [
            format!("run {SYSTEMCTL} --user disable --now {SERVICE} {env}"),
            format!("mkdir {removed} (0700)"),
            format!("write {removed}/{SERVICE}.1700000000 (0644, {service_len} bytes)"),
            format!("remove {UNITS}/{SERVICE}"),
            format!("write {removed}/{SLICE}.1700000000 (0644, {slice_len} bytes)"),
            format!("remove {UNITS}/{SLICE}"),
            format!("run {SYSTEMCTL} --user daemon-reload {env}"),
        ]
    );
    assert!(
        out.contains(&format!(
            "kept: {STATE} (the binary and the logs) and the bucket"
        )),
        "{out}"
    );
    let mut out = Vec::new();
    run_uninstall(
        &mut exec,
        &uninstall(),
        X86,
        "1700000000",
        Mode::Apply,
        &mut out,
    )
    .expect("applies");
    let (again, out) = removal(&exec);
    assert!(again.is_empty(), "{:?}", steps(&again));
    assert!(
        out.contains("kbf-pilot-server.service is not installed"),
        "{out}"
    );
}

/// Catches a rollback of only one unit moving the other, or a missing state
/// directory's parents not made.
#[test]
fn uninstall_of_one_unit_makes_the_removed_directory_and_its_parents() {
    let (mut exec, _) = host();
    put(&mut exec, &format!("{UNITS}/{SLICE}"), 0o644, b"[Slice]\n");
    let (plan, _) = removal(&exec);
    assert_eq!(
        steps(&plan),
        [
            "mkdir /srv/op/.local (0755)".to_owned(),
            "mkdir /srv/op/.local/share (0755)".to_owned(),
            format!("mkdir {STATE} (0755)"),
            format!("mkdir {STATE}/removed (0700)"),
            format!("write {STATE}/removed/{SLICE}.1700000000 (0644, 8 bytes)"),
            format!("remove {UNITS}/{SLICE}"),
            format!("run {SYSTEMCTL} --user daemon-reload (env XDG_RUNTIME_DIR)"),
        ]
    );
    exec.euid = 0;
    let e = plan_uninstall(&exec, &uninstall(), X86, "1", &mut Vec::new())
        .err()
        .map(|e| e.to_string());
    assert_eq!(e, Some(PilotError::IsRoot.to_string()));
}
