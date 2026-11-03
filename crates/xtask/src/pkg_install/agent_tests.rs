//! The install test's checks of the host agent against the model of the container: the helper's
//! layout read as whole words, and a unit gated on its program started with the program there but
//! not executable.

use super::tests::{AGENT_PROGRAM, Defect, run_gated};
use super::*;

/// The gate of the agent's unit on its program.
fn agent_gate() -> Vec<Gate> {
    vec![
        format!("kbf-updater-agent.service={AGENT_PROGRAM}")
            .parse()
            .expect("gate"),
    ]
}

/// The problems of a run of the deb with `defect` and the agent's gate, or none.
fn problems_of(defect: Defect) -> Vec<String> {
    match run_gated(Format::Deb, defect, &agent_gate()).1 {
        Ok(_) => Vec::new(),
        Err(Error::Refused(p)) => p,
        Err(other) => panic!("{defect:?}: {other}"),
    }
}

/// The helper's line installs the package only with exactly one layout, `deb`, as a whole word:
/// `--layout debug` and two layouts are refused with the line, and `--layout=deb` is the same as
/// `--layout deb`.
///
/// Catches: a check that takes `--layout debug` for `--layout deb` because one begins with the
/// other, or that passes a line that names a second, conflicting layout.
#[test]
fn the_helper_names_exactly_one_layout_deb_as_a_whole_word() {
    for (defect, layout) in [
        (Defect::HelperLayoutDebug, "--layout debug"),
        (Defect::HelperLayoutTwice, "--layout deb --layout rpm"),
    ] {
        assert_eq!(
            problems_of(defect),
            [format!(
                "kbf-host-local.service does not install the package (exactly one --layout deb): \
                 \"ExecStart=/usr/libexec/kbf/kbf-host-install --serve {layout} --restart-unit \
                 kbf-daemon.service\""
            )],
            "{defect:?}"
        );
    }
    assert_eq!(
        problems_of(Defect::HelperLayoutEquals),
        Vec::<String>::new()
    );
    assert_eq!(
        layouts("x --layout=deb --layout rpm --layout"),
        ["deb", "rpm", ""]
    );
}

/// A unit gated on its program is started three times: without the program (skipped), with the
/// program there and not executable (skipped again), and with it executable (it begins). A unit
/// that starts on the plain file is a named problem.
///
/// Catches: a gate test that makes the file executable at once, so that a unit gated on the file's
/// existence alone (and run on a program that cannot run) passes as gated on its program.
#[test]
fn a_unit_gated_on_its_program_does_not_start_on_a_file_that_is_not_executable() {
    let (world, result) = run_gated(Format::Deb, Defect::None, &agent_gate());
    result.expect("a gated agent passes");
    let installs: Vec<&String> = world
        .log
        .iter()
        .filter(|l| l.contains(AGENT_PROGRAM) && l.contains("install"))
        .collect();
    assert_eq!(
        installs,
        [
            &format!("exec c1 install -D -m 0644 /dev/null {AGENT_PROGRAM}"),
            &format!("exec c1 install -D -m 0755 /dev/null {AGENT_PROGRAM}"),
        ]
    );
    assert_eq!(
        problems_of(Defect::GateIgnoresExecutable),
        [
            format!(
                "kbf-updater-agent.service with {AGENT_PROGRAM} not executable: its start was not \
                 skipped by its condition"
            ),
            format!(
                "kbf-updater-agent.service with {AGENT_PROGRAM} not executable: it began to start"
            ),
        ]
    );
}
