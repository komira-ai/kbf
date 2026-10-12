use std::io;

use super::*;

/// Catches a check that lets through what breaks a unit line or the overlay as it is:
/// a space in an `ExecStart=` word (systemd splits there), a `%` (a systemd
/// specifier), a quote, `$` or a newline; and one that refuses the characters paths,
/// URLs and property sets need.
#[test]
fn carried_takes_only_what_a_unit_or_the_overlay_holds_as_it_is() {
    let fine = "/srv/op/.config/kbf-pilot/pki/server.pem http://192.0.2.20:30900 a=b,c+d@e-f_g";
    for word in fine.split(' ') {
        assert!(carried("w", "unit", word, false).is_ok(), "{word}");
    }
    assert!(carried("w", "overlay", fine, true).is_ok());
    for bad in ["a b", "200%", "a\"b", "a'b", "$HOME", "a\nb", "a\\b", "a;b"] {
        let e = carried("--pki", "unit", bad, false)
            .err()
            .map(|e| e.to_string());
        assert_eq!(
            e,
            Some(format!(
                "--pki holds a character the unit cannot carry: {bad:?}"
            )),
            "{bad:?}"
        );
    }
    assert!(carried("w", "overlay", "100%", true).is_err());
}

/// Catches a failed step or a server that never got ready reported as a refusal
/// (exit 2 tells the operator nothing ran), and a refusal reported as a failure.
#[test]
fn a_failed_step_or_no_start_line_exits_1_and_a_refusal_2() {
    let step = PilotError::Host(kbf_host::HostError::Step {
        index: 0,
        step: "run x".into(),
        source: io::Error::other("planted"),
    });
    assert_eq!(step.exit_code(), 1);
    assert_eq!(PilotError::NotReady { secs: 30 }.exit_code(), 1);
    let refusals = [
        PilotError::IsRoot,
        PilotError::Host(kbf_host::HostError::IsRoot),
        PilotError::NoBind,
    ];
    for e in refusals {
        assert_eq!(e.exit_code(), 2, "{e}");
    }
}

/// Catches a command helper that drops or reorders arguments.
#[test]
fn command_keeps_its_arguments_in_order() {
    let cmd = command(GIT, &["-C", "/srv/wt", "status"]);
    assert_eq!(cmd.program.as_str(), GIT);
    assert_eq!(cmd.args, ["-C", "/srv/wt", "status"]);
    assert!(cmd.env.is_empty());
}

/// Catches a tool that cannot start, or a path that cannot be read, losing what
/// failed: each error names the command or the path.
#[test]
fn a_failed_read_names_its_command_or_path() {
    let mut exec = kbf_host::testkit::FakeExec::new(1000);
    exec.answer = Box::new(|_| Err(io::Error::from(io::ErrorKind::NotFound)));
    let e = query(&exec, &command(ID, &["-un"]))
        .err()
        .map(|e| e.to_string());
    assert_eq!(e.as_deref(), Some("/usr/bin/id -un: entity not found"));
    let unreadable = HostPath::new("/unreadable").expect("path");
    let want = Some("cannot read /unreadable: permission denied".to_owned());
    assert_eq!(
        inspect(&exec, &unreadable).err().map(|e| e.to_string()),
        want
    );
    assert_eq!(stat(&exec, &unreadable).err().map(|e| e.to_string()), want);
}
