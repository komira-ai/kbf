use std::ffi::{OsStr, OsString};
use std::fs;

use super::state::Locked;
use super::*;
use kbf_host::testkit::FakeExec;
use kbf_installer::testkit::{A, B, C, full_state, host_root};

/// `kbf-deploy host status <args>` with `--root dir`: exit, stdout, stderr.
fn status_cli(dir: &std::path::Path, args: &[&str]) -> (ExitCode, String, String) {
    let argv: Vec<OsString> = ["kbf-deploy", "host", "status", "--root"]
        .into_iter()
        .map(OsString::from)
        .chain([dir.as_os_str().to_owned()])
        .chain(args.iter().map(OsString::from))
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut exec = FakeExec::new(0);
    let code = crate::cli::run(argv, &mut exec, &|_| None, &mut out, &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (code, text(out), text(err))
}

/// Catches a status that leaves out a field, prints a commit or deadline from the wrong
/// field, says the link matches when it does not (or exits 0 then), or reads a host
/// with no state as an error. The lines are what an operator reads before a swap and
/// the JSON what a script parses, so both are pinned whole.
#[test]
fn status_prints_the_state_and_the_link_verdict() {
    let (root, dir) = host_root("deploy-host-status-cli");
    let (code, out, err) = status_cli(&dir, &[]);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert_eq!(
        out,
        "state: /var/lib/kbf-installer/state.json (none yet)\n\
         current: none\n\
         previous: none\n\
         staged: none\n\
         pending confirm: none\n\
         floor: epoch 0, order 0, generation 0\n\
         current link: missing (matches the state)\n\
         releases kept: 0\n"
    );
    let (code, out, _) = status_cli(&dir, &["--json"]);
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(
        out,
        "{\"current_link\":{\"kind\":\"missing\",\"matches_state\":true,\"target\":null},\
         \"exists\":false,\"releases\":[],\"schema\":2,\"state\":{\"current\":null,\
                  \"floor\":{\"epoch\":0,\"generation\":0,\"order_id\":0},\
         \"last_acked_order_id\":0,\"pending_confirm\":null,\"previous\":null,\
         \"serving_instance\":null,\"staged\":null,\"unacked\":[]},\
         \"state_file\":\"/var/lib/kbf-installer/state.json\"}\n"
    );

    Locked::acquire(&root)
        .expect("lock")
        .write(&full_state())
        .expect("write");
    let link = dir.join("var/lib/kbf-installer/releases/current");
    fs::create_dir_all(link.parent().expect("a directory")).expect("releases");
    std::os::unix::fs::symlink(A, &link).expect("link");
    let (code, out, _) = status_cli(&dir, &[]);
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(
        out,
        format!(
            "state: /var/lib/kbf-installer/state.json\n\
             current: {A}\n\
             previous: {B}\n\
             staged: {C}\n\
             pending confirm: {A} by 1800000000 (unix seconds), else back to {B}\n\
             floor: epoch 3, order 9, generation 7\n\
             current link: {A} (matches the state)\n\
             releases kept: 0\n"
        )
    );
    let (code, out, _) = status_cli(&dir, &["--json"]);
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(
        out,
        format!(
            "{{\"current_link\":{{\"kind\":\"link\",\"matches_state\":true,\
             \"target\":\"{A}\"}},\"exists\":true,\"releases\":[],\"schema\":2,\"state\":{{\
             \"current\":\"{A}\",\"floor\":{{\"epoch\":3,\"generation\":7,\"order_id\":9}},\
             \"last_acked_order_id\":9,\"pending_confirm\":{{\"activation\":\"3-9\",\
             \"commit\":\"{A}\",\"deadline_mono_s\":86400,\"deadline_wall\":1800000000,\
             \"previous\":\"{B}\",\"starts_left\":2}},\"previous\":\"{B}\",\
             \"serving_instance\":\"a\",\"staged\":\"{C}\",\"unacked\":[{{\"epoch\":3,\
             \"first_seen\":1700000000,\"order_id\":10,\"record\":\"{{}}\"}}]}},\"state_file\":\"/var/lib/kbf-installer/state.json\"}}\n"
        )
    );

    fs::remove_file(&link).expect("remove");
    std::os::unix::fs::symlink(B, &link).expect("link");
    let (code, out, _) = status_cli(&dir, &[]);
    assert_eq!(code, ExitCode::FAILURE);
    assert!(
        out.ends_with(&format!(
            "current link: {B} (the state says {A})\nreleases kept: 0\n"
        )),
        "{out}"
    );

    let mut first = full_state();
    first.current = None;
    first.pending_confirm.as_mut().expect("pending").previous = None;
    Locked::acquire(&root)
        .expect("lock")
        .write(&first)
        .expect("write");
    fs::remove_file(&link).expect("remove");
    fs::create_dir(&link).expect("a directory");
    let (code, out, _) = status_cli(&dir, &[]);
    assert_eq!(code, ExitCode::FAILURE);
    assert!(
        out.contains(&format!(
            "pending confirm: {A} by 1800000000 (unix seconds), else back to none\n"
        )),
        "{out}"
    );
    assert!(
        out.ends_with("current link: not a symbolic link (the state has no current release)\nreleases kept: 0\n"),
        "{out}"
    );
    let (_, out, _) = status_cli(&dir, &["--json"]);
    assert!(
        out.starts_with(
            "{\"current_link\":{\"kind\":\"not_a_link\",\"matches_state\":false,\
             \"target\":null}"
        ),
        "{out}"
    );
}

/// Catches a swap whose window has not started (a Linux swap waits for the new
/// release's first start) printed with a deadline that is not there, and the order and
/// the start budget of the swap left out of what a script parses.
///
/// Catches: the `None` deadline printed as `0`.
#[test]
fn status_says_when_the_window_to_confirm_has_not_started() {
    let (root, dir) = host_root("deploy-host-status-window");
    let mut state = full_state();
    let pending = state.pending_confirm.as_mut().expect("pending");
    pending.deadline_wall = None;
    pending.activation = "order-1".to_owned();
    pending.starts_left = Some(3);
    Locked::acquire(&root)
        .expect("lock")
        .write(&state)
        .expect("write");
    let (_, out, _) = status_cli(&dir, &[]);
    assert!(
        out.contains(&format!(
            "pending confirm: {A} once its window starts (its first start), else back to {B}\n"
        )),
        "{out}"
    );
    let (_, out, _) = status_cli(&dir, &["--json"]);
    assert!(
        out.contains(&format!(
            "\"pending_confirm\":{{\"activation\":\"order-1\",\"commit\":\"{A}\",\
             \"deadline_mono_s\":86400,\"previous\":\"{B}\",\"starts_left\":3}}"
        )),
        "{out}"
    );
}

/// Catches a state file that cannot be parsed shown as no state (an operator would
/// swap on a host whose state is lost), and a relative `--root`.
#[test]
fn status_refuses_a_broken_state_and_a_relative_root() {
    let (_root, dir) = host_root("deploy-host-status-refused");
    fs::write(dir.join("var/lib/kbf-installer/state.json"), "{").expect("seed");
    let (code, out, err) = status_cli(&dir, &[]);
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""));
    assert!(
        err.starts_with("REFUSED: ")
            && err.contains("the state is unusable and nothing was changed"),
        "{err}"
    );

    let argv: Vec<OsString> = ["kbf-deploy", "host", "status", "--root", "relative"]
        .into_iter()
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::cli::run(argv, &mut FakeExec::new(0), &|_| None, &mut out, &mut err);
    assert_eq!(code, ExitCode::from(2));
    assert!(String::from_utf8_lossy(&err).contains("not absolute"));
}

/// `kbf-deploy host <args>`: exit, stdout, stderr.
fn host_cli(args: &[&OsStr]) -> (ExitCode, String, String) {
    let argv: Vec<OsString> = [OsStr::new("kbf-deploy"), OsStr::new("host")]
        .into_iter()
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::cli::run(argv, &mut FakeExec::new(0), &|_| None, &mut out, &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (code, text(out), text(err))
}

/// Catches a release the user a service runs as could replace: a binary, unit or
/// directory planned writable by group or others, or with no owner (it would be the
/// running user's). The dry run of `host install` is what the operator reads before
/// `--apply`, so it is pinned whole, with root as the owner of every step.
///
/// Catches: the binary planned 0775, and planned with no owner.
#[test]
fn install_plans_a_release_only_root_can_change() {
    let dir = kbf_host::testkit::scratch("deploy-host-install-cli");
    for sub in ["run", "var/lib", "usr/local"] {
        fs::create_dir_all(dir.join(sub)).expect("seed");
    }
    let from = kbf_host::testkit::scratch("deploy-host-install-cli-src");
    fs::create_dir(from.join("units")).expect("units");
    fs::write(from.join("kbf"), "kbf A").expect("kbf");
    fs::write(from.join("units/kbf.slice"), "[Slice]\n").expect("unit");
    let args = ["install", "--commit", A, "--from"].map(OsStr::new);
    let args = [
        &args[..],
        &[from.as_os_str(), OsStr::new("--root"), dir.as_os_str()],
    ]
    .concat();
    let (code, out, err) = host_cli(&args);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"kbf A"));
    let r = format!("/var/lib/kbf-installer/releases/{A}");
    assert_eq!(
        out,
        format!(
            "release {A}: {r} (5 bytes, sha256 {sha}), 1 unit(s)\n\
             DRY-RUN: mkdir /var/lib/kbf-installer (0755, owner 0:0)\n\
             DRY-RUN: mkdir /var/lib/kbf-installer/releases (0755, owner 0:0)\n\
             DRY-RUN: mkdir {r} (0755, owner 0:0)\n\
             DRY-RUN: mkdir {r}/units (0755, owner 0:0)\n\
             DRY-RUN: write {r}/kbf (0755, owner 0:0, 5 bytes)\n\
             DRY-RUN: write {r}/units/kbf.slice (0644, owner 0:0, 8 bytes)\n\
             dry run: nothing changed. Re-run with --apply.\n\
             links: current none -> {A}, previous none -> none\n\
             DRY-RUN: link /var/lib/kbf-installer/releases/current -> {A}\n\
             dry run: nothing changed. Re-run with --apply.\n"
        )
    );
}

/// Catches `rollback` and `gc` not reaching their verbs from the command line, a refusal
/// that exits 0 or prints nothing, `--apply` read as a dry run (or the reverse), and a
/// commit id the parser takes without the check.
#[test]
fn rollback_and_gc_run_from_the_command_line() {
    let (_root, dir) = host_root("deploy-host-verbs-cli");
    let root = [OsStr::new("--root"), dir.as_os_str()];
    let (code, out, err) = host_cli(&[&[OsStr::new("rollback")], &root[..]].concat());
    assert_eq!((code, out.as_str()), (ExitCode::FAILURE, ""));
    assert_eq!(err, "kbf-deploy: no previous release to roll back to\n");
    let gc = ["gc", "--keep", "2"].map(OsStr::new);
    let (code, out, _) = host_cli(&[&gc[..], &root[..]].concat());
    assert_eq!(
        (code, out.as_str()),
        (
            ExitCode::SUCCESS,
            "keeping: none and the 2 newest others; removing: none\nnothing to do\n"
        )
    );

    let bad = ["install", "--commit", "HEAD", "--from", "/x"].map(OsStr::new);
    let (code, _, err) = host_cli(&bad);
    assert_eq!(code, ExitCode::from(2));
    assert!(err.contains("\"HEAD\" is not a commit id"), "{err}");

    let args = |apply| ApplyArgs {
        apply,
        root: Root::host(),
        platform: None,
    };
    assert_eq!(
        (args(false).mode(), args(true).mode()),
        (Mode::DryRun, Mode::Apply)
    );
}

/// A process table that cannot be read.
struct Blind;

impl Procs for Blind {
    fn exe_of(&self, _: u32) -> std::io::Result<std::path::PathBuf> {
        Err(std::io::Error::other("no process table"))
    }

    fn executables(&self) -> std::io::Result<Vec<std::path::PathBuf>> {
        Err(std::io::Error::other("no process table"))
    }
}

/// Catches a `gc` that removes releases on a host where it cannot tell which ones run: it
/// fails (exit 1), says so on standard error, prints no plan and leaves a stray release in
/// place; with a readable table the same release goes.
///
/// Catches: the in-use error treated as "nothing runs" (the release is removed).
#[test]
fn gc_removes_nothing_where_it_cannot_tell_what_runs() {
    let (layout, dir) = host_root("deploy-host-gc-blind");
    let stray = dir
        .join("var/lib/kbf-installer/releases")
        .join("dddddddddddddddddddddddddddddddddddddddd");
    fs::create_dir_all(&stray).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = gc(&Blind, &layout, 0, Mode::Apply, &mut out, &mut err);
    assert_eq!(code, ExitCode::FAILURE);
    assert!(out.is_empty());
    let err = String::from_utf8(err).unwrap();
    assert!(
        err.contains("cannot tell which releases run: no process table"),
        "{err}"
    );
    assert!(stray.exists());
}

#[path = "kept_tests.rs"]
mod kept;
