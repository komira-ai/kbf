//! The three entries of the program on a Mac, and the unprivileged one that relays.

use std::ffi::OsString;
use std::io::{self, Read};

use super::fx::{Fx, INSTALL, exec};
use crate::mac::{Invocation, MAX_REQUEST_LEN, run};

fn invocation(args: &[&str], root: bool, command: Option<&str>) -> Invocation {
    Invocation {
        args: args.iter().map(OsString::from).collect(),
        root,
        ssh_original_command: command.map(OsString::from),
    }
}

struct Answer {
    code: u8,
    out: String,
    err: String,
}

fn go(fx: &Fx, sim: &super::fx::Sim, inv: &Invocation, stdin: &[u8]) -> Answer {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut input = stdin;
    let code = run(&fx.host, sim, inv, &mut input, &mut out, &mut err);
    Answer {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

const REFUSED: &str = "{\"result\":\"refused\",\"reason\":\"bad_command\"}\n";

/// The ssh login is not root: its request goes to the root process through the one sudo
/// command line, on standard input, and the root process's line and status come back as
/// they are.
#[test]
fn the_unprivileged_process_relays_through_sudo() {
    let fx = Fx::new("mac-relay");
    for (status, line) in [
        (0, "{\"result\":\"query\"}\n"),
        (2, REFUSED),
        (3, "{\"result\":\"failed\",\"reason\":\"busy\"}\n"),
    ] {
        let sim = fx.sim();
        *sim.sudo.borrow_mut() = Some(Ok(crate::runner::Exec {
            code: Some(status),
            signal: None,
            stdout: line.into(),
            stderr: b"detail\n".to_vec(),
        }));
        let answer = go(&fx, &sim, &invocation(&[], false, Some(INSTALL)), b"");
        assert_eq!(
            (
                i32::from(answer.code),
                answer.out.as_str(),
                answer.err.as_str()
            ),
            (status, line, "detail\n")
        );
        let calls = sim.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "/usr/bin/sudo");
        assert_eq!(
            calls[0].args,
            ["-n", fx.host.self_exe.to_str().unwrap(), "--as-root"]
        );
        assert_eq!(calls[0].stdin, INSTALL.as_bytes());
    }
}

/// When sudo does not give the root process's answer (no rule, a password asked for, a
/// kill, a program that cannot start, an empty answer), the failure is the verb's own.
#[test]
fn a_sudo_that_does_not_answer_is_a_failure_of_the_verb() {
    let fx = Fx::new("mac-relay-fail");
    for (command, reason) in [("query", "query_failed"), (INSTALL, "install_failed")] {
        for reply in [
            exec(1, "", "sudo: a password is required"),
            exec(0, "", ""),
            exec(4, "x\n", ""),
            exec(1, "{\"result\":\"query\"}\n", ""),
            Ok(crate::runner::Exec {
                code: None,
                signal: None,
                stdout: vec![],
                stderr: vec![],
            }),
            Err(io::Error::other("cannot start")),
        ] {
            let sim = fx.sim();
            *sim.sudo.borrow_mut() = Some(reply);
            let answer = go(&fx, &sim, &invocation(&[], false, Some(command)), b"");
            assert_eq!(answer.code, 3);
            assert_eq!(
                answer.out,
                format!("{{\"result\":\"failed\",\"reason\":\"{reason}\"}}\n")
            );
            assert!(answer.err.contains("sudo:"), "{}", answer.err);
        }
    }
}

/// A request that is not text, is missing or is long is refused by the unprivileged
/// process, and sudo is not asked.
#[test]
fn a_request_that_cannot_be_passed_on_is_refused_before_sudo() {
    let fx = Fx::new("mac-relay-refuse");
    let sim = fx.sim();
    let long = "q".repeat(MAX_REQUEST_LEN + 1);
    for command in [None, Some(long.as_str())] {
        let answer = go(&fx, &sim, &invocation(&[], false, command), b"");
        assert_eq!((answer.code, answer.out.as_str()), (2, REFUSED));
    }
    use std::os::unix::ffi::OsStringExt;
    let inv = Invocation {
        ssh_original_command: Some(OsString::from_vec(b"query\xff".to_vec())),
        ..invocation(&[], false, None)
    };
    assert_eq!(go(&fx, &sim, &inv, b"").out, REFUSED);
    assert_eq!(sim.calls.borrow().len(), 0);
    // A request of exactly the longest length is passed on.
    *sim.sudo.borrow_mut() = Some(exec(2, REFUSED, ""));
    let exact = "q".repeat(MAX_REQUEST_LEN);
    assert_eq!(
        go(&fx, &sim, &invocation(&[], false, Some(&exact)), b"").code,
        2
    );
}

/// Asserts that a Mac refuses a `restart` request as a bad command with exit status 2, runs
/// no command, and audits it under the verb `restart` with no unit.
#[test]
fn a_mac_refuses_a_restart() {
    let fx = Fx::new("mac-restart");
    let sim = fx.sim();
    let inv = invocation(&[], true, Some("restart frontend=a1"));
    let answer = go(&fx, &sim, &inv, b"");
    assert_eq!((answer.code, answer.out.as_str()), (2, REFUSED));
    assert_eq!(sim.calls.borrow().len(), 0);
    let audit = fx.audit_lines();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["verb"], "restart");
    assert_eq!(audit[0]["reason"], "bad_command");
    assert!(audit[0].get("unit").is_none());
}

/// A process that already is root handles the request itself, with no sudo.
#[test]
fn a_root_process_handles_the_request_itself() {
    let fx = Fx::new("mac-root-direct");
    let sim = fx.sim();
    let answer = go(&fx, &sim, &invocation(&[], true, Some("query")), b"");
    assert_eq!(answer.code, 0);
    assert!(
        answer.out.contains("\"result\":\"query\""),
        "{}",
        answer.out
    );
    assert_eq!(sim.count("/usr/bin/sudo"), 0);
}

/// `--as-root` reads the request from standard input, whole: more than the longest request,
/// or something that is not text, is not one.
#[test]
fn the_sudo_entry_reads_the_request_from_standard_input() {
    let fx = Fx::new("mac-as-root");
    let sim = fx.sim();
    let inv = invocation(&["--as-root"], true, None);
    let answer = go(&fx, &sim, &inv, b"query");
    assert_eq!(answer.code, 0);
    assert!(answer.out.contains("\"result\":\"query\""));
    for bad in [
        &b"query\n"[..],
        b"",
        b"query\xff",
        "q".repeat(MAX_REQUEST_LEN + 1).as_bytes(),
    ] {
        let answer = go(&fx, &sim, &inv, bad);
        assert_eq!((answer.code, answer.out.as_str()), (2, REFUSED), "{bad:?}");
    }
    // A standard input that fails.
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("closed"))
        }
    }
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&fx.host, &sim, &inv, &mut Broken, &mut out, &mut err);
    assert_eq!(
        (code, String::from_utf8(out).unwrap().as_str()),
        (2, REFUSED)
    );
}

/// The root entries are for root: another user is refused, and so is any other word, the
/// request in the arguments, or an apply of a stage that is not this host's.
#[test]
fn the_entries_are_closed() {
    let fx = Fx::new("mac-entries");
    let sim = fx.sim();
    let stage = fx.host.stage().display().to_string();
    for inv in [
        invocation(&["--as-root"], false, None),
        invocation(&["--apply", &stage], false, None),
        invocation(&["--apply", "/tmp/elsewhere"], true, None),
        invocation(&["--apply"], true, None),
        invocation(&["--as-root", "x"], true, None),
        invocation(&["query"], true, Some("query")),
        invocation(&["install", "kbf=1", "from", "stable"], false, None),
        invocation(&["--help"], true, None),
    ] {
        let answer = go(&fx, &sim, &inv, b"query");
        assert_eq!((answer.code, answer.out.as_str()), (2, REFUSED), "{inv:?}");
    }
    assert_eq!(sim.calls.borrow().len(), 0);
}

/// `--apply <stage>` as root runs the apply job body: with no plan in the stage it fails
/// with the plan's status and exits 1.
#[test]
fn the_apply_entry_runs_the_job() {
    let fx = Fx::new("mac-apply-entry");
    let sim = fx.sim();
    super::fx::mkdir(&fx.host.work, 0o711);
    super::fx::mkdir(&fx.host.stage(), 0o700);
    let stage = fx.host.stage().display().to_string();
    let answer = go(
        &fx,
        &sim,
        &invocation(&["--apply", &stage], true, None),
        b"",
    );
    assert_eq!((answer.code, answer.out.as_str()), (1, ""));
    assert_eq!(fx.status(), "failed:plan_invalid\n");
}

/// The program's own streams are the ones used: an entry that is refused writes its line to
/// this process's standard output.
#[test]
fn the_process_streams_carry_the_answer() {
    let fx = Fx::new("mac-std");
    let sim = fx.sim();
    let code = crate::mac::run_std(&fx.host, &sim, &invocation(&["--bogus"], true, None));
    assert_eq!(code, 2);
    assert_eq!(sim.calls.borrow().len(), 0);
}

/// This process's invocation: its arguments, whether it is root, its request.
#[test]
fn the_invocation_of_this_process() {
    let inv = Invocation::from_process();
    assert_eq!(inv.root, rustix::process::geteuid().is_root());
    assert!(!inv.args.contains(&OsString::from("--as-root")));
}

/// The production paths are the kit's.
#[test]
fn the_production_paths() {
    let host = crate::mac::layout::MacHost::production();
    assert_eq!(host.releases().to_str(), Some("/usr/local/kbf/releases"));
    assert_eq!(host.current().to_str(), Some("/usr/local/kbf/current"));
    assert_eq!(host.previous().to_str(), Some("/usr/local/kbf/previous"));
    assert_eq!(
        host.node_conf.to_str(),
        Some("/usr/local/kbf/etc/av2_mac.conf")
    );
    assert_eq!(
        host.daemon_plist.to_str(),
        Some("/Library/LaunchDaemons/com.komira.kbf-daemon.plist")
    );
    assert_eq!(
        host.err_log.to_str(),
        Some("/usr/local/kbf/log/kbf-daemon.err.log")
    );
    assert_eq!(
        host.daemon_config.to_str(),
        Some("/usr/local/etc/kbf/kbf.toml")
    );
    assert_eq!(
        host.allowlist.to_str(),
        Some("/usr/local/etc/kbf/host-install.toml")
    );
    assert_eq!(
        host.self_exe.to_str(),
        Some("/usr/local/libexec/kbf/kbf-host-install")
    );
    assert_eq!(host.gh.to_str(), Some("/usr/local/kbf-installer/trust/gh"));
    assert_eq!(
        host.trusted_root.to_str(),
        Some("/usr/local/kbf-installer/trust/trusted_root.jsonl")
    );
    assert_eq!(host.curl.to_str(), Some("/usr/bin/curl"));
    assert_eq!(
        host.release("x").to_str(),
        Some("/usr/local/kbf/releases/x")
    );
    assert_eq!(
        host.stage().to_str(),
        Some("/var/db/kbf-host-install/stage")
    );
    assert_eq!(
        host.status_file().to_str(),
        Some("/var/db/kbf-host-install/status")
    );
    assert_eq!(
        host.lock_file().to_str(),
        Some("/var/db/kbf-host-install/lock")
    );
    assert_eq!(
        host.apply_plist().to_str(),
        Some("/var/db/kbf-host-install/apply.plist")
    );
    assert_eq!(host.tuning.welcome.as_secs(), 180);
}
