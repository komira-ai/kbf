//! `restart_frontend` over ssh, against the fake process runner of the parent module.

use super::*;

const TIMED_OUT: &str = "{\"result\":\"failed\",\"reason\":\"restart_timeout\"}\n";
const BAD_COMMAND: &str = "{\"result\":\"refused\",\"reason\":\"bad_command\"}\n";

/// The line the host prints when it has restarted `unit`.
fn restarted(unit: &str) -> String {
    format!("{{\"result\":\"restarted\",\"unit\":\"{unit}\"}}\n")
}

/// The result of a restart of the instance `a1` that the host answers with `reply`.
fn restart_says(reply: Result<Exited, ExecError>) -> Result<(), PortError> {
    let r = rig();
    r.fake.says(reply);
    r.ssh.restart_frontend(&host(), "a1")
}

/// Asserts that a restart is one call whose remote command is exactly
/// `restart frontend=<name>`, the last argument as for every call, for the plain unit and for
/// an instance; that the host's line for the unit asked for is success; and that the pinned
/// key file is gone afterwards.
#[test]
fn a_restart_is_one_call_with_the_request_as_the_last_argument() {
    for (name, unit) in [
        ("-", "kbf-frontend.service"),
        ("a1", "kbf-frontend@a1.service"),
    ] {
        let r = rig();
        r.fake.says(exited(0, &restarted(unit)));
        assert_eq!(r.ssh.restart_frontend(&host(), name), Ok(()));
        let calls = r.fake.calls();
        assert_eq!(calls.len(), 1);
        let args = strs(&calls[0].spawn.args);
        let want = format!("restart frontend={name}");
        assert_eq!(
            &args[args.len() - 3..],
            ["--", "kbf-update@203.0.113.7", want.as_str()]
        );
        assert_eq!(calls[0].spawn.timeout, Duration::from_secs(600));
        assert_eq!(leftovers(r.dir.path()), 0);
    }
}

/// Asserts that a frontend name the host would refuse is refused here with the fixed line,
/// and starts no process.
#[test]
fn a_bad_frontend_name_starts_no_process() {
    let r = rig();
    let over = "a".repeat(33);
    for name in ["", "A", "a;b", "../x", "a b", "-a", "--", over.as_str()] {
        assert_eq!(
            r.ssh.restart_frontend(&host(), name),
            Err(PortError::other("refused: bad_frontend")),
            "{name:?}"
        );
    }
    assert!(r.fake.calls().is_empty(), "no process");
}

/// Asserts that the host's refusal and failure lines of a restart are read as the fixed
/// errors; that an answer for another unit, an answer of another verb, a line that its exit
/// status disagrees with, output that is not one line, a call that a signal ended and one whose
/// output overflowed its bound are all a bad answer; and that
/// ssh's own failure, a timeout and a program that cannot start are the errors of every call.
#[test]
fn what_the_host_answers_to_a_restart_is_read_strictly() {
    assert_eq!(
        restart_says(exited(3, TIMED_OUT)),
        Err(PortError::other("failed: restart_timeout"))
    );
    assert_eq!(
        restart_says(exited(2, BAD_COMMAND)),
        Err(PortError::other("refused: bad_command"))
    );
    let wrong_unit = restarted("kbf-frontend@b.service");
    let right_unit = restarted("kbf-frontend@a1.service");
    for reply in [
        exited(0, &wrong_unit),
        exited(0, INSTALLED),
        exited(0, TIMED_OUT),
        exited(3, &right_unit),
        exited(1, &right_unit),
        exited(0, "a\nb\n"),
        exited(0, "garbage\n"),
        Ok(Exited {
            code: None,
            stdout: right_unit.as_bytes().to_vec(),
            stdout_overflow: false,
            stderr: Vec::new(),
        }),
        Ok(Exited {
            code: Some(0),
            stdout: right_unit.as_bytes().to_vec(),
            stdout_overflow: true,
            stderr: Vec::new(),
        }),
    ] {
        assert_eq!(restart_says(reply), Err(PortError::other("bad answer")));
    }
    assert_eq!(
        restart_says(ssh_failed("Permission denied (publickey).")),
        Err(PortError::unreachable("auth"))
    );
    assert_eq!(
        restart_says(Err(ExecError::Timeout)),
        Err(PortError::unreachable("timeout"))
    );
    assert_eq!(
        restart_says(Err(ExecError::Spawn)),
        Err(PortError::other("cannot run ssh"))
    );
}
