//! The exit code of each error: 1 for a step, download or program that failed, 2 for a refusal.

use host_step::ExecError;
use mdm_core::config::MdmConfig;
use mdm_host::MdmError;

/// Catches an error that calls a refusal a failure or the other way round: a script tells "fix
/// the input" (2) from "retry" (1) by it.
#[test]
fn failures_exit_1_and_refusals_exit_2() {
    let step = || ExecError::Step {
        index: 1,
        what: "x".to_owned(),
        why: "y".to_owned(),
    };
    let failed = [
        MdmError::Exec(step()),
        MdmError::Exec(ExecError::Write(std::io::Error::other("pipe"))),
        MdmError::Io(std::io::Error::other("x")),
        MdmError::Rejected {
            tool: "t".to_owned(),
            got: String::new(),
            want: String::new(),
            kept: String::new(),
        },
        MdmError::Download {
            url: String::new(),
            why: String::new(),
        },
        MdmError::Command {
            line: String::new(),
            code: 1,
        },
        MdmError::NoCertificate(String::new()),
        MdmError::NotCertificate(String::new()),
    ];
    for e in failed {
        assert_eq!(e.exit_code(), 1, "{e}");
    }
    let config = MdmConfig::parse("").unwrap_err();
    let refused = [
        MdmError::Config(config),
        MdmError::Exec(ExecError::NotRoot(5)),
        MdmError::NoFile("/x".to_owned()),
        MdmError::FetchAsRoot,
        MdmError::PortHeld(443),
        MdmError::NeedsRoot { verb: "v", euid: 5 },
        MdmError::NoLego("/x".to_owned()),
    ];
    for e in refused {
        assert_eq!(e.exit_code(), 2, "{e}");
    }
}
